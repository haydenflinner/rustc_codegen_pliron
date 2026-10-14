# Pliron PGO

Profile-guided optimization for the pure-Rust backend. One design covers
native (Cranelift) and wasm (waffle) targets; the profile format is shared.

## What LLVM does, and what we do differently

LLVM PGO: `-fprofile-instr-generate` inserts counter increments in IR,
a C runtime (`profiler_builtins`) writes `.profraw` at exit,
`llvm-profdata` merges to `.profdata`, `-fprofile-use` annotates IR with
`!prof` metadata. Weak points we improve on:

- **Staleness.** LLVM keys counters by index inside a name-hashed
  function; a CFG edit silently misattributes counts (a warning at best).
  Ours keys each function's record by `symbol_hash + cfg_fingerprint`:
  a stale entry simply doesn't match and is ignored — never misapplied.
- **Tooling.** No `llvm-profdata` step: `pliron.prof` is a single
  self-describing, self-merging file. Feed it straight back.
- **Wasm.** LLVM instrumentation PGO on wasm effectively doesn't exist
  (no profile runtime). We own the object format and the linker, so the
  same `.prof` format works on both targets.
- **Consumers.** Profile is a first-class input to our own passes —
  profile-guided inlining, cold-block layout — and eventually the
  e-graph/relational passes (frequency-weighted extraction), which LLVM
  has no equivalent of.

## Profile format

`pliron.prof` (little-endian, self-describing, mergeable by union):

```
magic        u64   "PLPROF01"
nfuncs       u64
repeat nfuncs:
  sym_hash   u64   FxHash of the symbol name
  fingerprint u64  hash of CFG shape at instrument time
  nblocks    u32
  nedges     u32
  counts     u64[nblocks + nedges]   block-entry counts, then edge counts
```

`fingerprint` = hash over `(nblocks, per-block successor-count sequence)`
computed when counters are placed. The use-side build recomputes it on
the same pre-pipeline CFG, so matching is exact; a function whose shape
changed keeps its old record harmlessly un-merged.

Merging: keyed union add on `(sym_hash, fingerprint)`. The runtime merges
on write, so repeated runs accumulate without a merge tool. Multiple
files can be concatenated by `pliron-prof merge`.

## Instrumentation (`PLIRON_PGO=<dir>`)

Runs in `optimize_cgu` right after the always-inline round and before
the main IR pipeline, so counters attribute to *source-level* functions.
The inliner clones counter ops during the profiled build, so hot
executions inside inlined clones still accrue to the source function's
counters — same attribution model as LLVM.

Per CGU, three data objects plus one constructor stub:

- `__pliron_prof_ctrs.<cgu>` — `u64[n]` zeroed, section `pliron_prof_ctrs`
  (writable). Counter `i` is `ctr[i]++`: `load`/`add`/`store` placed at
  the head of the target block, each op marked `st.volatile` so later
  passes (incl. the CLIF pipeline) can't reorder or elide them.
- `__pliron_prof_recs.<cgu>` — serialized func records (sym_hash,
  fingerprint, first counter index, nblocks, nedges), `pliron_prof_recs`.
- `__pliron_prof_hdr.<cgu>` — `{magic, recs ptr, recs len, ctrs ptr,
  ctrs len, cgu hash}` with relocations to the other two, `pliron_prof`.
- `__pliron_prof_ctor.<cgu>` — three lines of rsasm
  (`lea hdr; jmp __pliron_prof_register`) plus an `.init_array` slot,
  emitted through `st.asm`/objmerge so it gets a real `SHT_INIT_ARRAY`
  section. Undefined `__pliron_prof_register` pulls the runtime out of
  `libpliron_prof.a`; link without it and you get a clean
  "undefined symbol" naming the missing runtime.

Counter placement: one entry counter per block, plus an edge counter on
each edge that block counts alone can't disambiguate (successor with
more than one predecessor — emitted in a split edge block). Block counts
give hot/cold + callsite frequencies; edge counts give branch polarity
and loop trip counts.

## Runtime (`tools/pliron-prof`)

Pure Rust, `std`, ~200 lines, no deps beyond `libc::atexit`.

- `__pliron_prof_register(hdr)` — appends the CGU's `{recs, ctrs}` range
  to a static list and installs `atexit(flush)` once.
- `flush` — reads `PLIRON_PROFILE_FILE` (default
  `pliron-<pid>.prof`), merges counts into matching `(sym, fingerprint)`
  records, writes back via temp+rename.
- `pliron-prof` bin — `merge`/`dump` for host-side inspection and for
  profiles pulled out of wasm guests.

The gen build links it via `-Clink-arg=<path>/libpliron_prof.a`
(test.sh and compat scripts wire this automatically).

## Consumption (`PLIRON_PGO_USE=<file>`)

Loaded in `optimize_cgu` before the pipeline; each function's record is
matched by `(sym_hash, fingerprint)` and stored in `st.prof`. Per-block
counts land in `st.prof_bcount` and per-successor edge counts in
`st.prof_edges`; the inliner's clone path scales both into clones.

Consumers, roughly in win-per-line order:

1. **Cold layout** (lower.rs `cold_blocks`): a reachable block with a
   recorded count of 0 is cold — joins the existing
   `#[cold]`/`expect`/unreachable rules and feeds `set_cold_block`, so
   cold code moves out of line on native.
2. **Inlining** (inline.rs): callsite hotness scales the score threshold
   — hot sites get a multiplier, zero-count sites are treated like
   `#[cold]` callers. This is the single biggest PGO win in LLVM too.
3. **Branch polarity / fallthrough ordering**: edge counts bias the
   successor order used for block layout so hot edges fall through
   (fewer taken branches), and seed `st.expect` on `cond_br`s so all
   existing expect-consumers behave as if the user annotated them.

Phase 2 consumers (design only for now):

- **Unroll / loop passes**: edge counts give real trip-count histograms.
- **Switch lowering**: value profiles order jump-table lookups by hit
  frequency; indirect-call target profiles enable checked speculative
  devirtualization (the `dv` hooks inline.rs already has).
- **Frequency-weighted e-graph extraction**: cost × block frequency as
  the extraction cost model — pick the variant that's fastest on the
  hot path, not uniformly smallest.
- **Cross-crate profiles**: counts stored in rlib metadata so consumers
  inline callees with real frequencies without a ThinLTO-style import.
- **Sampling mode**: a PC→block map so periodic PC samples approximate
  counts with zero instrumentation overhead.

## Wasm

Instrumentation is target-independent — the same `pgo.rs` ops lower to
waffle like anything else — so gen mode emits identical globals. What
differs is how counts leave the guest:

- The counter arrays and records land in linear memory via the normal
  data-segment path; `pliron-wasm-ld` roots every `__pliron_prof_*`
  data object (they'd otherwise be GC'd), and synthesizes a
  `[count][u32 addrs...]` table of all `__pliron_prof_hdr.*` objects,
  exported as the i32 global `__pliron_prof_hdrs`.
- No wasm-side runtime at all: the host (node/JS) reads the table after
  the run, walks each header, and writes the same `.prof` format —
  `tests/wasm/pgo_extract.mjs` is the reference extractor. The `ctrs`
  field is a 4-byte linear-address reloc in an i64 slot (upper bytes
  stay zero), so the `{u64 magic, u64 ctrs, u64 nctrs, u64 recs_len}`
  wire layout is identical to native.
- This makes browser-native JIT feedback ("which functions did the user
  actually run") trivially collectible — something LLVM instrumentation
  PGO cannot do on wasm at all.
- `.init_array` ctor objects still work on wasm (`__wasm_call_ctors`
  exists), so a wasm-side flush function can be added later for
  long-running guests that want periodic snapshotted profiles.

## Knobs

| env | effect |
| --- | --- |
| `PLIRON_PGO=<dir>` | instrument build; profile written to `<dir>/pliron-<pid>.prof` (or `$PLIRON_PROFILE_FILE`) |
| `PLIRON_PGO_USE=<file>` | consume profile before the pipeline |
| `PLIRON_PGO_COUNTERS=block` | block counters only (edge counters are additive); default is block+edge |
| `PLIRON_PGO_ATOMIC=1` | `atomicrmw` counter ops instead of load/add/store (races lose counts by default, same as LLVM) |

## Non-goals (for now)

- `-Cprofile-generate`/`-Cprofile-use` flag compatibility and
  `llvm-profdata` interop (we can read `.profraw`/`profdata` later if it
  matters).
- Sampling/coverage mode: emit a `pliron.blk` PC→block map per object
  and attribute `perf`/SIGPROF samples to blocks (AutoFDO-style,
  zero-overhead collection).
- Function-level `#[pgo]` hot/cold section splitting.

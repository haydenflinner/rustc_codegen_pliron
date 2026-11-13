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

After the function records an optional trailer (`"PLICALL2"`) holds the
indirect-call sites of the functions that have any (see
"Devirtualization"); a profile without it is read as having no sites.

Merging: keyed union add on `(sym_hash, fingerprint)`. The runtime merges
on write, so repeated runs accumulate without a merge tool. Multiple
files can be concatenated by `pliron-prof merge`.

## Instrumentation (`PLIRON_PGO=<dir>`)

Runs in `finish_pre` (`src/cgupool.rs`), before IR-opt (`iropt_head`)
and, at `-O0`, after the always-inline round, so counters attribute to
*source-level* functions. The instrumentation itself is
`pliron-ir-opt/src/pgo.rs`.
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
  (`lea hdr; jmp __pliron_prof_register`) plus an init slot, emitted
  through `st.asm`/objmerge: `.init_array` on ELF,
  `__DATA,__mod_init_func` on Mach-O (weak + private_extern; sectnames
  are capped at 16 bytes, so the names blob lands in `pliron_pnames`),
  `.CRT$XCU` on COFF (the `__initterm` band the MSVC CRT calls).
  `__pliron_prof_register` is declared as a normal import so the splice
  resolves it like any other callee — undefined, it pulls the runtime
  out of `libpliron_prof.a`; link without it and you get a clean
  "undefined symbol" naming the missing runtime.

Counter placement: one entry counter per block, plus an edge counter on
each edge that block counts alone can't disambiguate (successor with
more than one predecessor — emitted in a split edge block). Block counts
give hot/cold + callsite frequencies; edge counts give branch polarity
and loop trip counts.

## Runtime (`tools/pliron-prof`)

The runtime is `tools/pliron-prof-rt` (pure Rust, `no_std`, libc
externs only); `tools/pliron-prof` is the host-side CLI. It must not link std: the archive is pulled
into links that already carry a Rust std (rustc itself, proc-macro
.so's) and bundled std objects collide on `__rdl_*`/`__rust_*` symbols
— the same reason it can't reference `core`/`compiler_builtins`
members, so the code is raw-pointer only (no indexing, no allocation).

- `__pliron_prof_register(hdr)` — appends the CGU header pointer to a
  static table and installs `atexit(flush)` once.
- `flush` — writes a raw dump (`PLRAW01`: per-CGU `len | hdr | recs |
  ctrs`) to `PLIRON_PROFILE_FILE` (default `pliron-<pid>.prof`).
  Merging lives in the CLI so the runtime never allocates.
- `pliron-prof` bin — `merge`/`dump` fold raw dumps (and `.prof`
  files) into a `pliron.prof`, for host-side inspection and for
  profiles pulled out of wasm guests.

The gen build links it via `-Clink-arg=<path>/libpliron_prof_rt.a`
(test.sh and compat scripts wire this automatically); cc-wild thins
the archive to the runtime object before linking it under
`PLIRON_PGO`.

## Consumption (`PLIRON_PGO_USE=<file>`)

Loaded in `finish_pre` (`src/cgupool.rs`), before IR-opt
(`pgo::load`); each function's record is matched by
`(sym_hash, fingerprint)` and stored in `st.prof`. Per-block counts land
in `st.prof_bcount` and per-successor edge counts in `st.prof_edges`;
the inliner's clone path scales both into clones by callsite count /
callee entry count. Lowering copies block counts onto CLIF blocks
(`Pipe::prof`) for the CLIF passes.

Consumers:

1. **Cold layout** (`lower.rs` `cold_blocks`): a reachable block with a
   recorded count of 0 is cold — joins the existing
   `#[cold]`/`expect`/unreachable rules and feeds `set_cold_block`, so
   cold code moves out of line on native.
2. **Inlining** (`pliron-ir-opt/src/inline.rs`): a callsite whose block
   count is 0 is rejected outright (unless the callee is
   `#[inline(always)]`); a callsite with count >= 4 adds twice the base
   limit to its threshold.
3. **Branch expectations** (`pgo::load`): a two-way `cond_br` with at
   least 8 samples and a 4:1 or stronger bias seeds `st.expect`, so the
   existing expect-consumers behave as if the user annotated it.
4. **Loops** (`pliron-clif-opt`): `unroll` skips loops whose header
   count is 0; `punroll` estimates the trip count from header vs entry
   counts to pick its width; `loopvec` also reads the counts.
   `PLIRON_PGO_LOOP=0` hides the profile from these passes.
5. **Switch lowering** (`brchain.rs`, under `PLIRON_JT_SHAPE`, default
   on): chain tests are ordered hottest first and a dominant case is
   peeled ahead of the table (`history/2026-10-27-jump-tables.md`).

Possible later consumers (design only):

- **Value and indirect-call profiles**: order jump-table lookups by hit
  frequency; checked speculative devirtualization (the `dv` hooks
  `inline.rs` already has).
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

## The profile bundle

With `PLIRON_PGO_USE` set, these are on unless `PLIRON_PGO_BUNDLE=0`
(each also has its own knob, which wins):

| knob | bundle value | what |
| --- | --- | --- |
| `PLIRON_PGO_HOT` | 13 | a call site whose count reaches the profile's hot count (the blocks carrying 99% of all counts, LLVM's `ProfileSummary` rule) gets 13x the inline limit and skips `loopy`/`fanin`/growth |
| `PLIRON_PGO_XCGU` | 4000 | a callee entered at least hot-count times is copyable across codegen units up to 4000 MIR statements (default limit 50), so the hot inliner has its body |
| `PLIRON_XCGU_CAP` | 100000 | no per-CGU bound on copies |
| `PLIRON_MEMFAST_SMALL` | hotloop | runtime-size copies handle sizes below 16 bytes inline in hot blocks, and in loops the profile saw run (without a profile the default is `loop`: copies on a CFG cycle; `0` off, `1` everywhere, `hot` hot blocks only) |
| `PLIRON_PGO_HEAVY` | 256 | a callee that runs more than 256 blocks per call in the profile gets neither the hot nor the warm call-site bonus: there is no call overhead worth buying back (regex `follow_epsilons` into `next_state`: -3.5% instructions) |
| `PLIRON_PGO_FREQ` | 1 | block placement uses measured edge probabilities (bit 2, spill weights, measured worse and stays off) |

Measured with `bench/heldout/run.py --pgo` (2026-11-12, 7 rounds):
pliron-pgo/pliron 0.974 [0.952, 0.995] over eight programs —
pulldown-cmark 0.885, rust-html-rewriter 0.942, the rest within noise
of 1. LLVM's own profile: 0.938. `docs/history/2026-11-12-pgo-baseline.md`
has the path there, including what LLVM's profile buys (block
placement first, memcpy size specialization second).

Tools: `bench/heldout/ab.py` (cached A/B with retired instructions and
cycles; LLVM sides take extra flags, so its passes can be ablated),
`PLIRON_LAYOUT_STATS=1` + `harness/layout_score.py` (expected taken
jumps and branches of the emitted order under the measured
frequencies), `harness/pgofuzz.py` (differential fuzz of the whole
generate/run/use loop).

## Field order

`repr(Rust)` structs have no guaranteed field order; rustc picks one to
minimise padding and LLVM never sees the choice. We own the layout
query, so the profile can pick it (`src/field_order.rs`, both knobs
opt-in):

```
# 1. profiled build that also records field heat (appends, one crate
#    after another; start from an empty file)
PLIRON_PGO_USE=pliron.prof PLIRON_FIELD_HEAT=$PWD/fields.heat cargo build --release
harness/field_order.py fields.heat        # per-type ranking, for a human
# 2. rebuild *everything* (fresh CARGO_TARGET_DIR: cargo does not track
#    PLIRON_* variables) with the order applied
PLIRON_PGO_USE=pliron.prof PLIRON_FIELD_ORDER=$PWD/fields.heat cargo build --release
```

- **Heat** is one line per `(struct, field)`:
  `<crate>::<path>\t<field>\t<weight>`; duplicates are summed. It comes
  from a walk over the MIR of every codegen'd instance (the builder only
  sees byte offsets): each field projection on a struct counts once,
  times the profile count of its MIR block. Without `PLIRON_PGO_USE` the
  weight is `8^loop depth`. The file can also be written by hand.
- **Order** wraps `layout_of`. Inside a plain `repr(Rust)` struct (no
  `C`/`packed`/`align`/`transparent`/`simd`; no enums or unions) the
  non-zero-sized fields are taken one alignment class at a time. When a
  class tiles one contiguous byte span in rustc's layout, the span is
  re-packed hottest first (ties keep rustc's order): all its fields have
  the same alignment `a` and sizes that are multiples of `a`, so any
  order fills the span exactly, and nothing outside it moves — size,
  alignment and padding cannot change. A class that is not contiguous
  (rustc put a niche field at an end, or interleaved classes) only swaps
  fields of equal size. Only sized in-memory aggregates
  (`BackendRepr::Memory`) are touched — `Scalar`/`ScalarPair` structs
  encode field positions in the repr — and not structs whose last field
  may be unsized (unsizing needs the prefix fixed). The largest niche
  moves with its field. `PLIRON_FIELD_ORDER_LOG=<file>` appends one line
  per permuted layout (compiling crate, struct, instantiation, fields
  moved).
- **Which types**: those named in the file whose defining crate is the
  crate being compiled or a dependency outside the sysroot. The rule
  looks only at the type's definition and the file, so every crate
  computes the same layout for it. `core`/`alloc`/`std`/`proc_macro`/
  `test` and everything else loaded from the sysroot stay stock.
- **Soundness and its limits**: code may not rely on `repr(Rust)` field
  order, and `offset_of!`, derives, const evaluation and debuginfo all
  read the same `layout_of`. What breaks it is mixing: every non-sysroot
  crate in the link (and every rlib/rmeta it reads) must be built by
  this backend with the same file. Objects built without the knob, with
  a different file, or by stock rustc (`harness/crates.py --deps stock`)
  disagree on offsets — undefined behaviour, not a link error. Two
  crates with the same name share keys; that is consistent, only
  coarser.
- **Check**: `tests/field_order/run.sh` builds one program plain, with
  heat, and under three order files, and diffs sizes, alignments and
  values.
- **Profile path** (2026-11-13): the full loop (`PLIRON_PGO` build, run,
  `pliron-prof merge`, `PLIRON_PGO_USE` + `PLIRON_FIELD_HEAT`, then
  `PLIRON_PGO_USE` + `PLIRON_FIELD_ORDER`) runs on the cache benchmark
  and three held-out programs. Weights are measured counts
  (`PLIRON_TRACE=pliron::field_order` prints, per CGU, how many
  `(block, field)` pairs found a count: pulldown-cmark 913 counted, 607
  count 0, 287 in functions the profile does not have, 0 without a
  backend block). Reordering does not stale the profile: the same
  functions match with and without the order file (pulldown-cmark
  2144, rust-html-rewriter 8196, rust-protobuf 1804; 0 stale).
- **What it buys** (`tests/field_order/cache.sh`, Apple M3 Max, 128-byte
  lines; `Particle` 160 bytes and `Node` 192 bytes with four hot fields
  spread over two lines, ns per element, plain -> reordered; stock
  rustc/LLVM equals plain within noise, it has the same layout):

  | elements | gather (shuffled, independent) | walk (shuffled, linked) | sweep (sequential) |
  | --- | --- | --- | --- |
  | 20k (fits L2) | 2.58 -> 1.07 | 11.2 -> 9.5 | 1.42 -> 1.05 |
  | 100k | 4.90 -> 2.98 | noise | 2.31 -> 5.87 |
  | 1M (160-190 MB) | 14.3 -> 9.4 | 149 -> 136 | 2.59 -> 7.12 |

  Random access to big structs gains 35-60% at every size; a linked
  walk gains 10-15% in cache and little beyond it (its misses already
  overlap inside one node; the chain is latency-bound). The sequential
  sweep is the warning: 25% faster in cache, **2.5x slower** once the
  array leaves the cache (that table is every listed struct reordered,
  `PLIRON_FIELD_ORDER_SEQ=1`).
- **Why the sweep loses** (`tests/field_order/sweep.rs`, layout only, no
  backend involved; 128 MB arrays, ns/element, best of 21). With the hot
  words spread out the sweep touches every 64-byte line in order; packed
  at the front it touches one line per element and skips the rest:

  | stride | lines touched, packed | scattered | packed | packed + prefetch 32 ahead | packed + read every line |
  | --- | --- | --- | --- | --- | --- |
  | 64 | 100% | 1.2 | 1.2 | 1.5 | 1.2 |
  | 128 | 50% | 2.1 | 4.6 | 4.4 | 2.1 |
  | 160 | 40% | 2.4 | 6.7 | 4.5 | 2.7 |
  | 192 | 33% | 2.7 | 5.3 | 4.0 | 3.0 |
  | 256 | 25% | 3.3 | 5.6 | 4.2 | 3.9 |
  | 320 | 20% | 3.8 | 6.4 | 3.9 | 4.7 |

  The loss starts exactly where the stride first skips a 64-byte line
  (128), not before, and reading one cold word from every skipped line
  gives the speed back: this core streams consecutive lines at about
  1.1 ns each, and stops streaming when lines are skipped, leaving one
  demand miss per element (4.5-6.5 ns whatever the stride). A software
  prefetch per element helps only partly (about 4.2 ns from 32 elements
  ahead on, no better at 64 or 128): it is still one line fill per
  element, not a stream. Cache-miss counters were not available
  headless (`xctrace`'s CPU Counters template records cycles and
  pipeline bottleneck classes only).
- **Sweep rule** (default): heat also records, per struct, how much of
  it is in accesses whose address *steps* (`@seq`: the pointer or index
  is on an arithmetic recurrence in the MIR — `for p in slice`,
  `v[i]` with `i += 1`) and how much comes *out of memory* (`@rand`:
  `v[order[k]]`, `node.next`); the rest (through a parameter, a stack
  local) is unknown. A struct wider than 64 bytes whose heat is more
  than half sequential keeps rustc's order. On the cache benchmark
  (`Particle` swept, an identical `Body` gathered, `Node` walked; 1M
  elements, loaded machine): sweep 2.6-3.6 plain, 2.6-3.2 with the
  rule, 7.7-8.3 forced; gather 13.5-17 -> 9.8-12.5; in cache gather
  2.82 -> 1.45. The rule gives up the 20% the sweep would gain in
  cache. It sees only what MIR inlining exposes: a sweep whose body is
  a method taking `&mut self` is "unknown" and the struct is reordered.
  One layout per type: a struct that is both swept and gathered follows
  whichever has more heat.
- **Held-out programs**, profile-weighted heat, `ab.py`, cycles
  reordered/plain: pulldown-cmark 1.014 (5 rounds, 8 structs),
  rust-html-rewriter 1.006 (15 rounds, 44 structs), rust-protobuf 0.989
  (15 rounds, 2 structs), on a loaded machine (noise about 2%). No
  signal: their hot structs are small and their data sits in cache. On
  the cache benchmark the effect needs structs wider than one line and
  either random access or a working set that fits in cache.
- **Not done**: co-access. Heat ranks fields one by one; fields read in
  the same hot block should share a line when the hot set is wider than
  one. Recording pairs per MIR block is straightforward but nothing
  measured here would show it.

## Devirtualization

Indirect-call promotion from a value profile, on by default in both
profile builds (`PLIRON_DEVIRT=0` turns it off; use the same setting
for the instrumented and the profile-use build, because it also decides
which symbols are promoted to external).

- **Sites.** Every virtual call (`dyn Trait` methods, `dyn Fn*`, virtual
  drop) is a site; `PLIRON_DEVIRT_FNPTR=1` adds calls through fn
  pointers. Both builds emit the plain indirect call, so the CFG
  fingerprint is the same with and without the feature. A function's
  sites are numbered by position (`pliron_ir_opt::devirt::sites`).
- **Instrumented build.** `__pliron_prof_icall(site, target)` runs
  before each site. A site is 4 `(address, count)` slots, first come
  first kept, plus a count for everything else. The CGU header (magic
  `PLPROF02`) is followed by `{crate hash, cgu hash, sites, nsites, fns,
  nfns, names, names_len}`; `fns` is `{symbol hash, &fn, flags}` for
  every body in the CGU. The function record's spare word holds its
  number of sites.
- **Merge.** The raw dump carries those tables; `pliron-prof merge`
  maps each recorded address to a symbol through the tables of all CGUs
  of the process and appends the sites to the `.prof` as a trailer
  (`PLICALL1`): per function, per site, the "other" count and
  `(count, crate hash, cgu hash, flags, symbol name)` per target.
  Addresses no table has (std, C) count as "other".
- **Profile-use build.** After `pgo::load`,
  `pliron_ir_opt::devirt::promote` turns a site into
  `if fnptr == &target { target(args) } else ... else { fnptr(args) }`
  for at most `PLIRON_DEVIRT_MAX` (2) targets with at least
  `PLIRON_DEVIRT_MIN_SHARE` (10) percent of its calls. The indirect
  call always stays, so a wrong profile costs a compare.
- **Which targets.** One whose body is in the CGU and whose profiled
  address came from that CGU; a symbol of the crate that other CGUs
  can link; a symbol of a dependency, verified in this build — a crate
  linked here (a binary) reads the dependency rlib's symbol table, an
  rlib crate requires the dependency's crate hash to equal the profiled
  one. `PLIRON_DEVIRT_DOWNSTREAM=1` also accepts targets in crates that
  depend on this one — the object then needs them at link time, and
  nothing checks the profile against those crates.
  `PLIRON_DEVIRT_LINKED=0` keeps only targets with a body in the CGU.
- **Promotion.** For a target to have a linkable name, vtable slot
  instances that are `Internal` in one CGU are defined hidden-external
  (`devirt::promotable`): all of them in the instrumented build, in the
  profile-use build those the profile names from outside their CGU.

`PLIRON_REMARKS` has one `devirt` line per site that ran: targets kept,
shares, and why a hot target was not used. Numbers:
`docs/history/2026-11-13-six-features.md`, section 3. Without a
profile, `PLIRON_DEVIRT=1` guards sites with at most two possible
targets among the vtables the crate instantiates (`src/devirt.rs`).

## Knobs

| env | effect |
| --- | --- |
| `PLIRON_FIELD_HEAT=<file>` | append per-field access weights for the crate (see Field order) |
| `PLIRON_FIELD_ORDER=<file>` | hot fields first inside each alignment class of listed `repr(Rust)` structs (see Field order); `PLIRON_FIELD_ORDER_LOG=<file>` lists what was permuted or skipped; `PLIRON_FIELD_ORDER_SEQ=1` also reorders swept structs |
| `PLIRON_PGO=<dir>` | instrument build; profile written to `<dir>/pliron-<pid>.prof` (or `$PLIRON_PROFILE_FILE`) |
| `PLIRON_PGO_OPT=1` | optimize the instrumented build too (default: it is built unoptimized — the counters sit on the pre-optimization CFG, the profile is the same, the build is about a third faster and the profiling run about 2x slower) |
| `PLIRON_PGO_USE=<file>` | consume profile before the pipeline |
| `PLIRON_PGO_COUNTERS=block` | block counters only (edge counters are additive); default is block+edge |
| `PLIRON_PGO_ATOMIC=1` | `atomicrmw` counter ops instead of load/add/store (races lose counts by default, same as LLVM) |
| `PLIRON_PGO_LOOP=0` | the CLIF loop passes ignore the profile (ablation) |
| `PLIRON_PGO_EXPECT=0` / `PLIRON_PGO_INLINE=0` / `PLIRON_PGO_COLD=0` | ablations: no branch expectations from the profile / the inliner ignores call-site counts / zero-count blocks are not marked cold |
| `PLIRON_PGO_BUNDLE=0` | the bundle above stays off |

## Non-goals (for now)

- `-Cprofile-generate`/`-Cprofile-use` flag compatibility and
  `llvm-profdata` interop (we can read `.profraw`/`profdata` later if it
  matters).
- Sampling/coverage mode: emit a `pliron.blk` PC→block map per object
  and attribute `perf`/SIGPROF samples to blocks (AutoFDO-style,
  zero-overhead collection).
- Function-level `#[pgo]` hot/cold section splitting.

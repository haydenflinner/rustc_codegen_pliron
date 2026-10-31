# Determinism

The backend's output (objects, the embedded LTO payload, emitted IR/asm)
must be a pure function of its inputs. That is what makes caching sound:
`stepcache.rs` today, a query-based build system later. Each rule below
says what must hold, why, where it is enforced and how it is checked.

## The inputs

Output may depend only on:

1. **The crate's MIR and rustc's session options** (opt level, CGU count,
   target, `-C` flags) — what rustc hands every backend.
2. **The backend's own source** (`harness/src-rev.sh`; `PLIRON_VERSION=1`
   prints it).
3. **`PLIRON_*` configuration** (`pliron_cfg::config`, read once per
   process).

Hazards on the input side:

- Cargo tracks neither the backend dylib nor `PLIRON_*` variables. A
  rebuild under a new flag can silently reuse objects built without it.
  `harness/crates.py` wipes its pliron target dir when the dylib's stamp
  changes, and experiments must clear fingerprints after changing env
  (`drvprobe.sh`). A query system must make both part of every key.
- `stepcache` already does this per step: a key covers (step, backend
  build id, input function state, the `PLIRON_*` variables the step
  *actually read*), and a variant also records the steps that ran, so a
  gate flip that changes the step list misses. This read-set keying is the
  model to copy: a query's key is its inputs plus the configuration it
  observed, not the whole environment.

## Rules

### R1. Scheduling never decides results

Workers finish CGUs in any order. Anything one CGU learns from others
must not depend on that order.

- Cross-CGU facts (`state.rs`, `GlobalFacts`) are stored per **participant
  ordinal**, the submission order on rustc's main thread. A CGU adopts
  exactly the ordinals below its own (or, under `PLIRON_XNEED`, the owners
  of the symbols it queries, all below it), and stage B waits until they
  are published (`cgupool.rs`, `parked`/`covered`). It never sees
  "whatever finished first".
- Under `PLIRON_XFACTS_ALL` the barrier is rustc's thin-local LTO step,
  which runs after every module's `optimize`. Every CGU then adopts from
  all of them, which is the same set however the workers ran.

Checked by `harness/detcheck.py` (`inflight1`, `serial`), and by the
`inflight` cap sweep below.

### R2. Execution strategy is invisible

The same inputs must give the same bytes whether CGUs run in the pool or
serially (`PLIRON_CGUPAR=0`), and whether functions within a CGU lower in
parallel or not (`PLIRON_PAR=1`). A cache entry produced one way will be
replayed the other way.

**Known exception:** `PLIRON_XFACTS_ALL`. Two-phase facts exist only in the
pool (`two_phase` requires `cgupool::enabled`), and the serial path compiles
as if the flag were off. On regex-syntax with every flag, `serial` differs
and every other variant matches. Before a query system relies on it, either
make the serial path hold for the barrier too, or put the strategy in the
key. `detcheck.py` reports it as `expected`, not as a failure.

### R3. Throttles change timing, never bytes

Thread counts, `PLIRON_PAR=N`, `PLIRON_CGU_INFLIGHT=N` (the in-flight CGU
cap) and memory knobs only reorder work. With the cap unset, 8 and 1, all 16
regex-syntax objects are byte-identical (`DEADFREE` on), as they are with
`PLIRON_PAR=1`.

The memory knobs of 2026-10-27 (docs/history/2026-10-27-memory2.md) are in
this class: `PLIRON_LOWERFREE` frees a CGU's IR earlier, `PLIRON_JEARENA`
changes which jemalloc arena holds an allocation and when freed pages go
back to the OS, and `PLIRON_CGU_ACAP=N` delays the start of stage A. None
may change a byte. `test.sh lowerfree` compares every CGU object of
tests/std built with `PLIRON_LOWERFREE=0 PLIRON_JEARENA=0` against the
default build; the `lowerfree-wasm` stanza does the same for the wasm
bench kernels (run it by name: the gate's stanza scan does not match the
hyphen, so neither the gate nor `quick.py` runs it). `detcheck.py`'s `acap2`
variant checks the CGU cap. Its `memfree` variant sets
`PLIRON_LOWERFREE=1 PLIRON_JEARENA=1`, which are now the defaults
(`pliron-cfg` `DEFAULT_ON`), so it compares the default build with itself
and checks nothing.

### R4. No output order from unordered iteration

- `std::collections::HashMap`/`HashSet` use a randomly seeded hasher, so
  never iterate one into output. Audit (2026-10-26): five uses, all
  membership-only (`types.rs` `WIDE_F32`, `scalarize.rs`'s `ok`/`wide`,
  `lower.rs`'s DFS `seen`).
- `FxHashMap` is unseeded, so its order is a function of keys and insertion
  history. That is deterministic for the same inputs and config, but it
  changes across configs and across allocation histories. Arena `Ptr`
  ordinals and `Value` uids are allocation-history dependent, and
  `PLIRON_DEADFREE` reuses freed slots. Prefer sorted output,
  `BTreeMap`, rustc's `items_in_deterministic_order`, or source order.
- When hashing a map into a cache key, hash entries separately and sort
  (`stepcache::seq_hash`). `cranelift`'s `params` map serializes in
  per-process-random order, so `stepcache` walks it instead.

Suggested guard: `clippy.toml` `disallowed-types` for
`std::collections::HashMap`/`HashSet` (use `rustc_hash::Fx*`; membership-only
sets can carry an `#[allow]`).

### R5. No wall-clock decisions

Budgets are counted in work (ops, rounds, `PLIRON_INLINE_CALLER_MAX`), never
time. Audit: `Instant::elapsed()` decides nothing outside tests. The two
`clifpeep` sites gate a slow-path log line, and `indvars`/`clifpeep`'s
`< 5 s` are test asserts. A time-based cutoff would make a loaded machine
emit different code.

### R6. No addresses

Never key, order or hash output by real memory addresses (`ptr as usize`,
`Rc::as_ptr`): ASLR and allocator state change them on every run. Arena
`Ptr`s are indices, so they don't have this problem, but see R4.

### R7. Per-crate state starts fresh

Process-global state is reset where a crate's codegen begins:
`cgupool::begin` (the job store), `state::reset_facts` (facts and
ordinals), `types::set_wide_f32` (per compile). A new global cache must be
reset there too, or keyed by crate. A query system that keeps one process
for many crates relies on this.

### R8. Diagnostics only observe

`PLIRON_*_INFO`, `PLIRON_TRACE*`, `PLIRON_MEM_MARK`, `PLIRON_FRAME_INFO`,
`PLIRON_LT_PROBE` and dumps may print, never change the IR. A diagnostic
that has to run analysis (`PLIRON_MEM_DEADPROBE` calls `dead_fns`) says
so, and is not used in a measured or cached build.

### R9. Deterministic numbering where order is observable

`PLIRON_OPT_BISECT` numbers pass applications globally, so it forces
the serial paths (`cgupool::enabled`, `lower.rs` `nthreads`). Any global
counter whose value reaches output must do the same.

## Checks

- `harness/run.py --suite determinism` (`run.py` tier 0; not part of
  `harness/gate.py`): each smoke program
  built twice at one CGU must be byte-identical.
- `harness/detcheck.py`: each input built per configuration (default, or
  `--config NAME="PLIRON_X=1 ..."`) as a reference, then once per variant
  that must not matter (`again`, `par1`, `inflight1`, `serial`).
  Objects must be byte-identical. A mismatch lists, per object, the
  functions that changed (disassembly with relocations, addresses stripped)
  and those present on one side only. `--self-test` sets
  `PLIRON_DEADFREE=1` and expects a difference; `DEADFREE` is now on by
  default, so the self-test no longer sees one and needs a different
  change.
- `stepcache`'s contract is its own check: a hit replays bytes produced
  from the same `(key, reads, ran)`.

## Toward a query system

The pipeline already decomposes into queries with explicit dependencies:

| query | inputs |
|---|---|
| `define(cgu)` | MIR of the CGU's items, session options, config |
| `stage_a(cgu)` | `define(cgu)`; publishes `facts(cgu)` |
| `stage_b(cgu)` | `stage_a(cgu)`, `facts(o)` for each owner ordinal `o` it adopts (all ordinals under `XFACTS_ALL`) |
| `object(cgu)` | `stage_b(cgu)` |
| per function: each pipeline step | `stepcache` key |

R1–R3 make each query's output a function of these inputs alone, R4–R6
make it stable across processes, and R7 across crates in one process. What
is still missing: a key for `define` that rustc's own query system can
provide (CGU content hashes), and closing R2's `XFACTS_ALL` exception.

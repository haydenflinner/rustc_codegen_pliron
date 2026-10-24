# are-we-fast-yet / sightglass: Mach-O CFI, a nocapture blowup, and a slotmerge poison checker

are-we-fast-yet is cloned to `~/code/purerust/are-we-fast-yet`, with its
`sightglass` submodule. It was used as a correctness corpus: the test
suites of both workspaces ran under stock rustc and under pliron. The
`awfy` workspace matched (114 tests). `sightglass` pulls in all of
Wasmtime and Cranelift, and its tests run wasm benchmarks through a
pliron-compiled Wasmtime. It found four problems.

## 1. `global_asm!` with `.cfi_*` on Mach-O (a94897e)

`wasmtime-internal-fiber`'s stack-switch functions failed to assemble,
and then to link:

- rsasm wrote no Mach-O call frame information.
- An FDE's PC-relative `pc_begin` had no relocation.
- `objmerge` dropped the `SUBTRACTOR` half of each relocation pair.
- `objmerge` turned `ltmpN` labels into section relocations.
- Raw `_name` asm symbols were prefixed again, giving `__name`.

The fiber's own tests pass now (`test.sh asmcfi`).

## 2. nocapture fixpoint never finished (1ac16ec)

A store into a frame-confined slot added a flow edge from the stored
value to every load of that slot, so the relation grew as stores × loads.
After sroa, one slot in cranelift-codegen's ISLE matchers reached 1.2e9
flow tuples, and `Capture::run` ran for more than 37 minutes. Stores into
a slot with more than 8 loads now go through the slot pointer. The crate
builds in 3.5 minutes.

## 3. The miscompile: sightglass-cli `instrumented_component_validates` SIGSEGV

The crash was in `cranelift_codegen::machinst::lower::compute_use_states`.
The bisection chain was:

- `fnbisect` with pliron on `cranelift_codegen` only and everything else
  stock: one function, `constructor_simplify`.
- `bisect.py`: step `sroa` unit 5193/5194, a pinned split of the 24-byte
  `ContextIter::next` result around an invoke.
- `PLIRON_NOCAP=0` fixes it, so the bad input is a nocapture fact.
  `PLIRON_NOCAP_ONLY=@file` / `PLIRON_NOCAP_LOG=file` (new) bisected the
  facts to `compute_use_states::{closure#0}`'s `[0, 1]`. That fact is
  correct; two consumers draw more from it than it allows.
- slotmerge was one consumer (`PLIRON_SM_LIMIT=n`, new, found merge #13).
  It led to the poison checker below and two real slotmerge bugs.
- SROA's call-boundary sync across an **invoke** was the other.
  `PLIRON_SROA_SYNC_INVOKE` (now opt-in) separates it: plain calls are
  fine, invokes miscompile. Ruled out:
  - the unwind trampoline;
  - Cranelift's compile-time egraph;
  - pliron's compile-stage passes (bumphoist/rematsink/lastuse).

  The IR right after the split reads correct, so the defect is in
  lowering or later. **Open**; sync across invokes is off by default
  until it is found.

## 4. slotmerge poison checker (`PLIRON_SM_POISON=1`)

slotmerge has needed 7–8 correctness fixes since it landed. Its failures
look the same every time: a wrong live range only bites when a merge
actually reuses the bytes, far from the cause. The poison mode is a
use-after-scope check for its analysis:

- it merges nothing;
- it overwrites each slot coloring would consider with `0xA5` bytes after
  its last use;
- it does the same at the head of every block outside the slot's
  interval that is entered from inside it.

A wrong interval then fails at the first stale read, on every run.

Knobs:

- `PLIRON_SM_POISON_SLOTS=a..b` and `PLIRON_SM_POISON_SITES=a..b` bisect
  the slot and the exit path.
- `PLIRON_SM_LIMIT=n` caps the number of merges.
- `PLIRON_SM_INFO` logs every poison site with its interval and block
  positions.

Its first hour found two real bugs:

- **Partial per-trip writes counted as fresh.** An 8-byte store to an
  array iterator's counter made all 64 bytes look rewritten every trip
  (tests/loopvec, a few seconds per bisect step). Only a write covering
  the whole slot counts now. This was 7 of the 9 gate failures under
  poison.
- **Nested loop bodies past the outer latch.** A span was (header start,
  back-edge source end) in RPO, but RPO placed a nested loop's body after
  the outer latch (`compute_use_states`, `ss43`). Spans now close over any
  span whose header lies inside them.

Tests: `pliron-clif-opt/tests/slotmerge_loop_spans.rs`. Each case fails
with its fix reverted.

Under poison the gate's stanzas pass except:

- slotmerge's own unit tests, which expect merges;
- the `pgo` wasm leg's `sort_check` (core's sort built for wasm32): open,
  likely another interval bug.

## Performance notes from the same session

These are instruction counts (`/usr/bin/time -l`, load-independent) from
the real `bench/benches/kernels.rs` built against a criterion stand-in;
not landed.

- **selchain + mulcarry + stepcmp:** parse_only goes from +27% to +11%
  instructions (cycles +15% to +6%). No kernel regresses. Candidate for
  default-on after a crates run.
- **`PLIRON_XCGU_SIZE=300`:** hashmap goes from +45% to +25% instructions,
  binaries grow 3%. SipHash's `hash_one` sits in another CGU, beyond the
  native copy limit of 50. Compile time not yet measured.

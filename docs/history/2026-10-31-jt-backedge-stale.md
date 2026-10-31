# Jump threading: a back edge through a merge block kept its stale params

**Symptom.** With the new second-level `Option` null-test fold
(`fold_null_tests` looking through `select (a != 0), a, 0` and single-edge
params, d264785), the crates suite failed `indexmap` `map::tests::drain_range`:
`update_index` panicked "index not found" (src/inner.rs:70).

**Localization.** A look-through-only bisect counter (temporary) with
`PLIRON_PAR=0` named one fold in `indexmap::inner::Core::<usize,
()>::erase_indices`; the parallel-compile numbering is not deterministic,
so the first bisect had to be redone serially. Per-step `PLIRON_DIFFCHECK`
reported nothing: every trial was inconclusive (the interpreter panics on
the function's external names, `extname.rs:204`). A `PLIRON_CORPUS` dump
replayed with `clif-check vcode` reproduced it; the per-step dump
(`PLIRON_CLIF=erase_indices`) showed the back edge going wrong at jtround.

**Cause.** The shifted-index loop `for ((new, old), e) in
(start..).zip(end..).zip(shifted)` merges the zip's `Option` in a block
(`block379`) that rotates its params each trip (`v1390 := v1394`). The fold
made that block's flag a constant on the latch's back edge, and jtround
threaded `latch -> block379 -> block25` into `latch -> block25`,
substituting `block379`'s params by the edge's args. Those args named
`block379`'s *own* params: valid while every trip ran `block379`, stale
once the threaded edge skips it. The loop then carried the entry's
`start + 1` / `end + 1` forever. SSA still verified (the params dominate).
The fold was correct; it only made the edge threadable.

**Fix.** `jumpthread::round` refuses an edge whose args include a value
defined in the bypassed block, except a param passed back to itself
(`PLIRON_JTSTALE=0` disables the check). Regression test:
`pliron-clif-check/tests/jt_backedge_stale.rs` (the corpus dump through
the whole pipeline; fails without the check), now in gate's `unit` job.

**Diffcheck gap.** Mid-pipeline CLIF that calls user-named functions is
uninterpretable (`extname.rs:204` index out of bounds), so diffcheck was
silently inconclusive on this function at every step.

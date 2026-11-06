# 2026-11-06: licm region-frozen hoists skipped the clobber scan

## Symptom

`bench/heldout/run.py`'s `blind-sig` pliron binary burned CPU for
minutes at 100% (was a ~4ms / 2.148x program on 2026-10-31) — a
livelock, not a slowdown. `sample` put every tick in
`num_bigint_dig::algorithms::div::div_rem` (RSA key precompute). With
all transforms disabled it finished in ~12ms.

## Bisection

`harness/fnbisect.py` over ~768 `num_bigint_dig` functions narrowed the
hang to five: `div_rem`, `mul3`, `sub2`, `From<u8>::from`, and
`drop_glue<SmallVec>` — the failure needs the combination, no single
function miscompiles alone. `PLIRON_OPT_BISECT` +
`PLIRON_OPT_BISECT_DEBUG` then pinned transform unit 6590: the 4th
`licm-load` hoist inside `div_rem` (6589 good, 6590 hangs). A final-CLIF
diff of the two builds showed exactly one moved instruction:

    v5558 = load.i64 notrap v5557     ; *(stack_addr ss61 + 16)

hoisted from a conditional arm of the Knuth-D inner `while` loop into
the loop preheader.

## Root cause

`licm::hoists_inner` has two "region-frozen" early returns
(`inline::frozen_args` scoped tags and `region_frozen_op` records): when
the hoist stays inside the tagged region, the pass trusted the tag —
"no writer inside the region can clobber it" — and skipped the clobber
scan entirely, gated only by `own_write`, an *exact* same-root overlap
test (`norm_root` of the store's address must equal the loc's root).

`div_rem`'s inner loop stores the SmallVec `len` through a `select`
over address arms (`store v5838, v10718`, `v10718 = select(...)`).
`norm_root` cannot resolve a `select` → root mismatch → `own_write`
missed it → frozen early-return fired → the `len` load froze on its
entry value → the `cmp_slice` loop never converged.

The full `clobbers` scan does the right thing: an unresolvable store
root is a conservative may-write. It was computed anyway — just *after*
the early returns.

## Fix (`pliron-clif-opt/src/licm.rs`)

Compute `nw_w` (count of `clobbers`-flagged writers) *before* the
frozen paths and require `nw_w == 0` for both in-region early returns —
same check the fallthrough already applied. `own_write` stays: it
catches the scope-name same-root store `clobbers` deliberately treats
differently (`loadfwd_frozen_same_root_store`).

Regression test `pliron-clif-opt/tests/licm_frozen_select_store.rs`:
loop counter read through a frozen-tagged load, bumped through a
`select`-rooted store — fails without the fix (load hoists to the
preheader, loop never exits), passes with it.

## Result

`blind-sig`: livelock → **1.107x** [1.027,1.284] (15 interleaved
rounds), output byte-identical to the LLVM binary. `blake3-scalar`
measured 1.164 [0.896,2.026] in the same run — noisy box; an earlier
quiet-window reading put it at 1.05.

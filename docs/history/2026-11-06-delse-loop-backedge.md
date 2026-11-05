# 2026-11-06 — wpeep `delse` folded `br` back-edges to `loop` labels

## Symptom

Wasm std tests (`tests/wasm/run_std.mjs`, `-Zbuild-std=std`):

- `float_fmt` trapped `unreachable`: a `panic_const_div_by_zero` inside
  `core::num::imp::flt2dec::strategy::grisu::format_exact_opt` — the
  digit loop's `remainder / ten_kappa` ran with `ten_kappa` decayed to
  zero because the `i > max_kappa` exit was never reached.
- `hm_u64(100)` returned `100005` instead of `100000` — 5 missed
  lookups.

Both symptoms were deterministic per wasm binary but flipped with
`PLIRON_OPT_FNS` selection: optimizing only `~format_exact*` reproduced
`float_fmt` alone (hashmap bodies kept their unoptimized pre-wpeep
shape, which did not contain the bad pattern). `PLIRON_OPT_BISECT=0`
passed both, pointing at a transform — but the counted-transform
bisect was a red herring: the culprit is `wpeep`, which is not
bisect-gated. `PLIRON_WASM_PEEP=0` passed; per-sub-pass kills isolated
`PLIRON_WASM_DELSE=0` as the culprit.

## Root cause

`delse` folds `if c { A; br K' } else { br K }` when the `if`'s `end`
is followed by K enclosing `end`s: it reasoned that `br K` lands at the
same continuation as the fallthrough, so the else arm (and arm1's
trailing `br K'` under the same check) is dead.

That is only true when label K is a `block`/`if` (a forward label).
`br` to a **`loop` label jumps to the loop header** — a back-edge, not
a forward exit. `ends_clear` checked only that K `end` bytes followed
the `if`'s `end`, never which construct those `end`s close.

In `format_exact_opt` (core `cgu.12`, body 42) the pre-body contained
the digit loop as nested `if`s inside a `loop`; the `ten_kappa != 0`
arm ended in `br 4` whose label 4 counted out to the enclosing `loop` —
the loop's back-edge. `ends_clear(4)` saw the four `end`s (three `if`s
plus the `loop`'s `end`) and folded: back-edge removed, `elideblk` then
dropped the unreferenced `loop` header, and the "loop" became
straight-line code that fell through to the next iteration's locals
mismapped — `ten_kappa` decayed through `ten_kappa /= 10` until the
`rem / ten` division-by-zero guard fired.

`hm_u64(100) = 100005` is the same bug: the hashmap probe loop's
back-edge folded the same way, so a few buckets were never probed.

## Fix

`delse`'s main scan now carries a stack of ancestor construct opcodes
(one byte each: `block`/`loop`/`if`). A fold candidate drops each `br`
only when

- the K `end`s after the `if`'s `end` are all `end` bytes (as before),
  **and**
- the K-th ancestor (`stack[len - K]`; the `if` itself for K = 0, the
  function body past the end of the stack) is **not** `0x03` (`loop`).

The else arm still needs its own `br K` dead to fold; arm1's trailing
`br K'` drops only when `br_dead(K')`. The stack is maintained
incrementally by the linear scan — folded regions are consumed
atomically and balanced, unf folded insts are pushed/popped as they
pass, so the check costs O(1) per candidate.

## Regression tests

- `delse_br_to_loop_label_keeps_both_arms` — `loop { if {A; br 1}
  else {br 1} }`: both `br 1`s are back-edges; nothing may fold.
- `delse_else_folds_but_loop_backedge_kept` — `loop { block { if {A;
  br 2} else {br 1} } }`: the else `br 1` (forward, block `B`) still
  folds; arm1's `br 2` (back-edge, `L`) is kept.
- `tests/wasm/lib.rs` `float_fmt` — end-to-end reproducer: `"{:.5}
  {:e} {}"` over PI, 1.5e300, 0.3f32 traps without the fix.

## Evidence trail

- `PLIRON_OPT_BISECT=0` passed → counted transforms clean; the CLIF
  pipeline's final `format_exact_opt` was verified correct against the
  source idiom (`i > max_kappa` bound intact).
- `PLIRON_WASM_PEEP=0` passed → bug is post-CLIF, inside `wpeep`.
- `PLIRON_WASM_ELIDEBLK=0`, `=BTR=0`, `=BRI/=BRI2=0` still failed;
  `PLIRON_WASM_DELSE=0` passed.
- `PLIRON_WASM_BODYDUMP` pre/post bodies: the delse-off post keeps
  `loop` + `br 4` (back-edge) inside `format_exact_opt`; the delse-on
  post has the `br` and the `loop` header both gone.

## Verification

- Unit tests: both new tests fail on HEAD, pass with the fix; the rest
  of `wpeep_tests` is unchanged (the one other failure,
  `licm_get_get_add_hoists`, fails identically on HEAD — pre-existing,
  `elideblk` now removes the empty `loop` the test expects).
- `tests/wasm/run_std.mjs`: all 6 tests pass on the rebuilt dylib,
  including both previously-broken `float_fmt` and `hm_u64`.
- `harness/quick.py`, `harness/gate.py --only wasm`, `harness/gate.py`
  (93 jobs): all green.

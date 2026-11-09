# Post-rotation dead-loop deletion + escape-remap ICE fix (2026-11-11)

`PLIRON_LOOPDEL`, default on (64ca6c2 + d205f29). A post-`looprot`
`s_loopidiom` invocation folding post-tested countdown loops whose only
effects are `st(f(iv))` to invariant addresses plus re-executable loads
— num_bigint_dig `montgomery`'s `*p = --i` loops, which LLVM emits as a
single final `str xzr`.

## Mechanism (reuses the existing `DeadPlan`/`apply_dead` machinery)

- `Ins::Load`: a new leaf — re-execute one load on the fast path.
  `arg_ins` maps an in-loop load to it when the load is `loadable`:
  every store dominates it (its last-iteration read is post-store
  state, reproduced by emitting after the folded stores), or every
  store is stack-slot rooted (disjoint from the load's param root).
- `count(relax)`: `step < 0` countdowns and in-body `iconst` bounds
  (remat'd by `emit`), LOOPDEL-gated only. `trips_idx` `uge` arm:
  `iters = iv0 - bound + 1`.
- `rmw_locs`: `RmwDelta::Set` now carries `arg_ins`-built closed forms
  (`st(f(iv))` evaluated at `itb = iters-1` post-tested), and load-only
  locs are exempt from the single non-stack-root rule — they re-order
  after all folded stores anyway.
- `apply_dead`: folded stores emit first in `mb`, then load-bearing
  exit args (they must see post-store state), then the exit jump.

## The latent bug it exposed

`apply_dead`'s escape remap rewrote `v -> np` (live-out loop value →
appended `exit_dest` param) in **every** block. Once `plan_dead`
converted a *surviving* loop (non-empty `preds` keep a non-fast path),
in-body uses of the escape kept getting remapped to `np`, which doesn't
dominate the body — `egg`'s `SymbolLang::matches` ICE'd in `s_egraph`
(`iadd v147` in the latch where `v147` is `block26`'s own param). Fix:
remap only inside `exit_dest`'s dominion — exactly the domain the
comment already claimed (`PathBuf` needed downstream-of-dest uses).
Dominion is resolved before `make_block` since the stale `dt` reads
`SecondaryMap` defaults for new blocks.

## Numbers

- `montgomery` 858 → 816 static insns (same-dylib A/B; LLVM 307 — the
  remaining gap is the bounds-check/fragmentation class, not this idiom).
- Held-out suite `loopdel/pliron 0.994 [0.977,1.015]` — trending win:
  blind-sig 0.973, regex 0.955, **html-rewriter 0.978***, nothing
  significantly slower.
- gate 94/94 and fuzz 400/400 on the flipped default.

## Tests

- `loopdel_countdown.rs` — countdown folds to 1 store + 1 reachable
  load; a same-root pre-store load declines.
- `loopdel_escape_remap.rs` — the egg shape (escape IV still used in
  the latch); asserts no param is used outside its dominion.
  Red-verified: fails without the dominion filter.
- `loopdel_gated.rs` — `=0` kill switch keeps the loop reachable.

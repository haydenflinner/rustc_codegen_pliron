# 2026-11-05 bunroll LCSSA repair: varint_postcard

`PLIRON_BUNROLL` existed for exactly postcard's `for i in
0..varint_max` varint decoders but never fired on them: the
exit-cone walk bailed `leak-arg` on every candidate. The `done`
exit validates `i == varint_max` — a loop-defined value read
directly by an out-of-loop block, which the old free-var check
rejected (cloned exits aren't dominated by the original header, so a
free use would lose its def).

## Fix

`pliron-clif-opt/src/bunroll.rs` now repairs the cone instead of
declining:

1. Walk the exit cone (same 64-block budget) collecting each block's
   direct free uses of in-loop values — inst args and its
   terminator's edge args alike.
2. Propagate needs backward to a fixpoint: a cone block must supply
   its cone successors' values on its out edges.
3. Bail only if a needy block has a pred outside loop+cone (in
   practice unreachable on already-valid input — a loop-defined
   value that dominates the use forces all its preds into the cone).
4. `repair()` appends the values as params of the cone blocks,
   rewrites the free uses (splink-style raw-value capture, since
   `map_inst_values` can't re-enter `resolve_aliases`), and appends
   matching args on every pred edge — in-loop preds pass the
   original value, cone preds pass their own param. Cloned exits
   then forward cloned values automatically via the existing arg
   mapper.

## Numbers (battery, same-state A/B)

- varint_postcard 6.84 -> 5.73 ms (-16%); the u64 decoder now emits
  the ldrb + const-shift `orr` + `tbz` chain LLVM emits instead of a
  runtime-`lsl` tracking loop. Scoreboard 0.876 stays faster.
- varint (non-postcard) -4.8%; sieve/hashmap/eqbug/iterw/tokenize/
  nbody/fib flat.
- Compile time ~+15% on the bench crate (bounded by MAX_BODY and the
  iters cap; only fires on small constant-trip loops).
- wasm leg under `PLIRON_BUNROLL=1`: identical to baseline (the two
  pre-existing miscompiles `float_fmt` and `hm_u64` only — no
  verifier failures from the CFG/param mutation).
- gate 91 jobs green; `bunroll_lcssa.rs` red/green-verified (loop
  declined without the repair, diffcheck-clean with it).

Flipped default-on in `DEFAULT_ON`; `PLIRON_BUNROLL=0` restores the
old opt-in-only behavior (no transform at all).

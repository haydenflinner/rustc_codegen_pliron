# loopvec: mask/data `band` is lane-exact — ascii_lowercase vectorizes

2026-11-11. `to_ascii_lowercase` was ~258 insns scalar vs LLVM's 141
inlined+vectorized; traced to a `vec bail ... store value` in
`alloc::vec::spec_from_iter_nested` (the `Map<slice::Iter, lower>`
collect loop) and siblings in `selectors`, `cssparser`, `lol_html`.

The loop body is `dst[i] = src[i] | (((src[i] - 'A') u< 26) & 1) << 5)`:
`icmp` produces a {0,-1} lane mask that goes into `masks`, but
`band(mask, 1)` was classified neither mask (operand 1 isn't one) nor
`can_vec` (operand 0 isn't) — so the `ishl`/`bor` tail stayed
unreplicable and the whole map loop stayed scalar.

Fix: in `loopvec`'s `can_vec`/`masks` fixpoint, the `band`/`bor`/`bxor`
arm now accepts operands drawn from `masks ∪ can_vec ∪ splats`;
all-mask stays a mask, mixed produces a plain replicable value —
bitwise ops are lane-faithful regardless of operand class
(`band({0,-1}, 1)` yields {0,1} lanes exactly). `emit_val` already
materialises mask operands as vector icmps.

Every `store value` bail in the html-rewriter build is gone; all
lowercase copies now emit `sub.16b`/`cmhi.16b`/`and.16b`/`shl.16b`/
`orr.16b` bodies (three 16-byte groups per iter). Static size on the
lowercase functions grew ~+318 insns total (vector body + remainder +
entry checks).

Held-out A/B, `pliron-bandvec` vs stale `pliron-loopdel` binaries,
7 interleaved rounds: geomean 1.010 [0.993, 1.042] old/new
(positive trend, CI straddles 1); per-program all within noise;
llvm-ratio geomean 1.087 -> 1.077. Unit test
`band_mask_splat_chain_vectorizes` covers the idiom; corpora and fuzz
clean. Landed direct (default-on pass coverage widening, same class
as f515d4d/a6f134c).

Remaining in the same family: `selectors::to_ascii_lowercase`'s
in-place remainder loop (load+store through the same ptr) and
`utf8_valid_up_to` (683 vs 217; per-byte `add`-heavy IV recomputation —
structural, needs the interior-merge-param IV classification that also
blocks decompress's bit-buffer accumulate).

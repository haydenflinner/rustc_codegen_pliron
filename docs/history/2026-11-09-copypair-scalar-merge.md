# 2026-11-09 — copypair scalar-pair merge for the brotli swap

Follow-up to `2026-11-08-amode-crc-brotli.md`: the fix direction named
there — merge the scalar `i64` halves of a 32-byte `ZopfliNode` swap
into a second `i64x2` access — is implemented.

## What landed

`copypair` (still opt-in `PLIRON_COPYPAIR`) gained `merge_pairs`: two
consecutive `load.i64` from the same base at offsets `o`/`o+8`, each
value used exactly once in two consecutive `store.i64` to the same
base at `r`/`r+8`, become one `load.i64x2` + one `store.i64x2`. Same
location, same bytes, no reordering — so no aliasing analysis is
needed beyond matching flags post-`plain` and `notrap`. A
precomputed value->use map and per-block positions keep it linear;
a `gone` set stops stale DFG data from re-matching removed insts.

In the swap loop the 64-byte bidirectional copy is now 2x `i64x2`
each way. With `PLIRON_AMODESHARE` on top, the second access keeps the
shared base register (+16 imm) so the vendored `FpuLoad/StoreP128`
pair fusion emits real `ldp q`/`stp q` (1251 paired vector ops in the
profbench binary, up from single `ldr q`/`str q`).

## Numbers

- Unit: 7/7 `copypair` tests green incl. the new swap-tail test;
  quick + gate (93 jobs) green.
- Mixed gzip+brotli profbench: ~**1.215x** (was 1.235x) — the swap is
  now clean vector code but the wall-time delta is small because the
  function's self-time was already nearly matched: `Brotli` went from
  1.87x sample share to ~1.0x share in the profile (462/~2300 vs
  LLVM's 463/~2450). The remaining spread is thin: `FindMatchLength`,
  `log2f`, `bzero`/`memmove` counts, and a structural insn-count gap
  (2352 vs 1320 — inlining depth, not the copy shape).

## State

`PLIRON_COPYPAIR` and `PLIRON_AMODESHARE` remain opt-in. A/B on the
held-out suite plus crates/stage-2 before either flips (work list #2).

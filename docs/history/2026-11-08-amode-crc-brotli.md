# 2026-11-08 — aarch64 scaled amodes, Apple CPU feature tiers, brotli gap

Follow-up to `2026-11-07-licm-nashel-provescapes.md`: the held-out
`rust-compression` gap was chased end to end.

## What landed

1. **aarch64 scaled amodes for `iadd y,y` / `imul y,C` indices**
   (vendor patch 105, `aarch64_scaled_amode.rs` test). The egraph's
   `ishl y, 1 -> iadd y, y` rewrite hid the scaled-index pattern
   upstream `amode_no_more_iconst` rules 6/7 key on; `base + 2*i`
   emitted a standalone `add` + reg+reg amode. New rules fold both
   shapes; i32 indices chain to `uxtw #1`. `enc_ldst_reg` keeps its
   uxtw/sxtw/sxtx/lsl whitelist — the uxtb/uxth/sxtb/sxth option
   encodings **trap on Apple Silicon** (verified: SIGILL on a real
   `ldrh [x, w, uxth #1]`, and llvm-mc refuses to assemble them), so
   i16-typed indices must go through `RegScaled` on the extended value.

2. **Apple-silicon `cpu_features` tiers completed** (`src/lib.rs`).
   Only aes/sha2/sha3/dotprod/lse/fp16/i8mm were listed, so
   `cfg!(target_feature = "crc")` (plus 18 more cpu-implied features)
   was false where stock reports true. crc32fast's no_std dispatch
   reads that cfg and silently took the scalar baseline — a 3x hit on
   ~15% of the compression profile. Tiers now seed the same entry
   features LLVM's cpu model resolves to (verified identical to
   `rustc --print cfg -Ctarget-cpu` for all six apple tiers, including
   that `bti`/`mops`/`mte`/`sme` stay off where stock keeps them off);
   rustc's implied-feature closure fills the rest. `-Ctarget-feature=-x`
   still wins.

## Numbers

- `compress_inner` self-time: **at parity** (81 vs 79 samples) — the
  amode work closed the codegen-shape gap LLVM had (~21 vs ~24 insns
  in the insert loop, `uxtw #1`/`lsl #1` amodes now emitted).
- pliron's crc32 now calls `crc32fast::specialized::aarch64::State::update`
  (`__pliron_llvm_aarch64_crc32x` in the disasm) like stock.
- Mixed gzip+brotli profbench: **1.235x** (was ~1.5x before the crc fix).

## Where the remaining gap is

`BrotliCreateHqZopfliBackwardReferences`: 864 vs 463 samples (1.87x),
2352 vs 1320 insns. The hot inner loop does 32-byte `ZopfliNode`
swaps; LLVM emits `ldp q; ldp q; stp q; stp q` (2 vector ops each way)
while ours gets `i64x2` for one half and scalar `i64` pairs for the
other (SROA leftovers), i.e. `ldr`+`ldp x`+`ldr` per side. Fix
direction: generalize `copypair` (currently opt-in, only matches
straight-line `load;store` runs) to merge adjacent same-base scalar
loads/stores into `i64x2`/`i8x16` when all uses stay in the copy
region, or widen SROA's per-slice expansion to vector chunks. Second
mover: `BrotliHistogramCombine` 1.65x, `FindMatchLengthWithLimit` 1.32x.

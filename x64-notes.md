# x86-64 codegen notes (wt/x64-codegen)

Running log of work + learnings. Bench: `/tmp/cpubench/{main,wide}.rs`,
`rustc +nightly-2026-10-06 -O --target x86_64-apple-darwin -Ctarget-cpu=x86-64-v3
--edition 2021 [-Zcodegen-backend=...dylib]`, run via `arch -x86_64` (Rosetta).

## Session 1: target-cpu → Cranelift ISA flags

- Cargo.toml: cranelift `host-arch` → `all-arch` (required to emit x86_64).
- **`-Ctarget-cpu` was a no-op for x86**: `cpu_features()` in src/lib.rs only
  knew Apple cpus → `internal_target_features` got no sse4.1/avx2 →
  `has_sse41` etc. never enabled → every SSE4.1 lowering rule fell back to
  SSE2 sequences (`umax.i32x4` = pxor-bias + pcmpgtd + pand/pandn/por +
  constant reload *inside the loop*; `imul.i32x4` = pmuludq dance).
  Added an x86 cpu table: x86-64-v2/v3/v4 + mainstream Intel/AMD names.
  `internal_target_features` expands implied features, so entry points
  suffice (avx2 pulls the whole sse chain).
- **Rosetta finding: VEX-128 xmm encodings are ~2x slower than legacy SSE**
  in streaming loops (verified with a faithful micro-repro: 104ms vs 54ms).
  Integer VEX (BMI1/2: mulx/shlx) is fine. ymm VEX is fast too (stock LLVM
  uses it) — but vendored Cranelift x64 has *no >128-bit vector register
  class* (`type_register_class` returns None over 128 bits), so has_avx*
  only bought VEX-128 encodings. `build_isa` now maps only
  sse3/ssse3/sse4.1/sse4.2/popcnt/bmi1/bmi2/lzcnt/cmpxchg16b for x86_64;
  `PLIRON_X64_VEX=1` re-enables the VEX families for real-hardware runs.
- Result (Rosetta, v3 flags): everything in wide.rs/main.rs at parity or
  faster than stock LLVM -O:
  max_u 0.193 vs 0.233, min_i 0.195 vs 0.237, dot_i32 0.095 vs 0.132,
  sum_u8 0.388 vs 0.702, dot_i8 0.320 vs 0.447, axpy 0.384 vs 0.581,
  clamp8 0.092 vs 0.144, plus prior wins held (find_off 14x, has_val 4.4x,
  dot_u8 3.1x, even_sum 2.7x, cnt_vowel 2.3x, rev_copy32 1.7x).
  Residual gaps: sum2d ~5% slow (ordered fadd reduction — extractlane fold
  per group + serial scalar acc; LLVM splits vector accs and folds with
  shuffle+add tree), fillzero/xor_fold parity, matmul ~parity (LLVM sees
  n=256 constant; black_box blocks it for us).
- 256-bit vectors: not feasible without writing ymm register-class +
  lowering support in vendored Cranelift (upstream never did it; only
  aarch64 has real vector-legalization for >128). loopvec stays 128-bit;
  UNROLL=4 gives 512 bits/iter anyway.

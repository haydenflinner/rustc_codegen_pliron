# Vendored cranelift-assembler-x64-meta 0.135.5

Build-time instruction tables for `cranelift-assembler-x64`. Vendored to add
encodings upstream doesn't define; changes are marked `PLIRON:` in source.
Submit upstream once proven, then drop this copy.

1. `src/instructions/avg.rs`: `psadbw`/`vpsadbw` (SSE2 `66 0F F6`,
   VEX.128.66.0F `F6`). The codegen-meta ISLE generator picks these up
   automatically as `x64_psadbw_a_or_avx`.

2. `src/instructions/wide.rs` (new, PLIRON-only): VEX.256 and
   EVEX.512 forms for the wide-vector op set — float add/sub/mul/
   div/min/max/sqrt/fma/round for ps and pd, broadcasts, int
   add/sub/mul/avg/abs/sat/min-max/compares/shifts, vpmovsx/zx
   extends, vpack/vpshufd/vpermps, vmovmsk/vptest, vextract/insert
   lane-chunk ops. Operands use the `xmm_m256`/`xmm_m512` memory
   locations (see cranelift-codegen patch 23/24); byte/word
   512-bit forms are gated `avx512bw`, `vpmullq` on `avx512dq`.

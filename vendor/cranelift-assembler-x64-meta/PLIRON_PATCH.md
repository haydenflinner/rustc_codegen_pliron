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

3. `src/instructions/wide.rs`: dot-product family — `vpmaddwd`/`vpmaddubsw`
   Y (avx2) and Z (avx512bw), `vpdpbusd` Y (avx_vnni), H (avx512vnni+vl),
   Z (avx512vnni). Feeds the `sdot`/`udot`/`udot_acc` CLIF opcodes
   (cranelift-codegen patch 26).

4. `src/instructions/wide.rs`: runtime-index permutes — `vpermd`/
   `vpermps` X/Z (avx512f, +vl at 128), `vpermq`/`vpermpd` X/H/Z
   (EVEX reg-index qword permute; avx512f, +vl at 128/256), and
   `vpermt2d`/`vpermt2q`/`vpermt2ps`/`vpermt2pd` X/H/Z two-source
   forms (index is the read-write first operand). Feeds
   `vswizzle`/`vswizzle2` (cranelift-codegen patch 27).

5. `src/instructions/wide.rs`: x86 VSIB gather/scatter — VEX
   `vpgatherdd/dq/qq/qd` + `vgatherdps/dpd/qpd` X/Y (avx2; mask operand
   is declared first since the generated `read_write_xmm` operand
   collector ties a `rw` write to use-index 0) and EVEX
   `vpgather*`/`vgather*`/`vpscatter*`/`vscatter*` H/Z forms (avx512f,
   +avx512vl for ymm-index forms). All `.custom(Encode)` — emitted via
   `emit_vsib` in cranelift-assembler-x64. Feeds `vgather`/`vscatter`
   (cranelift-codegen patch 28).

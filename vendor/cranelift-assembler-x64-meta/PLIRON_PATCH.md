# Vendored cranelift-assembler-x64-meta 0.135.5

Build-time instruction tables for `cranelift-assembler-x64`. Vendored to add
encodings upstream doesn't define; changes are marked `PLIRON:` in source.
Submit upstream once proven, then drop this copy.

1. `src/instructions/avg.rs`: `psadbw`/`vpsadbw` (SSE2 `66 0F F6`,
   VEX.128.66.0F `F6`). The codegen-meta ISLE generator picks these up
   automatically as `x64_psadbw_a_or_avx`.

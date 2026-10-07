# Codegen improvements (-O)

Passes run in `finish_module` (src/lib.rs) before lowering to Cranelift, only at `-O`.
Each one can be switched off with its env var set to `0` (read when the backend compiles a crate).

Benchmark: stage2 rustc (built by this backend) compiling `regex-syntax-0.8.10/src/lib.rs`,
`--edition 2021 --crate-type lib`, best of 3 wall time, `perf stat -e instructions:u`.
Script: `selfhost/bench.sh` (`RC=<rustc>` to point it at another compiler).
Stage1 (LLVM-built) rustc: 0.79s metadata.

| pass (env toggle) | metadata | link | instr. metadata | instr. link | kept? |
|---|---|---|---|---|---|
| baseline (ade7be2) | 3.25s | 6.06s | 14.84B | 32.11B | |
| SROA + promotion to Cranelift vars (`PLIRON_SROA`) | 3.02s (−7%) | 5.44s (−10%) | 13.23B (−11%) | 28.41B (−12%) | kept |
| + notrap flags on non-volatile loads/stores (`PLIRON_NOTRAP`) | 2.96s | 6.49s (noise) | 13.23B (±0) | 28.42B (±0) | dropped (no instruction change) |
| + bottom-up transitive inlining, invoke sites (`PLIRON_INLINE_BU`) | 2.51s (−17%) | 4.66s (−14%) | 11.88B (−10%) | 25.63B (−10%) | kept |
| + native 128-bit SIMD (`PLIRON_SIMD`) | 2.47s | 4.04s (−13%) | 9.12B (−23%) | 20.16B (−21%) | kept |
| + switch via Cranelift `Switch` jump tables (`PLIRON_SWITCH`) | 2.21s | 4.57s (noise) | 9.09B (−0.3%) | 19.76B (−2%) | kept |
| + cold-block layout (`PLIRON_COLD`) | 2.12s | 3.95s | 9.08B (±0) | 19.82B (+0.3%) | kept (layout only; instruction count unchanged, wall time within noise) |
| + nounwind inference, invoke→call (`PLIRON_NOUNWIND=1`) | 3.13s (noise) | 4.07s | 9.08B (±0) | 19.80B (−0.1%) | opt-in only: just 62 of ~1000 invokes in regex-syntax have provably non-unwinding callees |
| + memcpyopt sret call-slot forwarding (`PLIRON_MEMCPYOPT=1`) | 2.59s | 5.23s (noise) | 11.87B (−0.1%) | 25.63B (±0) | dropped (opt-in only) |
| instcombine, egg e-graph (`PLIRON_INSTCOMBINE`) | pending | | | | |

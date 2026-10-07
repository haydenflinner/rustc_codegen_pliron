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
| + inline memcpy/memmove/memset ≤128 B in 16-byte vector chunks (was ≤64 B, 8-byte) | 2.10s | 4.08s | 8.95B (−1.4%) | 19.49B (−1.7%) | kept |
| omit frame pointers (`PLIRON_OMIT_FP`) | – | – | – | – | no effect: regex-syntax object is byte-identical; Cranelift x64 always sets up rbp frames |
| + internal ABI: tail CC for non-escaping, non-invoked local fns (`PLIRON_TAILCC`) + sret→register return (`PLIRON_SRET2REG`) | 2.15s | 3.94s | 8.92B (−0.3%) | 19.40B (−0.5%) | kept (first version, tail CC on invoked callees too, was +1%: try_call into tail CC clobbers all registers) |
| + egg instcombine (`PLIRON_INSTCOMBINE`) | 2.13s | 3.82s | 8.88B (−0.4%) | 19.30B (−0.5%) | kept |
| + memcpyopt sret call-slot forwarding (`PLIRON_MEMCPYOPT=1`) | 2.59s | 5.23s (noise) | 11.87B (−0.1%) | 25.63B (±0) | dropped (opt-in only) |
| instcombine, egg e-graph (`PLIRON_INSTCOMBINE`) | pending | | | | |

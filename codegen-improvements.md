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
| memcpyopt call-slot forwarding (`PLIRON_MEMCPYOPT`) | pending | | | | |

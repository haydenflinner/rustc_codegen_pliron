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
| + notrap flags on non-volatile loads/stores (`PLIRON_NOTRAP`) | 2.96s | 6.49s (noise) | 13.23B (±0) | 28.42B (±0) | left on (no instruction change, harmless) |
| + bottom-up transitive inlining, invoke sites (`PLIRON_INLINE_BU`) | 2.51s (−17%) | 4.66s (−14%) | 11.88B (−10%) | 25.63B (−10%) | kept |
| + native 128-bit SIMD (`PLIRON_SIMD`) | 2.47s | 4.04s (−13%) | 9.12B (−23%) | 20.16B (−21%) | kept |
| + switch via Cranelift `Switch` jump tables (`PLIRON_SWITCH`) | 2.21s | 4.57s (noise) | 9.09B (−0.3%) | 19.76B (−2%) | kept |
| + cold-block layout (`PLIRON_COLD`) | 2.12s | 3.95s | 9.08B (±0) | 19.82B (+0.3%) | kept (layout only; instruction count unchanged, wall time within noise) |
| + nounwind inference, invoke→call (`PLIRON_NOUNWIND=1`) | 3.13s (noise) | 4.07s | 9.08B (±0) | 19.80B (−0.1%) | opt-in only: just 62 of ~1000 invokes in regex-syntax have provably non-unwinding callees |
| + inline memcpy/memmove/memset ≤128 B in 16-byte vector chunks (was ≤64 B, 8-byte) | 2.10s | 4.08s | 8.95B (−1.4%) | 19.49B (−1.7%) | kept |
| omit frame pointers (`PLIRON_OMIT_FP`) | – | – | – | – | no effect: regex-syntax object is byte-identical; Cranelift x64 always sets up rbp frames |
| + internal ABI: tail CC for non-escaping, non-invoked local fns (`PLIRON_TAILCC`) + sret→register return (`PLIRON_SRET2REG`) | 2.15s | 3.94s | 8.92B (−0.3%) | 19.40B (−0.5%) | kept (first version, tail CC on invoked callees too, was +1%: try_call into tail CC clobbers all registers) |
| + egg instcombine (`PLIRON_INSTCOMBINE`) | 2.13s | 3.82s | 8.88B (−0.4%) | 19.30B (−0.5%) | kept |
| + dead-argument elimination for internal fns (`PLIRON_DEADARG=1`) | 2.17s (noise) | 3.99s (noise) | 8.88B (±0) | 19.30B (±0) | opt-in only: only 107 of 2655 internal-fn params in regex-syntax are dead; `.text` −0.03% |
| inline size limit 40 → 80 ops (`PLIRON_INLINE=80`) | 2.03s | 3.70s | 8.29B (−6.6%) | 18.09B (−6.3%) | kept; stage2 build time unchanged (5:55 vs 6:26) |
| inline size limit 80 → 160 ops (`PLIRON_INLINE=160`) | 1.88s | 3.42s | 7.99B (−3.6%) | 17.37B (−4.0%) | kept; librustc_driver.so +1.8%, stage2 build 6:12 |
| inline size limit 160 → 320 ops (`PLIRON_INLINE=320`, new default) | 1.89s (noise) | 3.50s (noise) | 7.83B (−2.0%) | 17.04B (−1.9%) | kept as default; librustc_driver.so +3% vs 160 (260 MB), stage2 build 6:39; returns diminishing (−6.5% → −3.8% → −2% per doubling) |
| + single-caller local fns inlined at 10× the size limit (`PLIRON_INLINE_ONCE`) + unreferenced local fns not lowered (`PLIRON_DEADFN`) | 1.84s | 3.42s | 7.70B (−1.7%) | 16.80B (−1.4%) | kept. Ablation (stage2 rebuilt with `PLIRON_INLINE_ONCE=0`): 7.83B / 17.03B, so all of the runtime gain comes from single-caller inlining; `PLIRON_DEADFN` only cuts backend work (regex-syntax at -O: 1280 of 1956 fns not lowered, compile 3.70s → 3.42s, `.text` −21%; with both: 2.71s, −23%). librustc_driver.so 260.4 → 256.7 MB |
| + runtime-size memcpy/memmove: sizes 16–32 B as two overlapping 16-byte moves, others call libc (`PLIRON_MEMFAST`) | 1.92s (noise) | 3.44s | 7.57B (−1.7%) | 16.62B (−1.1%) | kept; librustc_driver.so +0.3% |
| + inline callees that have their own landing pads, at plain call sites (`PLIRON_INLINE_EH`) | 1.71s | 3.32s | 7.40B (−2.2%) | 16.33B (−1.7%) | kept; librustc_driver.so +2.3%. Calls to `core::hint::select_unpredictable` went 7546 → 0 |
| + drop block args whose incoming values are all one value, so inlined alloca addresses reach SROA (`PLIRON_PHISIMP`) | 1.60s | 2.97s | 6.94B (−6.2%) | 15.33B (−6.1%) | kept; regex-syntax -O: 39.8k block args removed, `.text` −9%. Found via the stage1/stage2 per-function diff: `rustc_lexer::Cursor::eat_while` was 26× stage1, its `Chars` clone stayed on the stack |
| + fold loads from immutable globals with known initializers (`PLIRON_CONSTLOAD`) | 1.66s | 3.11s | 6.88B (−0.9%) | 15.18B (−1.0%) | kept; regex-syntax -O: 4019 loads folded |
| + memcpyopt sret call-slot forwarding (`PLIRON_MEMCPYOPT=1`) | 2.59s | 5.23s (noise) | 11.87B (−0.1%) | 25.63B (±0) | dropped (opt-in only) |
| one CGU per crate (`rust.codegen-units = 1`), so the inliner sees the whole crate | – | – | – | – | not measurable here: stage2 build OOM-killed (28 GB RSS) on `rustc_query_impl` and `cranelift-codegen`; pliron IR for a whole large crate doesn't fit in 31 GB |
| Cranelift `opt_level=speed` instead of `speed_and_size` | – | – | – | – | not tried: Cranelift 0.135 only checks `opt_level != none` (context.rs), so the two are identical |

Net at the current defaults: 6.88B / 15.18B instructions (−54% / −53% vs baseline),
1.66s / 3.11s wall (stage1 LLVM-built: 0.79s metadata).
Correctness at the current defaults: `./test.sh`, `./test.sh --sysroot`, -O std/asm/unwind,
and `UI_FLAGS=-O tests/ui_run_pass.py`: 2537 pass, the remaining 57 fail identically on stock LLVM.

# Raw performance data — 2026-10-16, Windows x64, i7-12700K (8P+4E), RTX 3080

All runs on `x86_64-pc-windows-msvc`, rustc `nightly-2026-10-06` (`-O`, default CGUs).
Backend revision `45063dd`; cubecl fork `2556b654f` (`clif-cpu-jit` branch).
The i7-12700K is a hybrid part — best-of-N is reported where noted.

## 1. Native compile time — `bench/wasm/lib.rs` (16 kernels, ~4 kLOC), `--emit link`

| backend | median of 3 | vs LLVM |
|---|---|---|
| stock LLVM | 488 ms | 1.00× |
| pliron (release dylib) | 925 ms | 1.90× slower |
| pliron (debug dylib) | 1188 ms | 2.43× slower |

`-Ztime-passes` codegen portion: LLVM `codegen_to_LLVM_IR + finish_ongoing_codegen` ≈ 0.30 s;
pliron-release ≈ 0.72 s (the rest is shared rustc frontend work).

## 2. Native runtime — criterion `bench/run.sh` (pliron binary vs saved LLVM baseline)

| kernel | LLVM (ms) | pliron (ms) | ratio |
|---|---|---|---|
| chars_alpha | 1.7879 | 2.2779 | 1.274 |
| chars_ascii | 0.9483 | 0.9604 | 1.013 |
| chars_mixed | 9.3674 | 9.3921 | 1.003 |
| contains_rng | 2.5120 | 2.9918 | 1.191 |
| eqbug | 0.1152 | 0.1067 | 0.926 |
| fib | 7.6769 | 7.4544 | 0.971 |
| hashmap | 45.660 | 45.927 | 1.006 |
| hmget | 10.176 | 10.374 | 1.019 |
| iterw | 30.172 | 31.491 | 1.044 |
| matmul | 5.6799 | 5.6127 | 0.988 |
| nbody | 69.958 | 69.455 | 0.993 |
| parse_only | 276.76 | 303.54 | 1.097 |
| sieve | 13.574 | 14.072 | 1.037 |
| sortw | 6.9629 | 8.3779 | 1.203 |
| strwork | 11.592 | 12.272 | 1.059 |
| tokenize | 2.6700 | 2.4155 | 0.905 |

geomean pliron/LLVM = **1.041**; faster on 5/16 (eqbug, fib, matmul,
nbody, tokenize); worst regressions: chars_alpha +27.4 %, sortw +20.3 %,
contains_rng +19.1 %. `strwork` dropped +41.6 % → +5.9 % after the
inlining work.

## 3. Wasm compile time — same lib, `--target wasm32-unknown-unknown --emit obj`

| path | median of 3 | vs LLVM |
|---|---|---|
| stock LLVM (wasm-ld) | 648 ms | 1.00× |
| pliron CLIF→waffle (default) | 2757 ms | 4.25× slower |
| pliron dialect→waffle (`PLIRON_WASM_CLIF=0`, since removed) | 6072 ms | 9.37× slower |

## 4. Wasm runtime — `bench/wasm/run.sh`, node v24.21, 9 samples/kernel

| kernel | LLVM (ms) | pliron CLIF | Δ vs LLVM | dialect | Δ vs LLVM |
|---|---|---|---|---|---|
| chars_alpha | 3.011 | 2.698 | −10.1 % | 5.198 | +70.5 % |
| chars_ascii | 1.267 | 1.901 | +48.7 % | 3.165 | +147.7 % |
| chars_mixed | 13.130 | 19.748 | +50.2 % | 13.863 | +5.8 % |
| contains_rng | 5.441 | 6.078 | +11.8 % | 10.720 | +95.9 % |
| eqbug | 0.119 | 0.113 | −5.7 % | 0.130 | +9.2 % |
| fib | 5.710 | 10.026 | +77.7 % | 18.039 | +217.2 % |
| hashmap | 46.340 | 71.735 | +53.2 % | 73.040 | +54.2 % |
| hmget | 9.031 | 14.063 | +55.6 % | 13.551 | +50.2 % |
| iterw | 58.063 | 35.419 | −39.4 % | 64.164 | +8.6 % |
| matmul | 26.612 | 23.468 | −11.9 % | 20.100 | −24.5 % |
| nbody | 73.385 | 81.018 | +10.0 % | 126.145 | +74.2 % |
| parse_only | 297.189 | 367.485 | +23.6 % | 347.666 | +16.5 % |
| sieve | 13.647 | 14.482 | +5.0 % | 14.831 | +9.0 % |
| sortw | 6.534 | 6.940 | +4.1 % | 8.071 | +14.6 % |
| strwork | 12.997 | 17.430 | +32.5 % | 22.473 | +71.9 % |
| tokenize | 3.300 | 4.065 | +23.4 % | 15.090 | +355.4 % |

geomean pliron/LLVM = **1.166**; dialect/LLVM = **1.553**; CLIF path beats
the dialect fallback on 14/16 (dialect wins only matmul & chars_mixed).
Module size: LLVM 97 KiB, pliron 813 KiB, dialect 1.08 MiB — wasm-ld
strips dead std code far more aggressively than pliron-wasm-ld does today.

### Post-fix (b50185e: winline caller-growth cap)

| metric | before | after |
|---|---|---|
| wasmbench.wasm (CLIF) | 813 KiB | **203 KiB** |
| wasmbench.wasm (dialect) | 1.08 MiB | **341 KiB** |
| wasm compile time (lib) | 2757 ms | **824 ms** (1.27x LLVM) |
| runtime geomean | 1.166 | **1.157** (unchanged) |

Root cause: `winline` rescanned callers for 8 rounds and each splice
exposed fresh callsites inside the copies — core's `quicksort` grew to
~344k wat lines (0.6 MB, ~75 % of the code section). Capping caller
growth at 2048 insts drops it to ~7.7k lines. No runtime cost: wasm
calls are cheap and the post-splice optimizer wasn't exploiting the
merges anyway.

## 5. CubeCL CPU JIT — `examples/throughput`, release build

### Runtime (best of 5 for compute, best of 3 for memory; µs for launch)

| bench | Cranelift JIT | LLVM LLJIT | ratio |
|---|---|---|---|
| compute f32 | 1.1378 TOPS/s | 1.1378 TOPS/s | 1.00 |
| compute f16 | 1.1378 TOPS/s | 1.1299 TOPS/s | ~1.00 |
| compute bf16 | 1.1378 TOPS/s | 1.1378 TOPS/s | ~1.00 |
| memory copy (1 GiB) | 35.53 GB/s | 38.46 GB/s | 0.92 (LLVM +8 %) |
| memory read (512 MiB) | 43.71 GB/s | 43.79 GB/s | ~1.00 |
| memory write (512 MiB) | 21.35 GB/s | 26.37 GB/s | 0.81 (LLVM +23 %) |
| launch overhead | 2.27 µs | 2.43 µs | ~1.00 (noise) |

Per-run spread on the hybrid CPU is large — e.g. clif f16 sampled
535 GOPS–1.14 TOPS across runs before pinning to best-of-N.

### Kernel compile time (`CUBECL_JIT_TIME=1`, µs/kernel, 17 kernels)

| JIT | total | mean | fastest | slowest |
|---|---|---|---|---|
| Cranelift | 155.5 ms | 9.1 ms | 2.9 ms | 32.9 ms |
| LLVM O3 | 421.9 ms | 24.8 ms | 11.3 ms | 47.3 ms |

Cranelift compiles each kernel **~2.7× faster** than LLVM's `-O3` pipeline.

### Correctness

`cubecl-cpu` test suite (release): Cranelift **1048/1051** pass — the 3
failures (`test_powi_constants` ×3) fail identically under
`CUBECL_CPU_JIT=llvm` (pre-existing cubecl polyfill bug, not backend).
LLVM LLJIT: **62 failures**, almost all bf16 (`identity`, `encode`,
`binary::*`) — a pre-existing Windows LLVM-path correctness problem;
bf16 tests all pass under Cranelift.

## 6. Isolated rechecks (microbench, same std, same process conditions)

criterion reported chars_alpha +28 % and strwork +42 % vs LLVM; isolated
binaries (best-of-15 ×10-iter) tell a different story — the regression is
partly harness/scheduler ordering on the hybrid part:

| kernel | LLVM | pliron | isolated ratio | criterion ratio |
|---|---|---|---|---|
| chars_alpha (3 M chars) | 2.10 ms | 1.97 ms | 0.94 (pliron faster) | 1.284 |
| strwork (4 k iters) | 1.07 ms | 1.15 ms | 1.07 | 1.416 |

Codegen diff on `count_alpha`: near-identical inner loops (~14 insns/char);
pliron emits 2 extra register-move edge copies per alpha char and keeps the
emulated-EH flag pointer live across the loop. The remaining real delta is
small; criterion's first-run-LLVM-then-pliron ordering on the hybrid
scheduler accounts for the rest.

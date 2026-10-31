# Parity card, 2026-10-30 (after the willreturn fix and the docs cleanup)

`harness/parity.py`, aarch64-apple-darwin, backend `3c43d21` (main), release
dylib for compile time, nightly-2026-10-22. Load 7–9 during the run (one core
was macOS XprotectService; no other compiler jobs), so SPD and CMP carry about
±3–5% noise. Memory, code size and stack don't depend on load. COR was then
recomputed (`--from-logs`) with fresh correctness inputs from the same
revision: `harness/crates.py --tier 1` (20 crates, default `--deps stock`)
and `harness/run.py --suite fuzz --fuzz-seeds 0:200`. The UI input is the
2026-10-27 stage-2 self-host run.

Against the first full card (2026-10-27, defaults, `2026-10-27-parity-card.md`):

| | SPD | CMP | MEM | SIZ | STK | COR | **overall** |
|---|---|---|---|---|---|---|---|
| 2026-10-27 defaults | 106 | 107 | 36 | 37 | 87 | 100 | **88** |
| 2026-10-30 defaults | 106 | 112 | 68 | 64 | 92 | 100 | **95** |

MEM and SIZ moved with the default-on flips of 2026-10-28 (memory set:
`LOWERFREE`, `JEARENA`, `CGU_ACAP=8`, `LTUNDEF`; size set: the wasm stack,
`JT_NARROW`, card-B leftovers) and later size work. Per input
(pliron / LLVM, time / peak RSS / machine code):

| input | 2026-10-27 | 2026-10-30 |
|---|---|---|
| kernels | 0.56 / 1.63 / 1.89 | 0.83 / 1.31 / 1.73 |
| kernels-wasm | 0.99 / 1.99 / 2.32 | 0.69 / 1.39 / 1.57 |
| regex-syntax | 1.08 / 2.82 / 2.06 | 0.97 / 1.58 / 1.73 |
| regex-wasm | 1.01 / 3.06 / 5.73 | 0.87 / 2.06 / 1.77 |
| syn | — | 0.93 / 1.54 / 1.45 |

## Cost of the willreturn fix (a8f0b06, c52dd57)

Loops and recursion now deny `willreturn` (callgvn was erasing calls to
side-effect-free functions that never return). Measured pre-fix `cd5865b`
against the fix, release dylibs:

- Native kernels: all 3,296 functions of the bench binary have identical
  machine code (`llvm-objdump`, addresses stripped). No runtime change is
  possible, so no runtime A/B was run.
- Output bytes (`ctime.py`, 2 interleaved rounds × 3): identical on
  kernels, kernels-wasm, regex-syntax and syn; regex-wasm +6 bytes. Peak
  RSS unchanged.
- Bodies keeping `willreturn` on `tests/std`: 32 → 16.
- Compile time: the first fix reused inline's `loop_blocks` (Tarjan per
  function) and took the `willreturn` phase from 7 to 22 ms CPU on
  regex-syntax (sum over CGUs). c52dd57 gathers CFG edges in the existing op
  walk and runs one early-exit DFS: 13 ms. Objects identical between the two
  (cgu=1, all five inputs). Whole-compile wall changes were within noise
  (≤1.5%).

## The run

## Parity card: **95** overall

| SPD | CMP | MEM | SIZ | STK | COR |
|---|---|---|---|---|---|
| **106** | **112** | **68** | **64** | **92** | **100** |

| category | weight | pliron/LLVM | rating | from |
|---|---|---|---|---|
| SPD runtime speed | 30 | 0.918 | 106 | native 0.917 · wasm 0.919 |
| CMP compile time | 15 | 0.853 | 112 | 5 inputs |
| MEM compile memory | 10 | 1.556 | 68 | peak RSS, 5 inputs |
| SIZ code size | 10 | 1.655 | 64 | machine code, 5 inputs + wasm module 1.71 |
| STK stack frames | 15 | 1.124 | 92 | regex-syntax, 327 shared fns |
| COR correctness | 20 | 1.000 | 100 | ui: 0 stage-2-only vs 3 stage-1-only (3 d old) · crates: 0 pliron-only across 20 crates (0 h old) · fuzz: 0 of 200 seeds (0 h old) |

Rating = 100 + 50·log2(LLVM/pliron), clamped to 0–150: 100 is parity, 150 twice as good, 50 half as good. Overall = weighted mean of ratings.

## Axes

| axis | pliron/LLVM |
|---|---|
| native runtime (geomean, 19 kernels) | 0.917 |
| wasm runtime (geomean, 16 kernels) | 0.919 |
| wasm module size | 1.71 (162 KiB vs 95 KiB) |
| compile time: kernels | 0.83 (0.38 s vs 0.46 s) |
| compile time: kernels-wasm | 0.69 (0.40 s vs 0.58 s) |
| compile time: regex-syntax | 0.97 (1.57 s vs 1.62 s) |
| compile time: regex-wasm | 0.87 (2.52 s vs 2.88 s) |
| compile time: syn | 0.93 (3.33 s vs 3.56 s) |

## Native runtime

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| arrayvec | 5.576 ms | 5.150 ms | 0.924 ◄ faster |
| chars_alpha | 2.094 ms | 2.014 ms | 0.962 ◄ faster |
| chars_ascii | 2.185 ms | 1.301 ms | 0.595 ◄ faster |
| chars_mixed | 15.527 ms | 15.371 ms | 0.990 |
| contains_rng | 2.189 ms | 1.720 ms | 0.786 ◄ faster |
| eqbug | 0.112 ms | 0.122 ms | 1.089 ▲ slower |
| fib | 14.395 ms | 13.072 ms | 0.908 ◄ faster |
| hashmap | 71.040 ms | 64.149 ms | 0.903 ◄ faster |
| hmget | 14.369 ms | 12.822 ms | 0.892 ◄ faster |
| iterw | 19.584 ms | 13.856 ms | 0.708 ◄ faster |
| matmul | 8.495 ms | 8.529 ms | 1.004 |
| nbody | 113.523 ms | 90.536 ms | 0.798 ◄ faster |
| parse_only | 367.568 ms | 391.640 ms | 1.065 ▲ slower |
| sieve | 12.320 ms | 11.887 ms | 0.965 ◄ faster |
| sortw | 6.841 ms | 6.368 ms | 0.931 ◄ faster |
| strwork | 13.876 ms | 13.951 ms | 1.005 |
| tokenize | 5.913 ms | 7.264 ms | 1.229 ▲ slower |
| varint | 6.372 ms | 6.448 ms | 1.012 |
| varint_postcard | 5.920 ms | 5.187 ms | 0.876 ◄ faster |

## Wasm runtime

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| chars_alpha | 4.635 ms | 4.008 ms | 0.866 ◄ faster |
| chars_ascii | 2.319 ms | 2.316 ms | 0.999 |
| chars_mixed | 18.381 ms | 18.227 ms | 0.993 |
| contains_rng | 4.924 ms | 3.939 ms | 0.804 ◄ faster |
| eqbug | 0.136 ms | 0.119 ms | 0.877 ◄ faster |
| fib | 9.352 ms | 8.862 ms | 0.940 ◄ faster |
| hashmap | 66.195 ms | 70.189 ms | 1.061 ▲ slower |
| hmget | 11.473 ms | 11.509 ms | 1.003 |
| iterw | 66.800 ms | 53.491 ms | 0.802 ◄ faster |
| matmul | 30.773 ms | 21.701 ms | 0.705 ◄ faster |
| nbody | 84.100 ms | 87.151 ms | 1.036 ▲ slower |
| parse_only | 380.972 ms | 389.002 ms | 1.021 |
| sieve | 13.941 ms | 14.024 ms | 1.006 |
| sortw | 6.042 ms | 5.630 ms | 0.930 ◄ faster |
| strwork | 17.729 ms | 19.087 ms | 1.078 ▲ slower |
| tokenize | 7.674 ms | 5.495 ms | 0.714 ◄ faster |

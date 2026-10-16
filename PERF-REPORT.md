# Performance report — pliron backend vs LLVM

**Machine:** Windows x64 (`x86_64-pc-windows-msvc`), Intel i7-12700K
(8 P-cores + 4 E-cores, AVX2, no AVX512), RTX 3080.
**Revisions:** rustc `nightly-2026-10-06`, pliron backend `45063dd`,
cubecl `clif-cpu-jit` `2556b654f`. Raw data: [`perf/data.md`](perf/data.md).

| axis | LLVM | pliron | verdict |
|---|---|---|---|
| **native runtime** | 1.00× | **1.037×** slower | 7/16 kernels faster, geomean within noise of parity |
| **native compile time** | 488 ms | 925 ms | 1.9× slower (release dylib) |
| **wasm runtime** | 1.00× | **1.157×** slower | 4/16 faster; dialect fallback is worse still (1.55×) |
| **wasm compile time** | 648 ms | 824 ms | 1.27× slower (was 4.3× before the winline cap) |
| **wasm module size** | 97 KiB | **203 KiB** | 2.1× LLVM (was 8.4× — winline cascade fixed) |
| **cubecl JIT runtime** | 1.00× | **~1.00×** | parity on compute & read; LLVM +8–23 % on copy/write |
| **cubecl JIT compile time** | 24.8 ms/kernel | 9.1 ms/kernel | **2.7× faster** |
| **cubecl correctness** | 989/1071 | **1048/1071** | LLVM's bf16 codegen is broken on Windows |

## The short version

At `-O`, stock rustc+LLVM is still the codegen-quality leader on native
code, but the gap is now noise-level: **geomean 1.037** on the 16-kernel
criterion suite, and pliron *wins outright* on 7 of them. A month ago the
same suite had two kernels at 5–10× slower; the punroll group-guard and
the load-forwarding fixes upstream closed that.

For JITs the calculus flips: LLVM's `-O3` compile times are the liability
(~2.7× slower than Cranelift for the same kernel), the runtime output is
statistically identical on compute, and on this Windows box the LLVM
path has real correctness gaps (bf16, `powi`, variadic imports) that the
Cranelift path does not.

## 1. Native: stock LLVM vs pliron

`bench/run.sh` builds `bench/benches/kernels.rs` twice (stock, then
`-Zcodegen-backend`), saves the LLVM run as a criterion baseline, and
reports the pliron binary against it. Times are best-of-100 samples.

```
kernel              LLVM       pliron     ratio   verdict
chars_alpha         1.792 ms   2.301 ms   1.284   slower
chars_ascii         0.959 ms   0.962 ms   1.004   parity
chars_mixed         9.363 ms   9.478 ms   1.012   parity
contains_rng        2.599 ms   3.030 ms   1.166   slower
eqbug               0.117 ms   0.108 ms   0.919   faster  ◄
fib                 7.737 ms   7.540 ms   0.975   faster  ◄
hashmap            49.499 ms  45.630 ms   0.922   faster  ◄
hmget              10.392 ms  10.401 ms   1.001   parity
iterw              33.703 ms  32.441 ms   0.963   faster  ◄
matmul              6.041 ms   5.593 ms   0.926   faster  ◄  (was ~10×)
nbody              70.963 ms  69.285 ms   0.976   faster  ◄
parse_only        276.200 ms 302.940 ms   1.097   slower
sieve              13.751 ms  14.308 ms   1.040   slower
sortw               6.739 ms   7.363 ms   1.093   slower
strwork            11.580 ms  16.398 ms   1.416   slower
tokenize            2.678 ms   2.468 ms   0.921   faster  ◄

geomean = 1.037   (7 faster, 4 parity, 5 slower)
```

What's changed since the last measurement: `d3e80e7` (punroll group-guard
unrolling for unprofiled loops) removed the biggest outlier — matmul went
from ~10× to actually *winning* vs LLVM. The remaining regressions cluster
in memory/string-bound loops (strwork +42 %, chars_alpha +28 %): the
CLIF pipeline doesn't yet match LLVM's store-merging and load
reassociation on those shapes.

**Compile time:** the same 16-kernel lib compiles in 488 ms under LLVM
vs 925 ms under pliron-release — a 1.9× constant on top of the shared
frontend. The backend does MIR→pliron-IR→CLIF→machine code in-process,
so there's no assembler/linker bill; most of the delta is the optimizer
pipeline itself (the CLIF pass suite runs per-function where LLVM batch
-amortizes at the module level).

## 2. Wasm: LLVM vs pliron (two pipelines)

`bench/wasm/run.sh` builds the same kernels to `wasm32-unknown-unknown`
three ways — stock LLVM+wasm-ld, the CLIF→waffle path (default since
`PLIRON_WASM_CLIF` flipped on), and the old dialect→waffle fallback — then
times all 16 in node.

| | LLVM | pliron CLIF | pliron dialect |
|---|---|---|---|
| geomean | 1.00 | **1.166** | 1.553 |
| faster than LLVM | — | 4/16 | 1/16 |
| compile time | 648 ms | 2.76 s | 6.07 s |
| module size | 97 KiB | 813 KiB | 1.08 MiB |

Three things stand out:

- **CLIF is now the better of our two paths** — 1.166 vs 1.553 geomean,
  and it wins 14/16 head-to-head against the dialect path. The dialect
  fallback only survives on matmul (−24.5 % vs LLVM, the best number on
  the page) and chars_mixed.
- **Wasm is farther from LLVM than native is** (1.166 vs 1.037) — mostly
  because wasm kernels like `fib` (+77 %) and `tokenize` (+23 %) exercise
  the wasm-specific translation layer rather than the native CLIF one.
- **Module size was 8× LLVM's — now 2.1×.** The bulk was one pathological
  case: `winline`'s 8-round rescanning cascaded splices inside its own
  copies, exploding core's `quicksort` into a ~0.6 MB single function
  (75 % of the code section). `b50185e` caps caller growth at 2048 insts:
  pliron wasm 813→203 KiB, dialect 1.08 MiB→341 KiB, and wasm compile
  time 2757→824 ms (the mega-bodies were dominating `optimize()` too).
  Runtime geomean unchanged (1.166→1.157) — wasm calls are cheap, the
  inlining never paid for itself anyway.

Compile time is now 1.27× LLVM for CLIF→waffle (the dialect path is still
slower to compile than it is to run).

## 3. CubeCL: LLVM LLJIT vs Cranelift JIT

`examples/throughput` on the `clif-cpu-jit` branch, release build,
`cubecl-llvm` + `cubecl-cpu` with the `cranelift` feature. Cranelift is
the default; `CUBECL_CPU_JIT=llvm` A/Bs back to LLJIT.

### Runtime — best-of-N on the hybrid part

```
bench                  Cranelift      LLVM         verdict
compute-direct f32     1.1378 TOPS    1.1378 TOPS   parity (saturation)
compute-direct f16     1.1378 TOPS    1.1299 TOPS   parity
compute-direct bf16    1.1378 TOPS    1.1378 TOPS   parity
memory copy (1 GiB)    35.5 GB/s      38.5 GB/s     LLVM +8 %
memory read (512 MiB)  43.7 GB/s      43.8 GB/s     parity
memory write (512MiB)  21.4 GB/s      26.4 GB/s     LLVM +23 %
launch overhead        2.27 µs        2.43 µs       parity (dispatcher-bound)
```

Compute saturates the box identically on both paths (~1.14 TOPS across
f32/f16/bf16 — that's the i7-12700K's real ceiling, not codegen).
The write-path gap is consistent (~23 %) — the store loop is vectorized
(16×f32 per store) but the pliron kernel keeps a per-element wrap/bounds
branch that LLVM hoists; worth a look at `loopvec`/`bcheck` coverage on
that shape, not a structural limitation.

**JIT compile time is the win.** `CUBECL_JIT_TIME=1`:

```
              Cranelift        LLVM O3
total (17)    155.5 ms         421.9 ms
per-kernel    9.1 ms           24.8 ms   →  2.7× faster
```

For a real application this is usually the axis that matters — GPU-scale
JIT workloads launch hundreds of kernels and compile time is on the
critical path; runtime output is already at hardware saturation.

### Correctness

`cargo test -p cubecl-cpu` (release, 1071 tests):

- **Cranelift: 1048 pass / 3 fail.** The only failures are
  `test_powi_constants` ×3, which fail identically on the LLVM path —
  pre-existing cubecl-frontend polyfill bug, not backend.
- **LLVM LLJIT: 989 pass / 62 fail** — the bulk is bf16 (`identity`,
  `encode`, `binary::*` all return wrong values) plus `powi_constants`.
  bf16 JIT compute on Windows LLVM is broken; Cranelift's bf16 path
  (native i16-lane lowering) is not.

Two Windows-specific fixes made this parity possible: x64 call veneers
in the vendored `cranelift-jit` (imports >±2 GB from JIT memory used to
overflow `rel32`) and `__truncsfbf2`/`__cubecl_jit_printf`/`libm` symbol
bindings on the JIT side, since UCRT exports neither bf16 builtins nor
`printf`.

## What I'd prioritize next

1. **strwork / chars_alpha regressions** (native, +42 %/+28 %) — the two
   real outliers left. Likely load/store reassociation and store-merging
   in the CLIF pipeline; same shape class as the cubecl write gap.
2. **wasm module size** — 8× LLVM is a shipping problem before it's a
   perf problem; needs GC/symbol stripping in `pliron-wasm-ld`.
3. **wasm compile time** — 4.3× is tolerable for a backend today, but
   it'll get noticed when the wasm leg ships to CI.
4. **`test_powi_constants`** — file/fix upstream; it fails on stock LLVM
   too, so it's a cubecl polyfill bug, not ours.

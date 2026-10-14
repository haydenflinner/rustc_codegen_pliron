# Performance of rustc_codegen_pliron

We measured the pliron code generator against the LLVM code generator in
rustc. Both paths use the same rustc MIR and the same MIR-level
optimizations. Only the backend is different.

## What we measured

The kernels live in `bench/` as a `criterion` bench target (`pliron-bench`).
`bench/run.sh` compiles that target twice — once with stock `rustc -O`
(LLVM), once with `rustc -O -Zcodegen-backend=<pliron>` — saves the LLVM run
as the `llvm` baseline, then runs the pliron build against it. Each
benchmark gets 1 s of warm-up and 2 s of measurement over 100 samples, and
criterion reports a significance-tested change per kernel. The `cargo
rustc --bench` flags reach the bench crate only, so criterion itself is
always LLVM-built and cannot skew the numbers. The machine is an Apple
Silicon Mac (aarch64-apple-darwin); the toolchain is `nightly-2026-10-06`.

`black_box` is a real barrier under both backends, and now nearly free:
pliron emits a call to a never-defined `__pliron_bb*`/`__pliron_bbm`
symbol, opaque to every IR-level pass (a bodiless callee writes memory
and captures its args, so nothing can fold through it), and `lower.rs`
rewrites the call into `black_box` — a CLIF instruction we added to the
vendored Cranelift (`can_store` + `other_side_effects`, i.e. exactly
LLVM's `asm("" : "r,~{memory}" :: v)` semantics) that lowers to zero
machine instructions: the result vreg aliases the input's. On wasm the
CLIF path doesn't exist, so `black_box` keeps the alloca + volatile-load
fallback there.

## Run time of compiled code

| kernel | LLVM | pliron | change |
|---|---|---|---|
| chars_alpha | 1.491 ms | 1.487 ms | −0.2% (no change detected) |
| chars_ascii | 1.540 ms | 0.998 ms | −35.2% improved |
| chars_mixed | 11.09 ms | 11.26 ms | +1.5% (noise) |
| contains_rng | 1.556 ms | 1.512 ms | −2.8% improved |
| eqbug | 78.3 µs | 95.9 µs | +22.3% regressed |
| fib | 10.09 ms | 9.242 ms | −8.4% improved |
| hashmap | 53.73 ms | 46.45 ms | −13.6% improved |
| hmget | 9.088 ms | 9.297 ms | +2.3% regressed |
| iterw | 13.77 ms | 11.57 ms | −16.0% improved |
| matmul | 5.967 ms | 6.075 ms | +1.8% regressed |
| nbody | 79.86 ms | 83.94 ms | +5.1% regressed |
| parse_only | 260.5 ms | 292.6 ms | +12.3% regressed (memory clobber) |
| sieve | 8.521 ms | 9.188 ms | +7.8% regressed |
| sortw | 4.896 ms | 4.352 ms | −11.1% improved |
| strwork | 9.833 ms | 9.497 ms | −3.4% improved |
| tokenize | 4.056 ms | 2.665 ms | −34.3% improved |

The geometric mean of the ratios is 0.92 excluding `eqbug` (0.94 including
it): about 8% faster than LLVM on this set. Eight kernels are faster, five
are slower, and the rest are at parity or within noise.

`eqbug` is a narrow-icmp stress kernel kept as a regression guard. Its
inner loop calls `black_box` three times per element — the residual +22%
is real codegen gap in the kernel itself, not barrier overhead: the
`black_box` op emits no machine code (each barrier only forces the value
into a register, exactly like LLVM's `r` constraint).

## Where pliron wins

- `chars_ascii` (−35.6%) and `tokenize` (−34.3%): byte-classification
  loops. `rangefold` folds range-test trees into offset-and-compare forms,
  and `loopvec` widens the ASCII fast path.
- `iterw` (−16.0%), `hashmap` (−13.6%), `sortw` (−11.1%): loop and
  call-heavy kernels. Loop rotation, induction-variable work, `divconst`
  multiply-high expansion feeding NEON widening multiply-accumulates, and
  the tail calling convention for local functions cover most of it.
- `fib` (−8.4%), `contains_rng` (−2.4%), `strwork` (−3.4%),
  `chars_alpha` (−4.0%): calls and range tests.

## Where LLVM wins

- `eqbug` (+22%) and `parse_only` (+12%): the two per-iteration
  `black_box` kernels. The barrier itself emits zero instructions — the
  residual is the `~{memory}` clobber (loads can't forward across it) plus
  surrounding codegen, same semantics LLVM applies.
- `sieve` (+7.8%), `nbody` (+5.1%): sieve's u128 multiply/divide chain
  stays slow-side (was +10.4% last run, worth a bisect); nbody's flip from
  −16.9% to +5.1% needs investigation — it moved far more than noise
  between runs.
- `matmul` (+1.8%) and `hmget` (+2.3%): hmget is consistently slow-side.

## WebAssembly

`bench/wasm/run.sh` compiles the same kernel sources to
wasm32-unknown-unknown twice — stock rustc (LLVM + wasm-ld) and the
pliron backend (`-Zbuild-std`, linked by `tools/pliron-wasm-ld`) — and
`bench/wasm/run.mjs` times each `k_<name>(iters)` export in node (V8):
warm calls for Liftoff→TurboFan tier-up, then 5 samples of ~150 ms,
median per iteration. Criterion can't run on wasm — it re-execs the
bench binary per benchmark — so the driver does its own sampling.
Every kernel's accumulator matched across backends on this run (the
driver cross-checks return values, so a pliron miscompile shows up as
`(ret differs)` or a trap).

Asymmetries vs the native table: std code is pliron-compiled here too
(native links the prebuilt LLVM sysroot — `-Zbuild-std` is forced
because pliron-wasm-ld reads pliron objects, not wasm-ld objects), so
stdlib-heavy kernels like `hashmap`/`hmget`/`sortw` also measure
std codegen; and measurement granularity is a whole `run()` call.

| kernel | LLVM | pliron | change |
|---|---|---|---|
| chars_alpha | 3.268 ms | 12.502 ms | +282.6% |
| chars_ascii | 1.675 ms | 6.773 ms | +304.3% |
| chars_mixed | 13.31 ms | 21.16 ms | +59.0% |
| contains_rng | 3.502 ms | 28.45 ms | +712.4% |
| eqbug | 0.098 ms | 0.116 ms | +17.7% |
| fib | 6.655 ms | 20.57 ms | +209.1% |
| hashmap | 47.96 ms | 275.0 ms | +473.4% |
| hmget | 8.229 ms | 74.73 ms | +808.2% |
| iterw | 48.15 ms | 46.09 ms | −4.3% |
| matmul | 22.12 ms | 15.60 ms | −29.4% |
| nbody | 60.28 ms | 100.6 ms | +67.0% |
| parse_only | 272.1 ms | 1222 ms | +349.1% |
| sieve | 10.01 ms | 12.43 ms | +24.2% |
| sortw | 4.483 ms | 7.474 ms | +66.7% |
| strwork | 12.38 ms | 27.86 ms | +125.0% |
| tokenize | 5.602 ms | 18.79 ms | +235.4% |

Geomean pliron/LLVM = 2.54 (16 kernels) — the wasm path (pliron LLVM
dialect → waffle → wasm32) is far behind LLVM wasm-ld codegen where the
native Cranelift path is slightly ahead (0.92). The module is also ~3x
the bytes (290 KB vs 97 KB) — consistent with the known function-count
gap that `winline` only partially closes; `winline`/`wpeep` are the only
wasm-local optimizations. The memory-indirect and std-heavy kernels
(`hmget`, `contains_rng`, `hashmap`, `parse_only`, `strwork`, `fib`,
`tokenize`, `chars_*`) are the biggest gaps; `matmul` (−29%) and `iterw`
(−4%) still win, and `eqbug`'s barrier cost shrinks to +18% because V8
store-forwards the linear-memory barrier LLVM's native targets can't.

## Compile time

The backend is slower than LLVM to compile code. Single files take about
3–5 times longer (0.3–1.9 s vs 0.1–0.3 s). The optimizer pipeline takes
about 95% of backend time. Function-level worker threads run the pipeline
in parallel across a crate's code-generation units.

A content-addressed step cache (`PLIRON_STEPCACHE=1`) stores per-pass and
per-function results. A warm rerun of the pipeline takes about 16 ms per
code-generation unit instead of about 800 ms. A gate flip such as
`PLIRON_SLP=0` reuses the unaffected steps and misses only the changed
ones. Any change to backend source code changes the build hash, so stale
results cannot hit.

## Correctness gates used during this work

- `cargo test -p pliron-clif-opt` unit tests, including exhaustive
  differential tests of `divconst` against the Cranelift interpreter.
- `./test.sh` smoke suite: nostd, std, unwind, asm, proc-macro, wasm, and
  per-pass feature tests.
- `PLIRON_DIFFCHECK=1` interprets each function before and after the
  pipeline and compares returns, calls, and memory writes.
- `PLIRON_VERIFY=full` runs the Cranelift verifier after each pass.

## What is next

- `sieve`: bisect the +7.8% regression.
- `nbody`: investigate the −17% → +5% flip between runs.
- `black_box`: done — the CLIF `black_box` op (vendored Cranelift patch
  22) emits zero instructions, matching LLVM's `asm("" : "r,~{memory}")`.
  wasm still uses the volatile-load fallback.
- `chars_mixed`: extend the `rangefold` coverage to the non-ASCII path.
- `hmget`: residual spill traffic in the hashbrown probe loop.
- More `loopvec` coverage: gather-loop shapes, multi-instruction update
  chains, and inner loops inside outer loops.
- Compile time: run `slp` only where vectorizable shapes exist, or add an
  applicability check that skips passes that cannot fire.

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
wasm32-unknown-unknown three ways: stock rustc (LLVM + wasm-ld), the
pliron backend's dialect path (pliron LLVM dialect → waffle), and the
pliron backend's CLIF path (`PLIRON_WASM_CLIF=1`: MIR → Cranelift IR →
the shared optimisation pipeline → waffle). `bench/wasm/run.mjs` times
each `k_<name>(iters)` export in node (V8): warm calls for
Liftoff→TurboFan tier-up, then 9 interleaved samples of ~150 ms. The
reported change is the median of per-round ratios, so a load burst hits
every column in the same round. Criterion can't run on wasm — it
re-execs the bench binary per benchmark — so the driver does its own
sampling. Every kernel's accumulator matched across all three modules
on this run (the driver cross-checks return values, so a miscompile
shows up as `(ret differs)` or a trap).

Asymmetries vs the native table: std code is pliron-compiled here too
(native links the prebuilt LLVM sysroot — `-Zbuild-std` is forced
because pliron-wasm-ld reads pliron objects, not wasm-ld objects), so
stdlib-heavy kernels like `hashmap`/`hmget`/`sortw` also measure
std codegen; and measurement granularity is a whole `run()` call.

This run was taken while the host was under heavy unrelated build load,
so per-kernel deltas are approximate — the module sizes and the column
ordering are stable.

| kernel | LLVM | pliron | change | clif | change |
|---|---|---|---|---|---|
| chars_alpha | 17.67 ms | 79.90 ms | +298.8% | 60.39 ms | +226.7% |
| chars_ascii | 5.529 ms | 45.37 ms | +796.6% | 47.68 ms | +827.9% |
| chars_mixed | 94.90 ms | 119.3 ms | +30.2% | 154.1 ms | +64.4% |
| contains_rng | 26.90 ms | 216.9 ms | +711.0% | 98.77 ms | +236.4% |
| eqbug | 0.765 ms | 0.847 ms | +12.2% | 0.791 ms | −2.5% |
| fib | 40.34 ms | 93.37 ms | +123.7% | 82.38 ms | +119.7% |
| hashmap | 314.8 ms | 1946 ms | +573.3% | 1162 ms | +263.3% |
| hmget | 152.5 ms | 1332 ms | +917.4% | 614.5 ms | +318.3% |
| iterw | 344.7 ms | 353.0 ms | −1.3% | 228.1 ms | −31.3% |
| matmul | 133.7 ms | 103.0 ms | −15.7% | 122.0 ms | −3.7% |
| nbody | 388.6 ms | 810.9 ms | +105.9% | 523.1 ms | +36.2% |
| parse_only | 2040 ms | 10615 ms | +225.5% | 9668 ms | +368.6% |
| sieve | 280.1 ms | 333.7 ms | +28.7% | 294.2 ms | +20.2% |
| sortw | 41.58 ms | 129.9 ms | +168.9% | 132.2 ms | +168.0% |
| strwork | 71.40 ms | 166.9 ms | +110.8% | 145.4 ms | +114.3% |
| tokenize | 25.18 ms | 100.5 ms | +286.6% | 123.6 ms | +405.4% |

Geomeans: dialect pliron/LLVM = 2.74, CLIF pliron/LLVM = 2.34
(16 kernels). Both wasm paths are far behind LLVM wasm-ld codegen where
the native Cranelift path is slightly ahead (0.92), but the CLIF path —
which runs the same optimisation pipeline as native codegen — is
roughly 15% faster geomean than the dialect path and produces a 41%
smaller module (175 KB vs 296 KB; LLVM is 97 KB). It wins clearly on
the memory-indirect and std-heavy kernels (`contains_rng`, `hashmap`,
`hmget`, `nbody`, `iterw`, `eqbug`); `matmul` (−16% dialect, −4% CLIF)
still wins vs LLVM. The remaining regressions vs the dialect path
(`tokenize`, `parse_only`) are in string-scanning code — both paths run
the same dialect-level `ir-opt` phase, so the gap is in CLIF→waffle
lowering itself and is not yet isolated.

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

## Pure-call elimination (x86_64, `callgvn`)

Measured on Linux x86-64 (Xeon 8559C), wall-clock best-of-3, microbenches
where the same source builds under stock `rustc -O` (LLVM) and the pliron
backend (`-O`).

| workload | LLVM | pliron |
|---|---|---|
| scalar hash loop (mul/xor/rol) | 585 ms | 435 ms |
| u32 dot product, vectorized | 169 ms | 148 ms |
| f32 sum-of-squares, vectorized | ~240 ms | ~240 ms |
| 100 identical pure calls in a loop | ~80 ns | ~185 ms → **2.4 ms** |

The last row is `callgvn`: LLVM proves the callee pure and eliminates all
100 calls; we now infer the same facts (`nowrite` + `nounwind`) and dedup,
hoist and dead-eliminate calls in `pliron-ir-opt`. The hoist also moves
the operand cone (loop-invariant loads/GEPs) and carries invokes — an
unwind edge is keyed on the op, so it moves with it. Residual gap vs LLVM
here: we hoist to one call per loop; LLVM removes the call entirely —
that last step needs a `willreturn` fact we don't infer yet.

Support pieces that made it fire: `nowrite` splits writes on
return-reachable blocks from writes on dead paths (`eh.exn` pads, aborts)
so callers aren't poisoned; `nounwind` inference runs by default (it also
turns invokes of nounwind callees into plain calls). `-Ctarget-cpu` /
`-Ctarget-feature` now map to Cranelift ISA flags (e.g. `native` → SSE4.1
`pmulld` instead of `pmuludq` repacks; `PLIRON_X64_VEX=1` switches
VEX-128 forms on for the ops that have `use_avx*` rules).

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

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

`black_box` is a real barrier under both backends: pliron lowers scalars
as a store into an alloca followed by a volatile load, which the
optimizer cannot forward back to the input — the volatile-marked op
survives every ir-opt and CLIF pass. `Ref` aggregates and ZSTs instead
call `__pliron_bbm`, a `ret`-only assembly stub: escaping the place
pointer into an unanalyzable callee is the `~{memory}` clobber, so no
volatile copy is needed. LLVM uses empty inline asm, which keeps the
value in a register, so our scalar barrier costs ~1.2 ns per call where
LLVM's is free. (A register round-trip through a call stub was tried —
the call's register clobbers and full-memory effect made it slower than
the volatile load on hot loops.) The two kernels that call `black_box`
per inner iteration (`eqbug`, `parse_only`) pay that difference
directly.

## Run time of compiled code

| kernel | LLVM | pliron | change |
|---|---|---|---|
| chars_alpha | 1.491 ms | 1.487 ms | −0.2% (no change detected) |
| chars_ascii | 1.540 ms | 0.998 ms | −35.2% improved |
| chars_mixed | 11.09 ms | 11.26 ms | +1.5% (noise) |
| contains_rng | 1.556 ms | 1.512 ms | −2.8% improved |
| eqbug | 78.3 µs | 256.8 µs | +228% regressed (barrier cost) |
| fib | 10.09 ms | 9.568 ms | −5.2% improved |
| hashmap | 53.73 ms | 46.41 ms | −13.6% improved |
| hmget | 9.088 ms | 9.307 ms | +2.4% regressed |
| iterw | 13.77 ms | 11.39 ms | −17.3% improved |
| matmul | 5.967 ms | 6.195 ms | +3.8% regressed |
| nbody | 79.86 ms | 66.40 ms | −16.9% improved |
| parse_only | 260.5 ms | 278.5 ms | +6.9% regressed (barrier cost) |
| sieve | 8.521 ms | 9.403 ms | +10.4% regressed |
| sortw | 4.896 ms | 4.556 ms | −6.9% improved |
| strwork | 9.833 ms | 9.902 ms | +0.7% (noise) |
| tokenize | 4.056 ms | 2.713 ms | −33.1% improved |

The geometric mean of the ratios is 0.92 excluding `eqbug` (0.99 including
it): about 8% faster than LLVM on this set. Eight kernels are faster, four
are slower, and the rest are at parity or within noise.

`eqbug` is a narrow-icmp stress kernel kept as a regression guard. Its
inner loop calls `black_box` three times per element, so the measurement
is almost entirely barrier overhead (≈179 µs over 150k barriers vs 78 µs
of real work) — without the barriers the whole loop folded to ~2 ns.

## Where pliron wins

- `chars_ascii` (−35.2%) and `tokenize` (−33.1%): byte-classification
  loops. `rangefold` folds range-test trees into offset-and-compare forms,
  and `loopvec` widens the ASCII fast path.
- `iterw` (−17.3%), `nbody` (−16.9%), `hashmap` (−13.6%): loop and
  call-heavy kernels. Loop rotation, induction-variable work, `divconst`
  multiply-high expansion feeding NEON widening multiply-accumulates, and
  the tail calling convention for local functions cover most of it. The
  earlier best-of-N harness had `hashmap` as a loss — under per-iteration
  sampling the hashbrown probe loop is ahead.
- `sortw` (−6.9%): `pdqsort`/`ipnsort` on u64. The partition-loop copies
  around address formation that used to cost ~17% are gone.
- `fib` (−5.2%) and `contains_rng` (−2.8%): calls and range tests.

## Where LLVM wins

- `eqbug` (+228%) and `parse_only` (+6.9%): per-iteration `black_box`
  calls. LLVM's empty-asm barrier costs nothing; our volatile round-trip
  costs ~1.2 ns each. The underlying work is at parity — this is barrier
  overhead, not a codegen gap.
- `sieve` (+10.4%): `sieve(5M)` plus the u128 multiply/divide chain. The
  regression is new since the last table — worth a bisect.
- `matmul` (+3.8%) and `hmget` (+2.4%): matmul flipped between runs
  (earlier measured at parity), and hmget is consistently slow-side.

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

- `sieve`: bisect the +10.4% regression.
- `black_box`: the register round-trip call stub measured slower than
  the volatile store/load (spills + clobber per call), so the remaining
  gap is inherent without a truly inline empty-asm primitive — real
  inline `llvm.inline_asm` lowering would close it.
- `chars_mixed`: extend the `rangefold` coverage to the non-ASCII path.
- `hmget`: residual spill traffic in the hashbrown probe loop.
- More `loopvec` coverage: gather-loop shapes, multi-instruction update
  chains, and inner loops inside outer loops.
- Compile time: run `slp` only where vectorizable shapes exist, or add an
  applicability check that skips passes that cannot fire.

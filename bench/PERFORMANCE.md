# Performance of rustc_codegen_pliron

We measured the pliron code generator against the LLVM code generator in
rustc. Both paths use the same rustc MIR and the same MIR-level
optimizations. Only the backend is different.

## What we measured

We built each kernel in `bench/` twice: once with stock rustc `-O` (LLVM),
once with `rustc -O -Zcodegen-backend=<pliron>`. `bench/run.sh` reports the
best run time of 5 repetitions and refuses to time a binary whose compile
failed. The machine is an Apple Silicon Mac (aarch64-apple-darwin). The
rustc toolchain is `nightly-2026-10-06`.

## Run time of compiled code

| kernel | LLVM (ms) | pliron (ms) | ratio |
|---|---|---|---|
| chars_alpha | 1.49 | 1.45 | 0.97 |
| chars_ascii | 1.53 | 1.00 | 0.65 |
| chars_mixed | 11.23 | 11.11 | 0.99 |
| contains_rng | 1.50 | 1.31 | 0.88 |
| eqbug | 9983 | 9983 | 1.00 |
| fib | 10.09 | 9.31 | 0.92 |
| hashmap | 38.47 | 41.60 | 1.08 |
| hmget | 7.08 | 7.59 | 1.07 |
| iterw | 13.84 | 11.67 | 0.84 |
| matmul | 6.04 | 5.99 | 0.99 |
| nbody | 80.99 | 65.10 | 0.80 |
| parse_only | 262.8 | 265.3 | 1.01 |
| sieve | 8.56 | 8.64 | 1.01 |
| sortw | 4.79 | 5.60 | 1.17 |
| strwork | 9.96 | 10.25 | 1.03 |
| tokenize | 4.02 | 2.65 | 0.66 |

The geometric mean of the ratios is 0.93. The generated code is about 7%
faster than LLVM's on this set. Six kernels are faster, three are slower,
and the rest are at parity.

## Where pliron wins

- `chars_ascii` (0.65) and `tokenize` (0.66): byte-classification loops.
  `rangefold` folds range-test trees into offset-and-compare forms, and
  `loopvec` widens the ASCII fast path.
- `nbody` (0.80) and `iterw` (0.84): loop and call-heavy kernels. Loop
  rotation, induction-variable work, `divconst` multiply-high expansion
  feeding NEON widening multiply-accumulates, and the tail calling
  convention for local functions cover most of the win.
- `contains_rng` (0.88) and `fib` (0.92): range tests and calls.
- `chars_alpha` (0.97): `char::is_alphabetic`'s `('a'..='z') ∪ ('A'..='Z')`
  brif chain. `rangefold` now matches a lo-test in either branch polarity
  and accepts a *congruent* miss destination (identical-body twin block,
  `miss_twin`), so the chain bit-partitions into LLVM's `c | 0x20` masked
  compare.

## Where LLVM wins

- `sortw` (1.17): `pdqsort`/`ipnsort` on u64. The partition loops keep
  residual copies before address formation (`mov x13, x22; add x8, x13,
  #0x8`) and miss post-index load/store fusion that LLVM gets on the
  cursor walks. Earlier measurement noise read this as ~1.0.
- `hashmap` (1.08) and `hmget` (1.07): the hashbrown probe loop. Register
  allocation adds spill traffic in `reserve_rehash_inner`; group-byte
  probing is otherwise lowered correctly (`mov_from_vec` on `i8x8`
  bitcasts).

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

- `sortw`: eliminate residual copies around address formation and get
  post-index addressing on the partition cursor walks.
- Register-allocation spill traffic for `hashmap`/`hmget`.
- More `loopvec` coverage: gather-loop shapes, multi-instruction update
  chains, and inner loops inside outer loops.
- Compile time: run `slp` only where vectorizable shapes exist, or add an
  applicability check that skips passes that cannot fire.

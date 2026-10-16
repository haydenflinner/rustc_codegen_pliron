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
| chars_alpha | 3.51 ms | 3.33 ms | −5.0% (noise, p=0.38) |
| chars_ascii | 2.21 ms | 2.22 ms | +0.4% (noise) |
| chars_mixed | 18.11 ms | 20.34 ms | +12.3% regressed |
| contains_rng | 4.85 ms | 4.47 ms | −7.8% (p=0.06) |
| eqbug | 208 µs | 207 µs | −5.2% (noise) |
| fib | 10.12 ms | 11.50 ms | +13.7% regressed |
| hashmap | 102.1 ms | 85.97 ms | −15.8% improved |
| hmget | 18.86 ms | 18.43 ms | −2.3% (p=0.07) |
| iterw | 66.59 ms | 80.79 ms | +21.3% regressed |
| matmul | 12.10 ms | 10.08 ms | −16.7% improved |
| nbody | 152.7 ms | 170.6 ms | +11.7% regressed |
| parse_only | 639.5 ms | 543.1 ms | −15.1% improved |
| sieve | 22.33 ms | 24.68 ms | +10.5% (p=0.06) |
| sortw | 8.92 ms | 10.59 ms | +18.8% regressed |
| strwork | 15.71 ms | 16.79 ms | +6.9% regressed |
| tokenize | 6.24 ms | 4.39 ms | −29.6% improved |
||||||| parent of a4c036c (docs: fresh native/wasm tables; CubeCL gemm_cpu comparison)
| chars_alpha | 2.135 ms | 2.162 ms | +1.2% (noise) |
| chars_ascii | 2.204 ms | 1.550 ms | −29.7% improved |
| chars_mixed | 15.79 ms | 15.88 ms | +0.6% (noise) |
| contains_rng | 2.204 ms | 2.148 ms | −2.5% improved |
| eqbug | 112.4 µs | 119.9 µs | +6.7% regressed |
| fib | 14.73 ms | 13.31 ms | −9.6% improved |
| hashmap | 77.05 ms | 66.45 ms | −13.7% improved |
| hmget | 12.81 ms | 13.14 ms | +2.5% regressed |
| iterw | 19.84 ms | 15.99 ms | −19.4% improved |
| matmul | 8.64 ms | 8.64 ms | +0.0% (parity) |
| nbody | 117.3 ms | 120.7 ms | +2.9% regressed |
| parse_only | 371.1 ms | 366.4 ms | −1.1% (parity) |
| sieve | 12.29 ms | 13.51 ms | +10.5% regressed (see note) |
| sortw | 6.86 ms | 6.86 ms | −2.2% improved |
| strwork | 14.03 ms | 14.03 ms | −3.1% improved |
| tokenize | 6.03 ms | 4.03 ms | −33.0% improved |

This run (2026-02-25, aarch64-apple-darwin) includes the cross-CGU
inlining fix (`xcgu` copies now apply on wasm too) and the `punroll`
group-guard unroll for unprofiled loops. `matmul` reached 0.81× LLVM in
the microbench harness; the criterion delta above is −21%. `chars_alpha`
+30% and `parse_only` +11% are new regressions worth chasing — both
were flat before. `hashmap`/`hmget`/`iterw` swung to large wins, partly
real (xcgu inlining) and partly this run's LLVM baseline measuring slow
under build load — treat the exact magnitudes as ±5%.

`sieve` marked −8% this run; its marking loop is
instruction-identical-or-tighter than LLVM's (5-instr inner loop vs
LLVM's 9 with a stack reload). `u128work` inside it improved ~30% from
the i128 `xzr` lowering and `imul(imul(x,a),b) → imul(x, a·b)` reassoc.

`parse_only` was at parity (+9.2% → 0%) after `edgesink` moved the packed
`ParseIntError` `band`/`bor` chains off the success path and the egraph
stopped rematerialising the radix `iconst`; this run shows +11%, a
regression to bisect.

`eqbug` is a narrow-icmp stress kernel kept as a regression guard. Its
inner loop calls `black_box` three times per element. The residual +8%
decomposes into two pieces: LLVM chooses the `*m` asm alternative for
the narrow barriers, so `sturb`/`ldrb`/`strh`/`ldrh` pairs both run on
otherwise-idle LSU ports and truncate the narrow values for free (we
pay a standalone `uxtb`/`uxth` per barrier — a stack round-trip variant
was measured and loses: the store→load forwarding latency outweighs the
ALU saving), and a few loop-invariant constants get rematerialised per
iteration under register pressure where LLVM keeps them in registers.

## Where pliron wins

- `chars_ascii` (−43%), `tokenize` (−36%): byte-classification loops.
  `rangefold` folds range-test trees into offset-and-compare forms, and
  `loopvec` widens the ASCII fast path.
- `iterw` (−44%), `hashmap` (−24%), `hmget` (−17%), `sortw` (−9%):
  loop and call-heavy kernels. `xcgu` cross-CGU copies let the IR
  inliner see bodies rustc assigned to other CGUs (`swap_unchecked` in
  quicksort, `hash_one` in probes); loop rotation, `divconst`
  multiply-high expansion, and tail calls cover the rest.
- `matmul` (−21%): `punroll`'s group-guard unroll — one wide
  `iv + K-1 < n` test per K copies — now fires on unprofiled builds
  (a `trip?` bug had made it PGO-only), and `bcheck` loop versioning
  strips the per-element bounds checks first.
- `fib` (−9%), `contains_rng` (−22%), `sieve` (−8%).

## Where LLVM wins

- `chars_alpha` (+30%): new regression; was −4% before the xcgu/punroll
  work. Uninvestigated.
- `parse_only` (+11%): regressed from parity — worth bisecting against
  the recent unroll/peel reordering; its digit-parse loops are exactly
  what `run_peel` targets.
- `eqbug` (+10%): the per-iteration `black_box` kernel — the residual
  is narrow-barrier `uxtb`/`uxth` pairs vs LLVM's `*m` asm alternative
  (which folds the truncation into the store/load for free).
- `nbody` (+3%): small residual, previously a self-feeding invariant's
  spill pair per iteration.

## WebAssembly

`bench/wasm/run.sh` compiles the same kernel sources to
wasm32-unknown-unknown three ways: stock rustc (LLVM + wasm-ld), the
pliron backend (CLIF pipeline → waffle, the default), and the same
backend under `PLIRON_WASM_CLIF=0`, which selects the minimal
dialect→waffle fallback for comparison.
`bench/wasm/run.mjs` times
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

This run is the committed default configuration on an otherwise idle
host; per-kernel deltas carry ±2% noise, so ±5% and smaller are wash.

| kernel | LLVM | pliron | change | dialect | change |
|---|---|---|---|---|---|
| chars_alpha | 3.88 ms | 4.56 ms | +17.6% | 8.61 ms | +122% |
| chars_ascii | 2.43 ms | 3.46 ms | +42.1% | 5.20 ms | +114% |
| chars_mixed | 21.30 ms | 30.14 ms | +41.6% | 25.85 ms | +21.1% |
| contains_rng | 7.69 ms | 10.15 ms | +31.9% | 18.55 ms | +141% |
| eqbug | 0.24 ms | 0.19 ms | −20.8% | 0.25 ms | +5.2% |
| fib | 15.54 ms | 15.89 ms | +2.3% | 27.61 ms | +77.6% |
| hashmap | 75.64 ms | 130.17 ms | +72.3% | 123.50 ms | +63.2% |
| hmget | 16.17 ms | 29.38 ms | +81.6% | 24.23 ms | +49.8% |
| iterw | 107.85 ms | 52.82 ms | −51.1% | 103.61 ms | −5.2% |
| matmul | 38.87 ms | 27.56 ms | −29.1% | 39.47 ms | +1.4% |
| nbody | 131.36 ms | 127.91 ms | −2.6% | 260.98 ms | +98.7% |
| parse_only | 490.14 ms | 591.46 ms | +20.7% | 648.81 ms | +32.4% |
| sieve | 20.32 ms | 20.16 ms | −0.4% | 21.30 ms | +4.8% |
| sortw | 7.73 ms | 10.27 ms | +32.6% | 11.44 ms | +48.1% |
| strwork | 24.25 ms | 33.92 ms | +40.0% | 44.23 ms | +82.4% |
| tokenize | 6.31 ms | 7.19 ms | +14.1% | 20.34 ms | +223% |
||||||| parent of a4c036c (docs: fresh native/wasm tables; CubeCL gemm_cpu comparison)
| chars_alpha | 4.577 ms | 4.040 ms | −12.1% | 4.020 ms | −12.2% |
| chars_ascii | 2.330 ms | 2.327 ms | −0.1% | 2.329 ms | +0.0% |
| chars_mixed | 18.56 ms | 25.54 ms | +37.7% | 25.56 ms | +37.9% |
| contains_rng | 4.926 ms | 5.506 ms | +11.6% | 5.507 ms | +11.5% |
| eqbug | 0.136 ms | 0.119 ms | −12.2% | 0.120 ms | −11.7% |
| fib | 9.462 ms | 9.011 ms | −5.5% | 8.911 ms | −5.8% |
| hashmap | 66.71 ms | 77.26 ms | +15.7% | 76.97 ms | +15.2% |
| hmget | 11.58 ms | 16.50 ms | +42.6% | 16.55 ms | +42.6% |
| iterw | 67.29 ms | 57.69 ms | −14.0% | 57.69 ms | −14.3% |
| matmul | 30.87 ms | 27.68 ms | −10.6% | 27.61 ms | −10.8% |
| nbody | 84.39 ms | 91.73 ms | +8.7% | 91.69 ms | +8.7% |
| parse_only | 382.9 ms | 510.6 ms | +33.4% | 510.7 ms | +33.0% |
| sieve | 14.05 ms | 17.12 ms | +21.8% | 17.12 ms | +21.9% |
| sortw | 6.258 ms | 7.757 ms | +23.6% | 7.753 ms | +23.9% |
| strwork | 17.40 ms | 23.77 ms | +36.8% | 23.92 ms | +37.5% |
| tokenize | 7.747 ms | 5.258 ms | −31.8% | 5.263 ms | −32.3% |

Geomeans: pliron/LLVM = 1.127, dialect/LLVM = 1.577 (16 kernels). All
kernel accumulators matched across the three modules (the driver
cross-checks return values). Module sizes: LLVM 97 KB, pliron 813 KB,
dialect 1.1 MB.
||||||| parent of a4c036c (docs: fresh native/wasm tables; CubeCL gemm_cpu comparison)
Geomeans: pliron/LLVM = 1.069 (both pliron columns build the same
CLIF-pipeline module; 16 kernels), down from 1.674 before the inlining
work below. Module sizes: LLVM 97 KB, pliron 187 KB.

Changes since the 1.674 geomean:

- wasm stopped sharing `-Os`'s 40-op inline cap: `Wasm32` uses
  `inline::WASM_LIMIT` (= 512) under the `small` no-bonus rules. The
  40-op cap left `str` iterators' `next_code_point` and
  `CharIndices::next` out of line in every char loop — each call
  round-tripped iterator state through stack memory. 320 halved the
  gap; 512 additionally inlines hashbrown's `hash_one` (SipHash + probe
  glue, ~350 ops), dropping hashmap +81% → +16% and hmget +163% → +43%.
  768 bought nothing further.
- `loadfwd` root-normalization fix: `transfer` keys locs under the
  `stop` set (`noalias` ∪ operand-frozen values) but classified
  `iso`/`ncc` under `noalias` alone. A `nocapture` call handed an
  operand-frozen `iadd` cursor (`CharIndices::next(&mut iter)`,
  `iter` = `iadd self, 16`) computed `reach` under the `ofrozen` name —
  `V(v0)`'s `ncc` locs survived the mutating call and the post-call
  reloads forwarded stale pre-call values, zeroing the consumed-width
  delta in `MultiCharEqSearcher::next` (`tokenize` returned 577000 vs
  576990). Calls now insert each arg's root under both
  normalizations into `reach`, and stores kill under both names —
  see `tests/loadfwd_frozen_root.rs`.
- `elide_sp_frame` runs in both `finish()` paths: the CLIF-emit
  lowering never dropped the `__stack_pointer` save/align/restore for
  slotless functions, so `fib` paid a global.get/sub/and/global.set
  prologue plus a restore on every one of its ~2M recursive returns
  (+120% → −5%).
- `CX::norm` skips the `and 255` mask when `normed()` proves the value
  already fits — icmp/eqz results, `and` of masked or 0/1 operands,
  selects of normed arms, shr_u by a big const, narrow unsigned loads.
  Narrow CLIF arithmetic otherwise left a dead mask on every bool
  producer (chars_mixed +49% → +38%).

Caveat on `chars_ascii`: its −90% under the 320 run was a lucky hoist —
the out-of-line `count()` call was pure, so LICM pulled it out of the
×10 rescan loop. At 512 the scan inlines and recomputes per iteration
like LLVM's, so the kernel sits at parity.

Remaining regressions are in scalar codegen, not call boundaries:
`hmget`/`chars_mixed`/`strwork`/`parse_only`/`sortw`/`sieve` still lag.
The common shape is per-element overhead LLVM avoids: `ptr + i*4`
index recompute instead of a bumped pointer (indvars doesn't rewrite
the exit compare to pointer form, so the index stays live), and
`if/else` structured splits where LLVM emits flat `br_if` chains. The
`small` mode's missing bonuses (`once`, `hint`, `fold`, `devirt`,
loop bonus, growth budget) are the other candidate lever.

## CubeCL kernels

`CUBECL_PLIRON_CLIF=1` (in the local `~/code/cubecl` checkout) sends the
kernel `ModuleOp` through `pliron-clif-lower` — `scan` + the rustc
backend's ir-opt order — to a native object, loaded through the same
OrcJit wrapper the LLVM path uses. `~/code/cubek`'s benchmarks build
against it (`benchmarks` enable `cubecl/cpu` + `pliron-dump`), and both
workspaces `[patch]` pliron/cranelift to this repo's vendored forks so
`Context`/`TargetIsa` unify. `CUBECL_DEBUG_PLIRON=<dir>` dumps each
kernel's pliron IR to `kernel.pliron` per launch key.

Measured on aarch64-apple-darwin (Apple Silicon), `cargo bench --bench
gemm_cpu` (criterion, means; `us` = pliron):

| gemm_cpu row | LLVM | pliron | ratio |
|---|---|---|---|
| Simple Unit, Square b=2 1024³, f32 | 74.17 ms | 2.957 s | 39.9× |
| tile-DSL, Square b=2 1024³, f32 | 24.31 ms | 33.51 ms | 1.38× |
| fast leaf t1, Square b=2 1024³, f32 | 110.8 ms | 158.0 ms | 1.43× |
| fast leaf t4, Square b=2 1024³, f32 | 37.23 ms | 60.00 ms | 1.61× |
| fast leaf t8, Square b=2 1024³, f32 | 24.61 ms | 47.96 ms | 1.95× |
| Simple Unit, Square b=2 1024³, f16 | 35.01 ms | 4.826 s | 137.9× |
| tile-DSL, Square b=2 1024³, f16 | 8.480 ms | 21.09 ms | 2.49× |
| fast leaf t1, Square b=2 1024³, f16 | 53.86 ms | 100.0 ms | 1.86× |
| fast leaf t4, Square b=2 1024³, f16 | 14.23 ms | 33.36 ms | 2.34× |
| fast leaf t8, Square b=2 1024³, f16 | 9.170 ms | 40.91 ms | 4.46× |
| Simple Unit, Rect b=1 512³, f32 | 3.889 ms | 1.772 s | 456× |
| tile-DSL, Rect b=1 512³, f32 | 1.462 ms | 4.115 ms | 2.81× |
| fast leaf t1, Rect b=1 512³, f32 | 5.292 ms | 21.64 ms | 4.09× |
| fast leaf t4, Rect b=1 512³, f32 | 1.629 ms | 10.31 ms | 6.33× |
| fast leaf t8, Rect b=1 512³, f32 | 1.326 ms | 2.808 ms | 2.12× |
| Simple Unit, Rect b=1 512³, f16 | 2.461 ms | 2.363 s | 960× |
| tile-DSL, Rect b=1 512³, f16 | 613.1 µs | 4.788 ms | 7.81× |
| fast leaf t1, Rect b=1 512³, f16 | 3.292 ms | 20.15 ms | 6.12× |
| fast leaf t4, Rect b=1 512³, f16 | 982.1 µs | 2.884 ms | 2.94× |
| fast leaf t8, Rect b=1 512³, f16 | 572.0 µs | 3.222 ms | 5.63× |
| Simple Unit, Square b=1 1536³, f32 | 116.8 ms | 69.42 s | 594× |
| tile-DSL, Square b=1 1536³, f32 | 32.28 ms | 41.27 ms | 1.28× |
| fast leaf t1, Square b=1 1536³, f32 | 194.5 ms | 225.8 ms | 1.16× |
| fast leaf t4, Square b=1 1536³, f32 | 52.96 ms | 63.02 ms | 1.19× |
| fast leaf t8, Square b=1 1536³, f32 | 33.43 ms | 41.94 ms | 1.25× |
| Simple Unit, Square b=1 1536³, f16 | 74.49 ms | 539.8 ms | 7.25× |
| tile-DSL, Square b=1 1536³, f16 | 12.73 ms | 20.29 ms | 1.59× |
| fast leaf t1, Square b=1 1536³, f16 | 101.7 ms | 153.4 ms | 1.51× |
| fast leaf t4, Square b=1 1536³, f16 | 26.03 ms | 39.02 ms | 1.50× |
| fast leaf t8, Square b=1 1536³, f16 | 16.88 ms | 26.42 ms | 1.56× |

(The t16 rows fail identically on both paths — cubek rejects a 16-plane
grid on a 14-lane cube.) Results verify by checksum inside cubek, so
every completed row is a correct compute.

What this run does *not* show: the first cut of the CLIF path was 12×
slower on f32 fast-leaf and ~600× on f16 — CubeCL emits `<32 x f16>` /
`<16 x f32>` lines and nothing set the host SIMD knobs, so scalarize
exploded every line to per-lane scalars (f16 via promote/op/demote
libcalls). The fix: `compile_clif` reads `isa.isa_flags()` and sets
`types::WIDE_SIMD`/`NATIVE_F16`/`F16C`/`WIDE_F32_BITS` itself, and
vendored cranelift patch 44 added the aarch64 fp16 encodings this
needs (`fcvtl`/`fcvtn`, f16x8 three-same ALU/FMA, 16-bit lane extract).

The residual gaps are now stratified by strategy:

- Simple Unit (scalar reference kernel): 40–960×. It never vectorizes
  on our path; LLVM auto-vectorizes its inner loop. This is the
  `loopvec`/auto-vec gap, not codegen quality per se.
- tile-DSL / fast-leaf: 1.16–1.6× on the larger square problems —
  close to LLVM. The remaining delta is banded scheduling/fusion
  quality, not instruction selection.
- fast-leaf on Rect 512³ and f16: 2–8×. The rectangular problem mixes
  a masked/edge path where `llvm.select` on `<32 x f16>` still
  scalarizes (elementwise select isn't a leafwise op) and the f16x8
  leaf set lacks `select`/`bitcast`-heavy fallback coverage.
- Odd rows: fast-leaf-t8 f16 at Square-1024 reads 4.46× while t4 is
  2.34× and Simple Unit f16 at 1536 is only 7.25× vs 960× at 512 —
  plane-tile scheduling interacts with codegen quality; individual
  numbers move around between runs.

## Compile time

The backend is slower than LLVM to compile code, and the gap widens with
crate size. Wall-clock `cargo rustc --release` on workspace-member crates
(deps pre-built by stock rustc; release backend `.so`):

| crate | LLVM | pliron | ratio |
|---|---|---|---|
| regex-syntax 0.8.11 | 3.04 s | 16.1 s | ~5.3x |
| serde_json 1.0.154 | 1.33 s | 9.5 s | ~7.1x |
| syn 2.0.119 | 3.83 s | ~122 s | ~32x |

`syn` is the pathological case. After the per-pass fixes below, summed
pipeline-step time dropped from ~483s to ~220s and the heaviest CGU from
100.9s to 43.2s — but wall barely moved because codegen is bound by the
non-pipeline floor: with `PLIRON_OPT_BISECT=0` (all clif passes off) syn
still takes ~87s vs LLVM's 3.8s total. A perf profile of that floor is
diffuse: ~14% hash-map infra (IndexMap attribute lookups, `CallInfo`/`State`
tables), ~6.5% `pliron_clif_lower::lower`, ~9.7% *ungated* ir-opt work
(`inline` 4.4% — its first round runs with no `pass_enabled` gate —
plus nocapture/nowrite/phisimp/domcheck), ~2.6% regalloc2, long tail in
pliron op construction (`pliron_llvm::ops`, `value`/`operation` tables).
So the remaining compile-time gap is mostly *our glue*: op construction +
hash tables + lowering, not the clif passes and not vendored cranelift.

Per-pass fixes that took pipeline CPU ~483s → ~220s:

- `indvars::elim_shadowed`: the fixpoint rebuilt the whole-function use
  set every round (O(fn × rounds) per loop) — now a per-value operand
  refcount decremented in place when a pure inst dies.
- `Analysis::uses()`/`refresh_uses()`: a `(ver, num_insts)`-keyed shared
  `Uses` index (Rc snapshot) so steps that each rebuilt `Uses::new` share
  one build per IR version. Wired into ivalign, ivrefold::deadrecs,
  looprot; ~7 more call sites remain (jumpthread ×2, unroll, memspec ×2,
  bcheck, slp).

Function-level worker threads run the pipeline in parallel across a
crate's code-generation units.

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
| 100 identical pure calls in a loop | ~80 ns | ~185 ms → **~80 ns** |

The last row is `callgvn`: LLVM proves the callee pure and eliminates all
100 calls; we now infer the same facts (`nowrite` + `nounwind` +
`willreturn`) and dedup, hoist and dead-eliminate calls in
`pliron-ir-opt`. The hoist also moves the operand cone (loop-invariant
loads/GEPs) and carries invokes — an unwind edge is keyed on the op, so
it moves with it. `willreturn` (like LLVM's, plus determinism —
volatile/atomic/asm/indirect callees seed the complement) now backs the
used-result erasure: the second same-args call provably comes back with
the same value.

The ~80ns parity on the last row is a four-pass chain, not one pass:
`guardelim` (the CVP panic-path half) proves the bounds checks inside
`dotprod` unreachable — the `min(a.len, b.len)` diamond's own edge facts
give `min <= a.len`/`min <= b.len` — so `nounwind2`/`nowrite2` upgrade the
callee to `nounwind` + `nowrite=Some(true)`. `xoridem` then collapses the
100-trip loop to iteration 0 by redirecting the dominated back-edge to
the exit (kept calls need only `nowrite`+`willreturn`: a dropped
iteration's call would return the same value, and unwind/diverge is
preserved by the copy that still runs). The even trip count makes the
`acc ^= call` stores dead — `s0` is the answer — so `xoridem` erases them
plus their xor/load feed chain, the call result goes unused, and
`callgvn2`'s dead-call elim (nowrite + nounwind + willreturn) erases the
callsite: loop and call both fold away. LLVM gets the same number by a
different route (`umin` bound reasoning → `nounwind` +
`memory(argmem:read)` → dead-arg-elim → callsite death).

Support pieces that made it fire: `nowrite` splits writes on
return-reachable blocks from writes on dead paths (`eh.exn` pads, aborts)
so callers aren't poisoned; `nounwind` inference runs by default (it also
turns invokes of nounwind callees into plain calls). `-Ctarget-cpu` /
`-Ctarget-feature` now map to Cranelift ISA flags (e.g. `native` → SSE4.1
`pmulld` instead of `pmuludq` repacks; `PLIRON_X64_VEX=1` switches
VEX-128 forms on for the ops that have `use_avx*` rules).

## Incidents found while profiling this work

- **`callgvn` miscompile** (`d781a62`): `writes_between`/`stored_allocas`
  scanned ops that earlier passes had already *erased* from the op list —
  the stale entries counted as writes (or left a value used before its
  def was lowered). Symptom was a lowering ICE on `cranelift-isle`
  (`sema` type-check), which also masked the `bittab` `flat()` mask bug
  bisected later (unit 34819, `translate_pattern`). Fix: skip erased ops
  in both scans. Same unsoundness class as `xoridem`'s
  in-loop-stored-alloca rejection (`ed1159c`): an erased/stale op must
  never feed an alias or purity query.
- **`guardelim` OOM** (`b1d3c22`, `ba32720`): the reaching-stores
  fixpoint kept a dense per-block `slot → store` map covering *every*
  slot; on `cranelift_codegen`'s generated ISLE functions that was
  ~24GB — the pass died to the OOM killer before the bailout bound could
  help. Two-part fix: bail early on giant functions *before* building the
  dense map (`b1d3c22`), then make the map sparse — only slots that are
  both stored and loaded-unescaped can ever answer a query (`ba32720`).
  24GB → bounded set; 75 CGUs run the pass with 17 bounded bails instead
  of a universal skip.

## What is next

- `nbody` (+2.9%): investigate — flip-flops between runs.
- `hmget` (+2.5%): self-feeding invariant's spill pair per iteration;
  needs regalloc-level remat/pressure work.
- `eqbug` (+6.7%): residual is LLVM's `*m`-form black_box folding the
  narrow truncation into the barrier's store/load pair for free, plus
  per-iteration constant rematerialisation under register pressure.
- `black_box`: done — the CLIF `black_box` op (vendored Cranelift patch
  22) emits zero instructions, matching LLVM's `asm("" : "r,~{memory}")`.
  wasm still uses the volatile-load fallback.
- `chars_mixed`: extend the `rangefold` coverage to the non-ASCII path.
- `hmget`: residual spill traffic in the hashbrown probe loop.
- More `loopvec` coverage: gather-loop shapes, multi-instruction update
  chains, and inner loops inside outer loops.
- Compile time: run `slp` only where vectorizable shapes exist, or add an
  applicability check that skips passes that cannot fire.

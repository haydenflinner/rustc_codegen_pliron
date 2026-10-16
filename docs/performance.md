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

`sieve` reads +10.5% under criterion but the marking loop is
instruction-identical-or-tighter than LLVM's (5-instr inner loop vs
LLVM's 9 with a stack reload) and interleaved A/B timing of the two
criterion binaries measures pliron ~6% *faster* — the delta is
baseline-window noise on a memory-bound 5MB workload (the run logged
17 severe-high outliers). `u128work` inside it improved ~30% from the
i128 `xzr` lowering and `imul(imul(x,a),b) → imul(x, a·b)` reassoc.

`parse_only` reached parity from +9.2%: `edgesink` moves the packed
`ParseIntError` `band`/`bor` chains off the success path onto the cold
error edges (and pushes the shared `band` producer into the edge
blocks), and the egraph no longer marks `iconst` for remat — the
radix constant now sits in one register like LLVM's `w14` instead of a
`movz` per digit block.

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

- `eqbug` (+6.7%): the per-iteration `black_box` kernel — see the eqbug
  note above for what the residual is made of.
- `sieve` (+10.5% criterion, −6% interleaved A/B): measurement noise on
  a memory-bound kernel; the codegen itself is at parity or tighter.
- `nbody` (+2.9%) and `hmget` (+2.5%): hmget's residual is a
  self-feeding invariant's spill pair per iteration.

## WebAssembly

`bench/wasm/run.sh` compiles the same kernel sources to
wasm32-unknown-unknown two ways: stock rustc (LLVM + wasm-ld) and the
pliron backend (CLIF → waffle). The table below also shows the since-
removed dialect → waffle path (`PLIRON_WASM_CLIF=0` at the time).
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

Geomeans: pliron/LLVM = 1.127, dialect/LLVM = 1.577 (16 kernels). All
kernel accumulators matched across the three modules (the driver
cross-checks return values). Module sizes: LLVM 97 KB, pliron 813 KB,
dialect 1.1 MB.

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

## Compile time

The backend is slower than LLVM to compile code, and the gap widens with
crate size. Wall-clock `cargo rustc --release` on workspace-member crates
(deps pre-built by stock rustc; release backend `.so`):

| crate | LLVM | pliron | ratio |
|---|---|---|---|
| regex-syntax 0.8.11 | 3.04 s | 16.1 s | ~5.3x |
| serde_json 1.0.154 | 1.33 s | 9.5 s | ~7.1x |
| syn 2.0.119 | 3.83 s | 152.9 s | ~40x |

`syn` is the pathological case: ~182s of summed pipeline-step time spread
uniformly across passes (looprot 23.7s, indvars 18.9s, slp 15.1s,
jumpthread 11.4s, emit 10.6s, then ivrefold/loadfwd/ifconv2/selfarg/licm/
mergechain/edgespec ~6-7s each) — no single quadratic, just ~95 passes of
per-function cost on syn's macro-generated functions. One CGU alone holds
72s (slp 12.5s inside it is the largest single-step spend).

The optimizer pipeline takes ~95% of backend time. Function-level worker
threads run the pipeline in parallel across a crate's code-generation
units.

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

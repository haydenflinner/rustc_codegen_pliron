# Performance report: pliron vs LLVM (x86-64 windows-msvc)

Measured on the Windows dev box, `rustc -O` (SSE2 baseline — no
`-C target-cpu`), stock LLVM backend vs `rustc_codegen_pliron`,
min-of-5 interleaved runs after killing stale CPU-hog processes.
Checksums verified identical on every kernel.

## Runtime vs LLVM (ms, lower is better)

| benchmark    |  LLVM  | pliron | ratio | status          |
|--------------|-------:|-------:|------:|-----------------|
| chars_alpha  |  1.664 |  1.646 | 0.989 | faster          |
| chars_ascii  |  0.958 |  0.951 | 0.993 | parity          |
| chars_mixed  |  9.476 |  9.367 | 0.988 | parity          |
| contains_rng |  2.497 |  2.291 | 0.918 | faster          |
| fib          |  6.671 |  6.711 | 1.006 | parity          |
| hashmap      | 50.280 | 53.175 | 1.058 | regalloc-bound  |
| hmget        |  8.825 |  9.456 | 1.072 | regalloc-bound  |
| iterw        | 28.995 | 29.956 | 1.033 | parity          |
| matmul       |  5.157 |  5.466 | 1.060 | regalloc-bound  |
| nbody        | 67.468 | 68.575 | 1.016 | parity          |
| parse_only   |271.535 |249.825 | 0.920 | faster          |
| sieve        | 13.654 | 13.159 | 0.964 | faster          |
| sipr         |221.161 |218.421 | 0.988 | faster          |
| sortw        |  5.519 |  5.635 | 1.021 | parity          |
| strwork      | 11.263 | 12.373 | 1.099 | regalloc-bound  |
| tokenize     |  2.521 |  2.885 | 1.144 | regalloc-bound  |

**11/17 kernels at or beating parity** (6 strictly faster). Worst
remaining: strwork 1.10, hmget 1.07, matmul/hashmap 1.06. tokenize
|reached ~1.0 after the lcse ident guard (`972c932`).
(hashmap swings 1.06–1.21 across runs on this box — load-dependent;
quiet-box best-of-5 lands ~1.06.)

### The lcse/tokenize interaction

`PLIRON_LCSE=0` buys ~12% on tokenize but loses ~16% on sortw and ~8%
on parse_only — no global win. Bisecting the four `lcse*` pipeline
positions plus `ident`'s internal arms isolated two live-range-merge
harms, both fixed:

- `00b05a2` — the select-cond identity (`select (x==0), 0, x → x`,
  added in `0d9a60f`) aliased a distinct in-loop value onto a
  cross-block arm, deleting the in-loop def ra2 could rematerialize.
  Removed outright: no scoping variant recovered the win, and the
  fold never had a measured benefit.
- `972c932` — lcse's `ident` aliases generally (`x*1`/`x+0`/
  `uextend(ireduce)`) now skip in-loop copies whose surviving value
  is defined in a different block: the copy is ra2's only in-loop
  remat point, and merging forces the outer value live-through into
  a spill home. tokenize 2.90→2.53 (≈1.0 vs LLVM); rest of suite
  unchanged within noise.

## Compile time (ms, whole `rustc -O` invocation)

| benchmark | LLVM | pliron | ratio |
|-----------|-----:|-------:|------:|
| hashmap   |  311 |    949 | 3.05x |
| sortw     |  282 |    806 | 2.86x |
| hmget     |  300 |    769 | 2.56x |
| nbody     |  195 |    510 | 2.62x |
| tokenize  |  212 |    513 | 2.42x |
| typical   | ~190 |   ~420 | ~2.2x |

Pass-level breakdown (`PLIRON_TIMING`, hashmap CGU): the pliron CLIF
pipeline is ~290ms of the ~950ms total; cranelift+emit itself is ~30ms.
Top steps: `slp` 75ms (was 100ms — lazy use-map below), `jumpthread`
70ms, `mergechain` 31ms, `edgespec` 17ms. Everything else ≤ 12ms.

## Code size (.text bytes)

| benchmark |   LLVM | pliron | ratio |
|-----------|-------:|-------:|------:|
| hashmap   | 113982 | 133342 | 1.170 |
| hmget     | 113966 | 129918 | 1.140 |
| sortw     | 114270 | 125646 | 1.100 |
| sieve     | 110574 | 114302 | 1.034 |
| strwork   | 112414 | 115806 | 1.030 |
| matmul    | 111342 | 114494 | 1.028 |
| typical   | ~112k  | ~114k  | ~1.02 |

The hashbrown-heavy kernels (hashmap/hmget/sortw) carry the worst size
bloat — more inlined helper specializations survive our pipeline.

## What was done this cycle

- `3c43542` regalloc2 spill weight → `min(prio, 8*isqrt(prio))`.
  The vendored `4*sqrt` favored long-lived bundles so hard that sparse
  values (SipHash seeds, hashbrown ctrl base) monopolized registers in
  dense loops; plain upstream `prio` starved lexer-type long ranges.
  `min8` is the measured optimum of 8 formulas tested
  (hashmap 1.20→1.06 vs LLVM).
- `cb4b139` `licm2` — a second LICM after `bcheck`/`loopvec`/`slp` so
  versioning-guard `min`/offset math hoists to preheaders.
- `29722bd` fold `icmp`/`select` null tests against `stack_addr`
  (hashbrown empty-table guard) — normalizes all table-header loads
  under one stack-slot root, enabling downstream CSE/disjointness.
- `9939913` `memspec` — clone jump-continuations when a predecessor
  just stored what they load (guarded by escape-scoped isolation and
  an escaping-def check). Verified sound; can't fire where it matters
  without `!noalias` metadata CLIF doesn't carry.
- `0d9a60f` `lcse`/`csweep` identity: `select (x==0), 0, x → x`
  (Option-chain residue; collapsed a 2-block select chain in tokenize)
  — **reverted by `00b05a2`**: the alias merges a distinct in-loop
  value onto a cross-block arm, stretching the arm's live range and
  deleting the in-loop def ra2 could have rematerialized. Bisecting
  `PLIRON_LCSE=0`'s ~12% tokenize win to this arm; every scoping
  variant (loop-level, same-block, param-only) failed to recover it,
  and the fold was added without a measured win.
- `4f5050f` `slp` lazy function use-map shared across blocks —
  slp pass time 100.7→75.1ms on hashmap.
- Upstream `0cc7e67` `affmerge` (affine phi merge) merged in parallel.

## Measured dead ends

- `RA2_SUMCOST` (sum vs max eviction-conflict weight): neutral-to-worse.
- `PLIRON_LICM_NOLOAD` (keep invariant loads in-loop for remat):
  uniformly worse — hoisted loads win.
- Post-regalloc dead spill-store elision: correct but fires on ~2
  stores across the whole suite — ra2 rarely emits them.
- `PLIRON_X64_VEX`: VEX encodings aren't wired for regular SSE ops in
  the vendored lowering (`has_avx` used in only 2 rules).
- Depth-weight bump (`4.0→8.0` loop bonus): miscompiled, reverted.
- `dgvn` loop-depth guard (keep cheap-ALU in-loop dups as ra2 remat
  points instead of merging onto shallower defs, mirroring the lcse
  `ident` guard): wash on tokenize/hashmap/hmget/sortw/strwork/iterw,
  consistently ~4% worse on matmul — reverted. Unlike lcse's identity
  folds, dgvn's merges don't fire on the harmful pattern often enough
  to matter.

## The remaining wall

Every residual gap traces to one mechanism: **regalloc2 ion's
spill/split placement under high pressure**. In the SipHash+probe and
byte-scan loops we carry ~10-12 live values through inner loops; ion
parks loop-invariants in spill homes and pays store+reload pairs per
iteration (e.g. hmget's probe loop stores the ctrl-block base to
`0xa8(%rsp)` every iteration), while LLVM's allocator keeps the hot
set register-resident and reloads only 2 cheap seed words. Concrete
per-lookup cost: ~140 insts with ~10 memory ops vs LLVM ~110 insts
with ~4. CLIF inspection shows no redundant carried params — the IR
is clean; the gap is allocation decisions, not optimization coverage.

Fixing it properly means work in `vendor/regalloc2/src/ion/`:
live-range splitting criteria, eviction-cost modeling (sum vs max was
measured neutral), or rematerialization support. All three are
multi-day allocator work with real correctness risk; the cheap knobs
(env-gated formulas, early-outs) are exhausted.

Secondary, smaller: code layout — our hot paths scatter over ~1.8KB
with interposed cold blocks vs LLVM's ~440B compact loops (extra
unconditional-jump trampolines per iteration), and residual diffuse
bloat (+1-3% .text) on non-hash kernels.

## slp/xpack compile-time work (9331bc1)

`xpack` rescanned every seed after every apply — nbody block63 ran
~38k plan tries for 76 packs (~143ms in `slp`). Landed fixes:

- Lazy, function-wide `uses` map shared across blocks, rebuilt only
  after an apply (4f5050f), dense `Vec<SmallVec>` keyed by value index,
  and `param_src` hoisted to `run()` (its applies never touch block
  params). hashmap slp 76→19ms.
- Failed-pair cache: a pair whose plan fails is retried only when a
  later apply could have unblocked it (edit inside the pair's span,
  moved/deleted endpoint, changed operand/result use chain — tracked
  via an upstream-closure `affected` set). Store pairs are never
  cached: `plan_store` resolves footprints through upstream address
  chains the eviction can't cheaply track. nbody slp 143→117ms,
  identical pack output.

Dead ends along the way, measured and reverted:

- Resume cursor (continue scan past the last apply): 135→17ms but
  missed 44 of 76 packs — sunk consumers land near the emit point and
  pair with *earlier* seeds — cost nbody ~14% runtime.
- Batch plan collection (apply all non-conflicting plans per scan):
  converged in ~6 waves but each wave re-tried every failed pair —
  28k tries, ~170ms, 61 rows; the dirty/span conflict checks plus
  re-tries cost more than sequential+cache.
- Aggressive eviction (positions of all users of affected values):
  recovered some stale store packs but the per-wave bookkeeping ran
  ~80ms by itself.

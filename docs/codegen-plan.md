# Codegen plan: generated-code parity with LLVM

Status 2026-10-31. Companion to [`PARITY.md`](PARITY.md) (the scoreboard
and the short work list). This file is the long-range plan for runtime
speed: what LLVM and GCC have that we lack, in what order to build it, and
how to tell whether it worked.

## Where we are

| suite | pliron / LLVM | note |
|---|---|---|
| our 19 kernels, native | ~0.91 | tuned against for three weeks |
| 8 held-out programs, native (`bench/heldout`) | **1.31** | 1.57 this morning |
| of which tuned on today | blake3 1.19, blind-sig 1.21, pulldown-cmark 1.51, regex 1.33, serde_json 1.28-1.38 | |
| of which kept untouched | lol_html 1.45, flate2 1.47, prost 1.01 | the check set |

The held-out number is the one that matters. The kernels say we can match
LLVM on code shapes we have looked at; the held-out programs say how much
of LLVM's general machinery we are still missing.

## What today's fixes have in common

Every gap found on the held-out programs so far
(`history/2026-10-31-tune-heldout.md`) is a case LLVM handles with a
general framework and we handled with a local patch:

| symptom | our fix | the LLVM/GCC machinery that does it generally |
|---|---|---|
| `const` table copied to the stack, every index a load + bounds check (blake3, 3.1x) | `constload::forward_copies` | InstCombine `isOnlyCopiedFromConstantMemory`; GCC's SRA + CCP |
| rotate folded into a shifted operand twice (blake3) | single-use rule in one ISLE family | isel's one-use checks everywhere (`isWorthFoldingALU`) |
| u128 `>> 64` as a variable shift (blind-sig) | `shift_i128_const` | type legalization splitting i128 ops by constant |
| carry as `add; cmp; cset; add` (blind-sig) | one ISLE family | DAG combine to `UADDO`/`ADDCARRY`, `adds`/`cinc` |
| zip loop testing both ends (blind-sig) | `ivcmp` | SCEV + IndVarSimplify LFTR (one exit compare) |
| table address rebuilt every iteration (pulldown) | three x64-only switches | MachineLICM + regalloc rematerialization |

The pattern repeats across the code base: `pliron-clif-opt` runs ~100
steps, most of them recognizing one shape. Each is cheap to write and each
misses the next variant (`ivcmp` itself regressed `parse_only` on a shape
it was not written for). LLVM and GCC reach parity on unseen code because
a few general analyses (SCEV, value ranges, alias analysis, branch
probabilities) feed general transforms. The plan is to build those, then
retire the special cases they subsume.

## Principles

1. **Held-out first.** A change is judged on `bench/heldout` and the
   kernels together. Tune on blake3, blind-sig, pulldown-cmark, regex and
   serde_json; lol_html, flate2 and prost stay untouched as the check set.
   Grow the held-out suite (W0) before the tuning set wears out.
2. **Frameworks over patterns.** Prefer an analysis that answers a class
   of questions (trip count, value range, probability, pressure) to a pass
   that matches one CLIF shape. When a framework lands, delete the passes
   it subsumes; the step count is compile time.
3. **Target-aware costs.** Several regressions came from heuristics
   measured on x86-64 and applied to aarch64 (address cloning, rotate
   folding). Every cost decision states its target, or takes a cost from
   the ISA.
4. **Facts from rustc are free.** rustc gives LLVM `nsw`/`nuw`, `inbounds`,
   `!range`, `noundef`, `nonnull`, `align`, `noalias`, `dereferenceable`.
   Every one we drop is reasoning we must redo or forgo.
5. **Batch validation.** Implement in runs; validate the stack (gate,
   crates, fuzz window, held-out, kernel A/B) once, bisect within it only
   on failure. Watch `earlyoom` (it SIGTERMs `rustc` under swap pressure
   and fakes failures; run heavy suites with `--jobs 2-3`).

## Workstreams

Ranked by expected held-out gain per effort. Gains are estimates from the
evidence named, not measurements.

### W0. Measurement (prerequisite, days)

- **Gap census.** A tool that, for each held-out program, profiles a loop
  driver on both sides (samply; the drivers in today's history note),
  matches hot functions by symbol, and reports per function: samples,
  instructions, stack loads/stores, taken branches per loop iteration, and
  an instruction-mix diff. Each top function gets a cause tag
  (isel idiom / spill / layout / IV / inlining / vector / memory). This
  turns "pulldown is 1.5x everywhere" into a ranked list.
- **More held-out programs.** The other sightglass Rust programs need wasm
  (W10); add native crates with stable benches: `sha2`/`sha3`, `zstd`
  (pure-Rust `ruzstd`), `image`'s PNG decode, `rust_decimal`,
  `serde_json` value-heavy inputs, `aho-corasick` search, `brotli`
  (pure Rust), `syn` parsing as a runtime benchmark. Keep a third of
  them untouched.
- **x86-64 numbers.** Not measured since 10-22. Half the heuristics are
  x86-derived; aarch64-only measurement cannot see their regressions.
- **wasm held-out.** The sightglass programs are wasm benchmarks; build
  them for wasm32-wasip1 with the pliron sysroot (`harness/run.py
  ui-wasm` provisions it) and run under V8.

### W1. Carry rustc's facts through to CLIF (weeks; enables W2, W3)

LLVM's loop and range reasoning starts from rustc's poison-generating
flags and metadata. Lowering keeps some (`!range` on bool loads,
`nonnull`, `noalias` scopes, `dereferenceable`) but not the general set.

- Record `nsw`/`nuw` on add/sub/mul/shl, `exact` on shifts and divides,
  `inbounds` (and `nuw`) on GEPs, `disjoint` on `or`, `!range` and
  `noundef` on loads and calls, `align` on params and loads, as fact
  tables keyed by CLIF value (the `VMap` facts lowering already passes).
- Keep them valid through rewrites: facts are dropped, never invented,
  when a pass changes a value's definition (`diffcheck` audits this).
- Consumers: IV widening without `sext` in the loop (W2), trip counts
  from `nuw` steps, range seeds (W3), alignment for vector loads (W10).

### W2. SCEV-lite and IV canonicalization (1-2 months; large)

LLVM's ScalarEvolution and the passes on it (IndVarSimplify with LFTR,
LoopStrengthReduce, IRCE, the trip counts unroll and vectorize use) are
the biggest single framework we lack. On CLIF:

- **Recurrence analysis**: every loop-header param as an add recurrence
  `{start, +, step}` (nested for inner loops), with `nuw`/`nsw` from W1;
  values in the loop as SCEV expressions; trip count for loops whose exit
  compares an affine IV with an invariant.
- **LFTR**: rewrite every exit to one canonical IV compare against a
  computed limit, then delete the redundant tests and dead IVs. Subsumes
  `zipiv`, `ivcmp`, `ivalign`, part of `ivretro` and `boundcmp`.
- **LSR with aarch64 addressing modes**: choose IVs for `[x, #imm]`,
  post-index `[x], #k`, and `[x, y, lsl #s]`; today `indvars`, `postinc`
  and `ivrefold` each pick locally.
- **IRCE / bounds-check versioning** from trip counts (generalizes
  `bcheck`), and exit-value replacement (a loop computing only its final
  IV value folds away).
- **Trip-count consumers**: unroll by known/runtime count with a
  remainder, and `willreturn` for loops SCEV proves finite (the
  `iropt.ipo.willreturn` gap).

### W3. Value ranges (weeks)

GCC's ranger and LLVM's LazyValueInfo + CorrelatedValuePropagation: an
on-demand range for any value at any block, from definitions (`band`,
`uextend`, `urem`, loads with `!range`), branch conditions on dominating
edges, and IV bounds from W2.

- Kill redundant zero/sign extensions (`eqbug`'s `uxtb` before compares),
  overflow checks whose operands are bounded, bounds checks with a known
  index range (`i % 5` into a 5-element array), and impossible switch
  arms.
- Narrow compares and arithmetic where ranges allow (`ult` to `eq`, i64
  to i32 ops).
- Feed jump threading: a range-implied branch direction threads without a
  shape match. Subsumes parts of `rangefold`, `celim`, `sgntest`, `mask01`.

### W4. Register allocation: rematerialization, splitting, pressure (1-2 months)

regalloc2 does not rematerialize. Our workarounds clone constants and
addresses into use blocks before allocation (`splink`, `remat_sink`,
REMATCALL), which is right on x86-64 and wrong on aarch64, and pressure
blocks hoisting (`PLIRON_ELABLOOPS` stays off because hoisted values
spill in `strwork`). LLVM's greedy allocator rematerializes cheap defs at
use, splits live ranges around loops and calls, and places spill code in
cold blocks.

- Rematerialization in regalloc2 for single-instruction defs (constants,
  `adrp`/`add` pairs as one unit, frame addresses): the allocator decides
  per spill, so the pre-allocation cloning heuristics can go.
- Region splitting around loops, so a value live across a loop but unused
  inside it spills at the loop boundary, not inside.
- Spill placement in cold blocks (panic paths are everywhere in Rust).
- Pressure estimates exported to LICM and hoisting (`ELABLOOPS`,
  `licm`), so hoists stop where they would spill (GCC's
  `-fsched-pressure` idea applied to code motion).
- Evidence: blake3's compression keeps 61 reloads vs LLVM's 35 after
  today's fixes; `strwork`'s ELABLOOPS spills; REMATCALL's sum_postcard.

### W5. Block placement and branch probabilities (weeks)

Cranelift lays blocks out in RPO with cold sinking; we patched edges.
Both hot loops studied today take two branches per iteration where LLVM
takes one (pulldown's escape scan, blind-sig's limb loop).

- Static branch probabilities (LLVM BranchProbabilityInfo / Ball-Larus):
  panic and `unreachable` paths cold, loop back edges likely, rustc's
  `likely`/`cold` hints, pointer and zero tests, early returns.
- Chain-based placement (MachineBlockPlacement): loop body contiguous,
  latch at the bottom so the back edge is the only taken branch, exits
  fall through to cold code out of line, hot fallthrough chains elsewhere.
- Loop rotation for layout where the CLIF form tests at the top
  (`looprot` does the IR side).
- Tail duplication guided by probabilities (generalizes `taildup*`).

### W6. Instruction selection idioms, systematically (continuous)

Three of today's six wins were missing aarch64 idioms. Instead of finding
them one benchmark at a time, mine them: run the census (W0) over the
corpus, diff hot-loop instruction mixes against LLVM, and list sequences
LLVM never emits that we do (`cmp; cset; add`, `lsr; lsr; ... csel`).

aarch64 catalog to audit against LLVM's AArch64 combines:
`adds`/`adcs`/`sbcs` chains for multi-word arithmetic, `cinc`/`cinv`/
`cneg`/`csinc`, `ccmp`/`ccmn` chains for `&&`/`||` conditions,
`bfi`/`bfxil`/`ubfx`/`sbfx`, `extr`, `madd`/`msub`/`umaddl`/`smaddl`,
`umulh`/`smulh` from wide multiplies, `tbz`/`tbnz`, `cnt`, `rev`,
`clz`/`rbit`, `ldp`/`stp` and pre/post-index forms, shifted and extended
operands only on one-use values, `fmadd`. x86-64 catalog: `adc`/`sbb`,
`lea` three-operand, `cmov` with branch-probability gating, `bt`, BMI
(`andn`, `blsr`, `tzcnt`), `mulx`, load-op fusion.

Each new rule gets an ISLE test (like `carry_cinc.rs`) and runs through
the `diffcheck` interpreter on the corpus.

### W7. Inlining and interprocedural (weeks)

- **Cost model with call-site benefit**: constant arguments that fold,
  loop depth of the site, callee size after simplification (LLVM's
  InlineCost); blake3's `compress_pre` stays a call from the chunk loop.
- **Partial inlining** (GCC `ipa-split`, LLVM PartialInliner and
  HotColdSplitting): inline the fast path, outline the cold tail. Rust's
  `?` and panic paths make this common.
- **IPSCCP and argument promotion**: interprocedural constant propagation
  (we have `spec` for all-callers-agree); by-reference small reads become
  by-value (GCC IPA-SRA); dead-argument elimination (`PLIRON_DEADARG`,
  opt-in) validated and on.

### W8. Instruction scheduling and ILP (weeks; measure first)

Cranelift emits in IR order. Apple cores are wide and out of order, so
this matters less than on in-order cores, but long chains still bound
loops (blake3's add/xor/rotate chain; the carry chain).

- A pre-allocation list scheduler per block, pressure-aware (LLVM
  MachineScheduler, GCC `-fsched-pressure`): issue loads and long-latency
  ops early, interleave independent chains.
- MachineCombiner-style reassociation for ILP (`PLIRON_REASSOC` is the
  opt-in start), `madd` formation where it shortens the critical path.
- Measure on the census first; skip if gains are within noise on Apple
  cores, revisit for x86-64.

### W9. Memory optimizations (weeks)

- A MemorySSA-lite on CLIF to unify `loadfwd`/`dse`/`slotprom`/`memspec`
  and the region-frozen facts: load PRE across loops, store sinking, DSE
  across blocks.
- Scoped `noalias` kept through inlining (LLVM's inliner turns callee
  `noalias` params into scope metadata); we have frozen/scoped regions,
  extend them to all `&mut` params.

### W10. Vectors (weeks)

- **`core::arch` NEON fidelity**: memchr, aho-corasick (Teddy) and regex
  use NEON intrinsics directly; audit that each lowers to one instruction
  (regex 1.33x, lol_html 1.45x are scanning-heavy).
- Early-exit loop vectorization (find-first loops; LLVM gained it
  recently) and SLP coverage for byte classification tables.
- wasm SIMD for the wasm held-out suite (W0).

## Progress (2026-10-31)

Held-out geomean 1.57x (morning) -> 1.27x (`bench/heldout`, 8 programs,
current stack). The check set moved too (lol_html 1.45 -> 1.33, flate2
1.47 -> 1.42), so most of it is general.

What the census (`bench/heldout/census.py`, W0) showed, and what was done:

- **Stack traffic is mostly allocator spills**, not memory-resident locals
  (`PLIRON_FRAME_INFO`: regex `Utf8Sequences::next` 464 B spills vs 24 B
  slots; serde `skip_to_escape` 112 vs 0). W4 is the top lever.
  - Fixed-register call arguments pinned whole bundles to a
    call-clobbered register (`PLIRON_ARGCOPY`, Cranelift patch 75).
  - No rematerialization: constant and address reloads now re-execute
    their definition (`PLIRON_REMAT`, patch 76).
  - Still open: loop-aware splitting (a use-free gap inside a loop goes
    to the stack); a first heuristic (keep in-loop gaps with the register
    piece) made serde worse and was reverted.
- **NEON fidelity (W10) was a hidden factor**: `src/simd.rs` scalarized
  portable-SIMD shifts and narrowing casts, so memchr's movemask (serde,
  regex, lol_html) ran lane by lane (`clif.lower.simdshift`).
- **Placement (W5)**: 64-byte loop alignment for small loops (patch 74);
  identical code across a 64-byte line cost chars_ascii 1.77x.
- **Isel idioms (W6)**: rotate folds (patch 71), `adds`/`cinc` carries
  (patch 72), constant i128 shifts (`clif.lower.shift128k`).
- **IV forms (W2 groundwork)**: `ivcmp` (offset exit tests, only on IVs
  whose sum is otherwise an address: the batch caught it regressing
  parse_only on a carried pointer), `invparam` (LoopSimplify-style
  separation of loops whose back edges disagree; opt-in until W4).
- **Facts (W1)**: `PLIRON_INBOUNDSNN` (inbounds GEP of a nonnull base is
  nonnull) exists but is off; brotli's `FindMatchLengthWithLimit` pays a
  null check per byte and pointer materialization because of it.
- Two old miscompiles surfaced in a new fuzz window (seeds 16782,
  16787); they predate this work.

## Sequencing (revised 2026-10-31, after the census)

The census reordered the plan: register allocation (W4) and the fact
infrastructure (W1) come first, ahead of new idioms.

LLVM and GCC both run mostly fixed pipelines (`passes.def`,
`buildPerModuleDefaultPipeline`); what they do differently is the
contract between passes. Analyses (SCEV, ranges, alias, MemorySSA)
are queried, cached and invalidated, not recorded in side maps every
pass must carry by hand; passes assume canonical forms (LoopSimplify,
LCSSA) instead of matching around variants; large transforms scan,
plan against a cost model, then rewrite (SROA slices, LSR formulae,
VPlan). Our failures today were all contract failures: indvars dropped
the `derived` facts jumpthread needed; licm hoists every constant on a
rematerialization bet regalloc2 did not take; constdedup2 undid the
egraph's constant placement.

**Measured, not static (same day, `bench/heldout/dynstack.py`).** The
census counted instructions statically; most spills it found sit on
cold paths. Classifying every profile sample by instruction class,
pliron minus LLVM, per program:

| program | extra samples | stack accesses | prologue/epilogue | non-stack loads |
|---|---|---|---|---|
| pulldown-cmark | 535 | +85 | +171 | +214 |
| rust-json | 347 | -22 | +114 | +150 |
| regex | 346 | +69 | +40 | +115 |
| blake3 | 127 | +101 | +79 | +17 |
| blind-sig | 92 | +9 | +9 | +10 (compares +167) |

So for runtime the order is: call and frame cost (prologue/epilogue
shape, shrink-wrapping, and inlining of what LLVM inlines), then loads
LLVM avoids (aliasing facts, load forwarding and hoisting: the facts
service), then spills. Item 1 below stays, but behind frame cost.

**Phase 1, parity (1.27 -> ~1.0):**

0. **Frame cost.** Cranelift's aarch64 saves callee-saved registers with
   pre-indexed pushes, a serial chain through `sp` (LLVM: one `sub sp`,
   stores at offsets; `PLIRON_A64_FLATSAVE`), and saves them at entry on
   every path (LLVM shrink-wraps: serde's `skip_to_escape` and
   pulldown's scanners return early through a ten-register save).

1. **Register allocation (W4).** Hot functions carry 5-40x LLVM's stack
   traffic, mostly spills (pulldown's `parse_block`: 22% of its
   instructions touch the stack). Done: call-arg detach, rematerialized
   reloads, dead remat-slot stores. Next: split live ranges around calls
   (a value crossing a call goes to a callee-saved register or to memory
   *around the call*, not for its whole range), loop-aware splitting
   (no reload inside a loop for a value untouched in it), profile and
   block-frequency weights. If regalloc2's bundle model resists, a
   greedy allocator in LLVM's style is the fallback.
2. **Facts as a service (W1).** One API for nonnull / derived (inbounds
   offset of) / range / bool facts, queried by passes and preserved by
   construction: every value replacement or clone goes through one hook
   that carries facts, instead of each pass calling `prop_derived` when
   it remembers. Then the losses get found by a checker, not by
   benchmarks: a debug mode that recomputes what it can and reports facts
   a pass dropped.
3. **Canonical loops and one licm with a pressure model.** LoopSimplify
   form maintained (`invparam` is the first piece); constants stay at
   their uses for the allocator to rematerialize.
4. **Pure rewrites into the egraph.** `clifpeep` rules that are pure
   value rewrites become ISLE rules, so they compose instead of being
   ordered; nothing after the egraph may undo its placement.
5. **Inlining (W7)** per the census' LLVM-heavy rows (regex
   `follow_epsilons`: LLVM 227 insts vs our 1064 after different
   inlining).

**Phase 2, past LLVM:** facts LLVM loses (borrow-checked noalias, slice
`len <= isize::MAX` and in-bounds offsets, iterator niche facts, cold
panics, discriminant ranges), Apple M-series tuning (alignment moved
chars_ascii 1.77x; fusion pairs; NEON idioms), PGO-directed effort on
hot functions, specialization from knowing every monomorphized instance.

Each phase should leave the step count lower than it found it. Compile
time stays the constraint (CMP 0.85x LLVM today).

## Not doing

- More single-shape passes without first checking whether W2/W3/W5 would
  cover the shape.
- Cost heuristics without a stated target.
- Tuning on the check set, or claiming runtime parity from the kernels.

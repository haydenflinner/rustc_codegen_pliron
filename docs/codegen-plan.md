# Codegen plan: generated-code parity with LLVM

Status 2026-10-31. Companion to [`PARITY.md`](PARITY.md) (the scoreboard
and the short work list). This file is the long-range plan for runtime
speed: what LLVM and GCC have that we lack, in what order to build it, and
how to tell whether it worked.

## Where we are

| suite | pliron / LLVM | note |
|---|---|---|
| our 19 kernels, native | ~0.91 | tuned against for three weeks |
| 8 held-out programs, native (`bench/heldout`) | **1.17** | 1.57 on 2026-10-31 morning, 1.31 that evening |
| of which tuned on | blake3 1.15, blind-sig 1.09, pulldown-cmark 1.20, regex 1.20, serde_json 1.16 | 2026-11-01 |
| of which kept untouched | lol_html 1.21, flate2 1.34, prost 1.05 | the check set |

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

Evening (2026-10-31):

- **Second-level `Option` niche folds** (`fold_null_tests` through
  `select (a != 0), a, 0` and single-edge params, on by default): removes
  the per-element null test in slice zip/enumerate loops. Held-out
  neutral (1.232 vs 1.231). It exposed a latent jump-threading miscompile
  (a back edge threaded past a merge block kept that block's stale
  params; indexmap `erase_indices`), fixed in `jumpthread::round`
  (docs/history/2026-10-31-jt-backedge-stale.md).
- **blind-sig's `montgomery` still tests `q == 0` per limb.** The
  remaining compare in the inner `mac_digit` loop is the zip's niche test
  on the *second* slice's pointer, rewritten as `p == 0 - D` (with
  `q = p + D`). LLVM's IR shows how it avoids it: the loop is
  `add_mul_vvw(z: &mut [u64], x: &[u64], ..)`, a real call at codegen with
  `nonnull` slice params, and LLVM simplifies callees with their own
  param attributes *before* inlining them (bottom-up CGSCC). Our IR
  inliner drops the callee's param facts. Marking the clone's entry-block
  params (region-scoped: they exist only inside the inlined body) works
  at inline time (`add_mul_vvw`: leaves 0 and 2), but a later IR pass merges
  the single-pred clone entry and replaces those params with the caller's
  args, and the fact is gone. Moving it to the args is unsound (the
  `carry_keeps_param_facts_scoped` bug). Needed: a region-scoped carrier
  IR passes keep, or a dominance-checked transfer when a param is
  replaced; plus `PLIRON_INBOUNDSNN` for the `q + 8` advance. Now opt-in
  (`PLIRON_INLINENN=1`, with `phisimp` keeping `nnargs` params): the facts
  reach CLIF (montgomery's non-null set grows from 11 to 46 values), but
  the zip's niche test (`v1729 == 0`, single incoming `v23 + 8i`) still
  only folds once `jumpthread` runs its null folds before `merge_chains`
  (which merged the clone entry away). With that, the per-limb test goes
  (montgomery's inner loops 106 -> 52 instructions) but blind-sig gets
  slower, 1.278 -> 1.681 of LLVM: what's left is the zip's
  `TrustedRandomAccess` index loop (`base + i*8` recomputed, a per-limb
  reload of the SmallVec length, latch spills). Needed next: index ->
  pointer strength reduction (W2), and the callee's `noalias` on `z`
  kept the same region-scoped way so the length load hoists. Follow-ups
  tried: opt-in `brifsel` (a `brif c` arg `select c, a, b` is `a`/`b`
  per edge) makes the zip index affine. Inner loop 52 -> 39
  instructions, no spills, one exit test, plus the exit-only length
  reload sunk (`PLIRON_EDGESINK_LOADS`). blind-sig still 1.566 vs 1.270:
  `phisimp` keeping the clone's `nnargs` params alive blocks IR
  simplification wholesale (100 functions change; der's
  `Header::decode` 330 -> 850 instructions). That carrier design was
  wrong and is replaced: the fact now sits on the caller's operand with
  the clone's region (`State::region_nn_op` -> `Pipe::region_nn` ->
  `jumpthread::RegionNN`, checked against the test's block region, like
  `operand_frozen`). Functions now shrink (montgomery -69, add2 -32,
  sub_sign -32) and `INBOUNDSNN` alone is neutral (1.273). But blind-sig
  under `INLINENN` + `BRIFSEL` + edge-sunk loads is still 1.580 vs 1.276. The
  hot loop is index-addressed (about 17 instructions per limb vs LLVM's
  11 with post-increment loads and a down-counter), and 54 functions
  changed. The profile puts all of it in montgomery (+148 of +156
  samples). Its two inner loops are 16 vs 17 instructions but take about
  40% more samples: the base addresses through a pointer IV (`ldr [x10]`,
  `add x10, x10, #8`), while the folded version recomputes both
  addresses from the index each trip (`add x14, x15, x10, lsl #3` after
  a `mov` from `i + 1`). First LSR-style piece, opt-in `ivptr`: rewrite
  `p' = B + s*i` (with `i' = i + 1`, entry `p0 = B + s*(i0-1)`) to
  `p + s`. With INLINENN + BRIFSEL + IVPTR + edge-sunk loads the hot
  loop is LLVM's shape (13 instructions, post-increment loads and
  stores). blind-sig is at 1.329, from 1.58 (base 1.273): montgomery
  +44 samples, mac_digit +11. The `ldr x, [p], #0` writeback was the
  emit-time post-index sweep (fixed, patch 90, neutral alone). With
  `BRIFSEL` limited to selects only the branch uses, blind-sig is **1.209
  vs default 1.274**. pulldown regresses 1.305 -> 1.550 under the same
  flags: `parse_block` 200 -> 442 samples, only with the INLINENN facts
  (a facts-free replay shows no BRIFSEL change). Tried and
  not enough alone: INBOUNDSNN, ASSUMENN, RETNN (MIR inlining removed
  `deref`'s call; smallvec 1.10's heap pointer is a raw `*mut T`).
  Resolved (2026-10-31 night): `PLIRON_CORPUS` headers now carry the
  region facts (`diffcheck-bregion/rparent/rnn`) and clif-check replays
  them. The pulldown regression was `EDGESINK_LOADS`, not BRIFSEL (that
  1.550 predates the BRIFSEL restriction): slot reloads after a call in
  `parse_block`'s byte-scan loop moved onto the loop's exit edge, and the
  scan index lost its register across the call (`ldr/add/str` per byte,
  parse_block 308 -> 667 samples). Loads now sink only onto edges leaving
  the block's innermost loop, from call-free blocks. New `ivdown`: an
  index left only as a trip counter after `ivptr` counts down to zero,
  the counter itself the `brif` condition (`sub; cbnz`); and Cranelift
  patch 91 (flag twins) fuses montgomery's `add` + `cmn` into one
  `adds`. Montgomery's inner loop is now LLVM's 11 instructions. All of
  INLINENN, BRIFSEL, IVPTR, EDGESINK_LOADS and IVDOWN are default since
  92bf1ac (batch 20: gate, crates, fuzz 21200:21600; held-out geomean
  1.208 vs 1.230 with them off; blind-sig 1.275 -> 1.156). With flag
  twins blind-sig is 1.113 (interleaved). Left on blind-sig: SmallVec's
  `extend(repeat(0).take(n))` (28 vs 4 samples): LLVM folds its two exits
  (`len < cap`, iterator left) into one `umin` trip count and stores
  `stp q0, q0` x2 per trip; we unroll x8 with a test per copy. Needed:
  multi-exit trip counts (a counted pre-loop of `umin(...)` trips that
  falls into the original loop for the last exit), then loopidiom or
  loopvec on the single-exit pre-loop. Done (2026-11-01): `mintrip`
  builds that pre-loop (umin of per-exit lower bounds, rotated, counted
  on a unit-step param, exit values rebuilt as `p0 + s*T`), and
  loopidiom/loopvec address by any lockstep param; the fill is a
  2 x i64 loopvec loop and blind-sig 1.113 -> 1.081.
- **Small memcmp** (opt-in `memcmpk`, after `ifconv2`): `memcmp`/`bcmp`
  with a constant `n <= 16` inline (exact sign for 1/2/4/8 via
  byte-swapped loads; others under zero tests only). pulldown's
  `HtmlWriter::run` made ~30 `memcmp(.., .., 1)` try_calls
  (`ends_with('\n')`): 1.309 -> 1.260. Also opt-in
  `PLIRON_INLINE_COLDONCE` (no free single-site inlining of `#[cold]`
  callees: `Vec::reserve` had swallowed its grow path). Its callers in
  other CGUs still see `reserve` as an import with no body: the xcgu
  copier can't copy it because `do_reserve_and_handle` is `#[cold]`
  (excluded) and internal to its home CGU; allowing cold copies wasn't
  enough on its own (still 45 calls), not chased further. memcmpk,
  `bittab2` (with `bittab` looking through single-pred params) and
  `demand` (xor constants under demanded-bits masks, serde_json's SWAR
  scan) went default in c9c6b6d with bytescan: held-out 1.211 -> 1.192
  (pulldown 1.303 -> 1.228, rust-json 1.245 -> 1.167; compression, a
  check-set bench, 1.322 -> 1.344). diffcheck now runs user-symbol
  `memcmp` like the libcall (`Facts::memcmp`): before, every memcmpk
  expansion of a `compare_bytes` call replayed as a false mismatch.
- **Inlining reach** (2026-11-01): `#[inline(always)]` now skips the size
  score like LLVM's AlwaysInliner (blake3's 9k-op `compress_pre` was
  called out of line: 1.190 -> 1.160). Cross-CGU copies can now bind to
  promoted deps: `#[cold]` Internal fns a copyable fn needs are defined
  hidden-external in their home CGU (ThinLTO's promotion), so
  `Vec::reserve` reaches other CGUs and inlines (pulldown 1.246 -> 1.209).
  Promoting every uncopyable dep (`PLIRON_XCGU_PROMOTE=1`) also helps
  json but reshuffles regex's inlining (+4%): with more inlined into
  `follow_epsilons`, the `&mut SparseSet`'s fields are reloaded every
  outer iteration: load PRE (`PLIRON_LOADPRE_DEBUG`) carries them
  around the inner loop and inserts them in its preheader, the block
  after the outer header, but never reconsiders the loads it inserted,
  so the outer header's upward PRE doesn't see them (the `Vec` grow call
  that clobbers them is `nocapture`, `v2` is in `ncc`). Fixed by giving
  upward PRE forwarding's iso/ncc-aware clobber test and rebuilding
  in-loop addresses from a dominating base (regex 1.343 -> 1.205).
  Also from the same profile:
  punroll unrolled a constant 3-trip loop x8 (`trip_max` now knows
  constant bounds; peeling caps at the bound).
- **Early-exit vectorization (W10), first piece**: opt-in `bytescan` vectorizes
  byte-set scans over constant tables (pulldown's `escape_html_scalar`
  -2.2% on dense input; 3.5x on a sparse repro), with table bytes from
  rustc's evaluation of imported statics (`State::extern_bytes`) and a new
  aarch64 `shrn` movemask (Cranelift patch 89). Next: top-tested index
  loops, and runtime tables (pulldown's `LookupTable`, shufti-style).

- **Escape analysis and dead code** (2026-11-01): `isolated_escapes`
  skips blocks the entry can't reach. A folded branch's dead arm (the
  `Vec` grow call behind `SparseSet::insert`'s capacity assert) kept a
  `&mut` param escaped through the last licm; regex 1.218 -> 1.200.
  Remaining regex gap: `cached_state` dispatches on a `band 7` tag that
  Cranelift's `remat.isle` re-emits in each block of the compare
  cascade, and inlining `follow_epsilons` into `next_state` (which a
  looser call count does) costs 40%: the inline model needs a reason to
  keep a large looping callee out of line beyond its op count.
- **Miscompiles found by fuzz** (2026-11-01): seed 24242, `clifpeep::ceval`
  kept an `ireduce`'s high bits and `bitmerge` folded a wrong field
  constant; seeds 26212/26694, slotprom skipped the writeback before an
  sret call whose return value covers only part of the slot (reverted);
  seed 27004, celim's signed nowrap test for a downward offset; seed
  27992, upward load PRE trusting a block that overwrote and reloaded
  the location (`preserves` now requires the entry value to survive). All fixed with regression tests or
  gate seeds. A batch's 400 seeds are too few for
  memory-pass changes: sweep 2000. Seed 31625 (31000:33000 sweep):
  slotmerge's `slot_at` overflowed on wrapped dead-code offsets (ICE).
- **Reload PRE** (2026-11-01, W4's "no reload inside a loop" in its
  narrowest form): at emission, a reload whose register already holds
  the value on some in-edges moves to the others (`PLIRON_RELOADPRE`,
  checker-validated with `PLIRON_RELOADPRE_CHECK=1`). Held-out 1.187 ->
  1.181 in the batch A/B; pulldown -1.5%, json -0.8% interleaved.
  `PLIRON_AMODESPLIT=1` (opt-in) gives serde's escape loop LLVM's six
  instructions but is neutral to negative elsewhere.
- **Next for pulldown: callee simplification before costing.** Its
  `Tree::append_text` (89 samples, LLVM 4) and `scanners::scan_nextline`
  (75, LLVM 0) stay calls. `append_text` lives in another CGU and needs
  the derived `ItemBody::eq` (124 MIR statements, over
  `PLIRON_XCGU_SIZE`=50); at 200 it is copied, but the inliner scores it
  n=313 ops (t=320 at a plain site) because `ItemBody::Text == body`
  inlined the whole `eq`. With `self` a promoted constant, the
  discriminant load and the variant match fold to one compare; LLVM
  sees the folded callee (SROA/instcombine/simplifycfg run on a callee
  before its callers inline it). Ours scores the unfolded body, and
  `PLIRON_INLINE_CLEAN` (DCE + block merge, uncounted) does not fold
  constant loads or switches.

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
   reloads, dead remat-slot stores, reload PRE. Next: split live ranges around calls
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
   Concrete consumers found on 2026-10-31, in the order to build them:
   - *Dereferenceable params* reach loadfwd now (`run_deref`): they make
     PRE loads speculatable. Upward PRE on top of that took regex from
     1.34x to 1.25x. Done.
   - *Exact slice ends*: rustc's `ptr.add(len)` for a slice end is
     inbounds, so `end - begin` is a multiple of the element size and
     `begin <= end`. With it, a pointer IV's `p == end` exit is `p >= end`,
     two exits fold to `p >= umin(a, b)`, and the trip count is exact
     (LLVM's IndVarSimplify + LFTR). num-bigint-dig's zip loops in
     blind-sig: +196 excess compare samples, 3 branches per limb against
     LLVM's one down-counter.
   - *Non-null at the first jump threading*: the `Option<&T>` niche test
     zip/enumerate produce (num-bigint-dig's zip in blind-sig, brotli's
     match loop) merges the pointer from the "has next" path with `0`;
     folding it needs the pointer known non-null when jumpthread first
     runs. Carrying `derived` across indvars' IV rewrite (tried) does not
     reach it: the test is decided earlier. Find which fact is missing
     there (the iterator's `NonNull` field loads, `ptr.add(1)`'s inbounds
     mark) before building a general replace-value hook.
   - *Flow-sensitive capture*: `iso` needs a root to never escape; LLVM
     asks whether it escaped *before* the access (EarliestEscapeAnalysis).
     `ncc` (nocapture-only escapes) covered today's case; a real
     escape on a cold path still disables everything.
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

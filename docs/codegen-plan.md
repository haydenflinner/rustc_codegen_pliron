# Codegen plan: generated-code parity with LLVM

Status 2026-10-31. Companion to [`PARITY.md`](PARITY.md) (the scoreboard
and the short work list). This file is the long-range plan for runtime
speed: what LLVM and GCC have that we lack, in what order to build it, and
how to tell whether it worked.

## Where we are

| suite | pliron / LLVM | note |
|---|---|---|
| our 19 kernels, native | ~0.91 | tuned against for three weeks |
| 8 held-out programs, native (`bench/heldout`) | **1.104** | 1.57 on 2026-10-31 morning, 1.31 that evening, 1.15 on 11-01, 1.111 on 11-02; 1.104 with placement + branch hints on (11-03 batch) |
| of which tuned on | blake3 1.06, blind-sig 1.07, pulldown-cmark 1.16, regex 1.13, serde_json 1.07 | 2026-11-02 (batch 46) |
| of which kept untouched | lol_html 1.19, flate2 1.32, prost 1.01 | the check set; 11-02's fixes did not move them |

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
- **Height reassociation** (2026-11-01, `PLIRON_HREASSOC`, on): blake3's
  G computes `a + b + m` with `b` fresh off a rotate; `(a + b) + m` put
  two adds on `b`'s path per `a` update (LLVM ranks operands and adds the
  message word first). Found by elimination: same ALU mix, stack traffic
  removal (LOADREMAT) and LLVM without its schedulers changed nothing.
  blake3 1.156 -> 1.051; held-out 1.166 -> 1.153 (batch 36). Fuzz
  35800:38600 (2800 seeds) clean.
- **Late null-test fold** (2026-11-01, `PLIRON_NFOLD2`, on): regex's
  `InstPtrs` varint loop tested its byte cursor against null on every
  byte (the `Option<&u8>` niche of `slice::Iter`). The test only becomes
  a bare `p == 0` once webmerge collapses `select c, p, 0` late. By then
  sameargs had rebuilt `p + 1` without its inbounds mark, and the
  `p + nread` advance's recorded base was gone. Step `nfold2`, after
  webmerge, uses two proofs that don't depend on those marks: an edge's
  `x + k` after `x` was dereferenced on every path to it, and either
  operand of an inbounds `iadd` whose base is a tombstone. Regex
  1.223 -> 1.179 in batch 37; the rest within noise.
- **Switch facts on the switch, not in a value** (2026-11-01,
  `PLIRON_SWITCH_RAW`, `PLIRON_SWITCH_KEEPDEAD`, on):
  `PLIRON_SWITCH_MASK` encoded "out-of-range is UB" as a padded table
  and `band x, 2^k-1`. That was an x64-only clamp elision; aarch64 paid
  the `and` and kept its clamp. After brchain turned a table into a
  compare cascade, Cranelift rematerialized the `and` in each cascade
  block (regex `cached_state`: four per element). Now a dead-default
  switch dispatches on the raw tag. unreach keeps the `unreachable`
  default of a `br_table`, redirecting only dead entries. brchain treats
  that default as a don't-care: the chain's else edge takes the first
  entry's target, and choosing the most common target instead cost regex
  2.3% by testing its hot arm last. Vendored Cranelift drops the clamp of
  a table whose default is `trap user2` (aarch64: `cmp; b.hs; csel;
  csdb` -> `mov`). Regex: 1016 fewer instructions, 44 of 94 clamps gone;
  timing neutral on all held-out programs (held-out 1.153 -> 1.150 in
  batch 38). Fuzz 39400:42200 clean.
- **Small switches as compare chains** (2026-11-01, `PLIRON_SWITCH_CHAIN`,
  on): Cranelift's `Switch` (and dense_switch from four cases) turns
  every contiguous case run into a guarded `br_table` on `ireduce(x -
  lo)`. brchain then turned tables with at most four live entries into
  compares on that derived index, so case values never reached `x`. Up
  to four cases now lower to a chain on `x`, with consecutive values as
  `x - lo <=u w`; bittab reads that offset form. regex-syntax's
  `Utf8Sequences::next` went from 572 to 443 instructions (LLVM 376), its
  bounds checks are gone and its `for i in 1..4` loops unroll. Held-out
  1.157 -> 1.150 (batch 39); regex 1.214 -> 1.169, compression 1.336 ->
  1.289.
- **pulldown, per source line** (2026-11-01): with the debuginfo path fix,
  both compilers' line tables let samples be compared per pulldown source
  line despite different inlining (`scratchpad lines.py`). The first pass's
  special-byte loop (`parse.rs:2839-2840`) is as tight as LLVM's. The
  difference is the out-of-line `parse_line::{closure#0}` on every special
  byte: register spills, a 40-byte zeroed result slot and a memory return.
  The closure (n=4594) misses the single-site cap (10x limit). Raising it
  (`ONCEX` 16/24) inlines it, but then the grown scan function stays out
  of line (n=6582) and its epilogue costs as much (`parse.rs:2853`, 63
  samples). Net pulldown -0.4%, regex +1%, so not kept. Also measured and
  dropped: `FANIN=0` (pulldown -0.5%, regex +4%), and sizing callees
  without free ops (their share is 20-38%, too uniform to change
  decisions). LLVM gets the whole nest into `parse_line` because it sizes
  callees after simplification. That is the remaining lever: per-callsite
  specialized costing, or callee simplification calibrated against
  thresholds.
- **Layout noise and the inline policy search** (2026-11-01): relinking
  the same objects under a random function order (`bench/heldout/run.py
  --layouts K`) moves held-out ratios by ~3%, and a byte-identical hot loop
  ran 50% slower in one placement, so single-layout A/Bs under ~3% are not
  evidence. Scored that way (geomean of five tuning benches, four layouts
  each, interleaved against default), callsite-specialized costing
  (`PLIRON_INLINE_SPEC`), callee simplification (`PLIRON_INLINE_SIMP`,
  branch folding alone or the subset without SROA/const casts) and their
  combinations all land within +/-0.5% of default: each wins some benches
  (json, blind-sig, pulldown about -1.3% with the subset) and loses others
  (regex +4%, real in every layout). The inline policy is near a local
  optimum for this backend. Where inlining matches LLVM's, our code is
  bigger: regex `next_state` has the same call set, 658 vs 533
  instructions and 40 vs 21 stack refs, mostly `mov` (+44), jumps (+18),
  spill loads/stores (+20) and re-extensions of an `i16` param at every
  compare (+16 `uxth`/`uxtb`; Apple's ABI already zero-extends it). Across
  the matched hot functions `mov` dominates: the gap left is backend code
  quality (regalloc moves, edge-split blocks), not decisions.
- **The three backend items, measured** (2026-11-01, layout-averaged,
  five tuning benches):
  1. *Narrow re-extension.* Not the ABI: the Rust calling convention marks
     no small-int arg extended, so a "param arrives extended" rule never
     fires. Extending once (`uextend` placed at the def, compares moved to
     it) cut regex `next_state`'s `uxth` 19 -> 13 but kept both registers
     live (stack refs 40 -> 56); neutral. Doing it right needs Cranelift to
     track clean high bits across uses. Dropped.
  2. *Edge-split move blocks.* 834 in regex vs LLVM's 399, but ~0% of
     samples (cold paths). Code size only.
  3. *Register copies.* The `mov` sample share (pulldown 9.2% vs 6.5%) is
     mostly skid onto block-leading `mov`s after taken branches. The real
     ones are back-edge copies: `latchspec` hoists `i + 1` above a scan
     loop's exit test, so the new value needs its own register (`mov x8,
     x10; b loop`, no branch saved). `PLIRON_LATCHSPEC=0` removes them
     (pulldown -95 reg moves), and so does hoisting only when the
     consumed values are dead on the exit side; both are neutral overall
     (geomean 1.002 / 1.004 over 8 layouts; pulldown -0.7%, regex +2%).
     Apple's cores eliminate `mov` at rename, so copies are cheap here.
  Spills are the one item left that costs real work (stack refs 40 vs 21
  in `next_state`).
- **Spills** (2026-11-01): `next_state`'s are cold (1 of 155 samples). Where
  they cost: stack accesses plus the next instruction are 13% of pulldown's
  samples vs LLVM's 6.5%, json ~5% of runtime; regex and blind-sig match.
  They are ordinary values (pulldown value stores 46 vs 8 samples), not
  constants, and not the spill weighting (upstream `RA2_LINEAR` changed
  nothing). Two allocator weaknesses:
  1. *Repeated reloads*: regalloc2 reloads before each use, often into a
     fresh register (`handle_inline_pass1`: `[sp, #0x3c0]` into four
     registers in a dozen instructions). Reload CSE at emission
     (`PLIRON_RELOADCSE`, through single-predecessor blocks) turns 206 of
     pulldown's stack loads into register moves; geomean 0.996 over six
     layouts; checker-clean.
  2. *Parked registers*: in pulldown's hot loops up to eight callee-saved
     registers hold outer values unused inside, while the loop's
     invariants spill (in-loop stack traffic 363 samples, 191 in loops
     with parked registers). Splitting every bundle around a use-free
     deeper loop span before allocation (`PLIRON_RA2_LOOPSPLIT`, not
     landed) cut those to 215 / 107, but the pieces often land in
     different registers: +3,116 moves, +4% code, geomean 1.006 (regex
     +2.5%). The narrower trigger (a conflicting bundle that lives through
     the conflict's loop without a use there is evicted at ~0 cost and
     split around that loop when requeued) adds only +0.5% code and cuts
     in-loop stack traffic 514 -> 364 samples, yet geomean is 1.003
     (pulldown -0.3%, regex +1.1%). On these cores spill reloads are
     cheap (L1, store forwarding); stack-traffic samples overstate them.
     Not landed; patches kept.
  3. *Stored-back invariants* (2026-11-02): a split loop range reloaded
     a loop invariant and stored it straight back each iteration. Spill
     CSE (`PLIRON_SPILLCSE`, on; PLIRON_PATCH #94) drops a spill of a
     register known to hold that slot, and the reload-CSE facts now
     flow through every block as an intersection over predecessors,
     iterated to a fixpoint, so they survive loop headers. pulldown -394
     spill stores; geomean 0.987 over six layouts (regex -3.7%, json
     -2.2%). The cheap-reload lesson above still holds for loads; dead
     stores were what cost.
  json's extra traffic is mostly results returned through memory after
  calls, a separate item.
- **Measurement note** (2026-11-02): the quick six-layout A/Bs quoted
  below for join threading, bittab canon, spill CSE and the LICM knobs
  ran against a stale `base` (a zsh `nomatch` abort made the cleanup a
  no-op), so their magnitudes include earlier flips. The authoritative
  numbers are the held-out batch A/Bs (same build, knob forced off):
  invparam 1.149 -> 1.142, join threading + canon 1.144 -> 1.142 (json
  1.150 -> 1.135), spill CSE 1.140 -> 1.134, ESC_TRAP 1.132 -> 1.131
  (blind-sig 1.108 -> 1.075), SYMSUB 1.133 -> 1.121 (json 1.129 ->
  1.069). Rejections stand: a stale base favours the variant.
- **Option-tag re-tests** (2026-11-02): serde_json's `parse_whitespace`
  inlined into a loop merges `peek()`'s tag and byte, the loop tests the
  tag, and the loop's exits join to test it again for the caller. That
  second use kept the merged tag live past the merge block, so jump
  threading never threaded the merge (13 instructions and three back
  branches per byte vs LLVM's 6). `PLIRON_JOINTHREAD` (on) folds a
  `brif` whose block is reached only through one arm of a `brif` on the
  same value, and retargets jumps into a block that is only such a test;
  the merge then threads. `PLIRON_BITTAB_CANON` (on) lets bittab compare
  leaf args through single-predecessor params, so the whitespace chain
  becomes LLVM's bit test. regex -3.5%, json -2%, geomean 0.991.
- **Single-site inline threshold** (2026-11-02): pulldown's
  `parse_line` closure (4,594 raw ops, one site) misses the free
  threshold (10 x limit = 4,200), so its scan loop pays a call, seven
  spills and a result through memory per special byte. LLVM inlines it.
  `PLIRON_INLINE_FREEMUL=12` inlines it, but the enclosing
  `scalar_iterate_special_bytes` then stays out of line with a
  register-starved loop: pulldown +2.2% (with spill CSE, -1% vs -0.6%),
  regex/json -2-3% from other inlines. Left at 10; the real fix is
  sizing the simplified callee (see below).
- **SparseSet field reloads in regex** (2026-11-02): `follow_epsilons`
  and `next_state`'s state-decode loop reload a `&mut SparseSet`'s five
  fields per element; LLVM keeps them in registers. Two reasons LICM
  saw the param as escaped: `insert`'s dead `Vec::push` grow call takes
  it (`len < cap` was just asserted; `celim` folds the test, but after
  the last LICM), and `next_state` hands `qnext` to the out-of-line
  `follow_epsilons`, outside the loop. `PLIRON_LICM_DOMCOND` (dominated
  folds before LICM) and `PLIRON_LICM_NCC` (a root escaping only through
  `nocapture` call operands is isolated in loops holding none of them;
  test `licm_ncc.rs`) fix both and the fields hoist, but `next_state`
  goes from 41 to 63 stack references: geomean 1.002 for DOMCOND alone,
  1.006 with NCC (regex +1.5%). Both opt-in. Same lesson as loop
  splitting: an L1 reload is cheaper than the spill that replaces it
  under register pressure; hoisting needs a pressure check.
- **Panic-message slots** (2026-11-02): num-bigint-dig's `montgomery`
  formats `&x, &y, &m` in an `assert!` message, so rustc keeps each in a
  slot and every use reloaded it; the slots escape only into the panic's
  arguments. `PLIRON_ESC_TRAP` (on) lets loadfwd ignore escapes in
  dead-end trap blocks that load nothing (forwarding only; DSE and
  promotion keep the strict set). The params' SmallVec inline/heap tests
  then hoist out of the outer loop: blind-sig 1.108 -> 1.075 held-out.
  Its remaining outer-loop work includes `(len*8)>>3` as a real `mul`:
  Cranelift's egraph is skipped for any function holding an i128 value
  (`egraph_safe`, ISLE prelude panics). Running it anyway (no panic on
  blind-sig) made blind-sig 20% slower: it breaks the
  `isplit (imul (uextend) (uextend))` shape the backend lowers to
  mul+umulh. Keep the guard; port the few wanted rewrites instead
  (`imul x, 2^k` -> `ishl` alone measured neutral, not landed).
- **Static-string copies** (2026-11-02): counting libc calls with a
  `DYLD_INSERT_LIBRARIES` interposer (same mallocs/reallocs as LLVM)
  showed 137k memcpy calls in pulldown vs LLVM's 115k and 189k vs 70k
  in json, all 1-14 bytes. A `&'static str`'s length arrives as
  `(sym + n) - sym` from two `symbol_value`s GVN keeps apart, so the
  copy's size never became a constant and `memcall` never expanded it.
  `PLIRON_SYMSUB` (on) folds the difference: json 1.118 -> 1.058,
  pulldown -0.9%; memcpy calls now 115k / 54k. With it, memcpy, memset
  and bcmp counts match LLVM on all five tuning benches.
- **Dead stores are cheap** (2026-11-02): three ways of deleting stores
  nobody reads all measured neutral with fresh bases: post-RA dead spill
  stores (`$S/deadspill.patch`, not landed; 69-134 per bench), slotprom
  writebacks of undefined or just-reloaded values before bounded calls
  (`PLIRON_SLOTPROM_UNDEFPARAM`, opt-in; up to 1,526 insts in regex,
  including the zeroing of `item_to_event`'s sret slot), and the
  i128-function identity folds (the final CLIF was already clean). Spill
  CSE won because it also removed in-loop reloads. Also neutral on
  current defaults: inline limit 256/400/480 (320 stays), REMATW,
  CONSTSINK, ELABLOOPS, XBLOCK, REMATLOOP, LIBCALL_DIRECT, BYVALHOME,
  DEADARG, RETNN, CSE_SYM; LUTCORR miscompiles blind-sig (opt-in).
- **Cross-CGU copy limit** (2026-11-02): `PLIRON_XCGU_SIZE=100` (default
  50) makes pulldown 2.4-2.6% faster in two layout-averaged runs (1.177
  -> 1.146), but blind-sig +5%, json +2.3%, regex +1.4%. In pulldown it
  is a global inlining reshuffle (203 extra copies, many from core):
  `scalar_iterate_special_bytes` goes out of line with the `parse_line`
  closure inlined into it, `Tree::append` and the code-fence scanners
  inline, `parse_paragraph` splits out; profile total -7%, closer to
  LLVM's shape. Bisecting the copies fails to link (copies depend on
  each other: `is_ascii::runtime`); the ascii helpers alone do nothing.
  Next: find which copy unlocks the closure inline, or size copies by
  their simplified body. blind-sig's loss (warm loop 1.057 -> 1.117) is
  not montgomery (slightly faster) but helpers going out of line:
  `mac::long` replaces `mac_digit`, and `Cloned<Iter<u64>>` folds and
  SmallVec `clone`/`extend`/`try_reserve` become calls. Extra local
  copies change which callees look single-caller.
- **blind-sig's real gap** (2026-11-02): the held-out binary times one
  ~2 ms sign; whole-process profiles (setup included) showed parity, so
  earlier montgomery analysis read the wrong code. A copy of the bench
  with `start..end` in a loop gives the same ratio warm (1.067 vs 1.065
  cold), and its profile shows the gap: one of montgomery's two inner
  multiply-accumulate loops keeps its source pointer in a stack slot,
  `stur` after the post-increment and `ldur` on the back edge every
  iteration (store-forwarding latency on the loop-carried chain, plus a
  `cbz`/`b` back edge); LLVM's loop has no stack access. The CLIF loop
  carries four values. With `PLIRON_POSTIDX=0` the spill moves to the
  other loop (blind-sig -0.9%, blake3 +0.6%); with `PLIRON_ESC_TRAP=0` it
  disappears but the outer loop loses more. Likely the pointer's bundle
  is merged with long out-of-loop ranges, its weight diluted, then split
  per use. Upstream linear spill weights (`RA2_LINEAR` build) keep the
  spill and the timing (1.065), so it is plain pressure: ~20 outer-loop
  values (including the six SmallVec selects ESC_TRAP lets LICM hoist)
  live across inner loops that need 8-9 more, against our 26 allocatable
  integer registers (Cranelift reserves x16/x17 as emit scratch; LLVM
  allocates them, 28). Levers: give regalloc2 x16/x17 (emit-time users
  need their own scratch), or pressure-aware LICM that leaves a cheap
  invariant load in an outer loop whose inner loops are register-bound.
  The second, crudest form landed as `PLIRON_LICM_INNERPRESS` (on): in a
  loop with inner loops, a load no inner loop uses is not hoisted. The
  montgomery spill is gone (warm loop 1.062 -> 1.054); tuning geomean
  0.997 / 0.998 in two runs.
- **Tonight's flips together** (2026-11-02, held-out, 6 layouts x 7
  rounds, one interleaved run vs all six forced off): geomean 1.141 ->
  1.122; json 1.152 -> 1.070, blind-sig 1.096 -> 1.070, regex 1.150 ->
  1.126, pulldown 1.185 -> 1.168. compression, html-rewriter and
  protobuf did not move: their gaps have other causes.
  Lesson: the mask had to be undone by every pass that reshaped the
  table. Encoding the fact where it belongs (the switch's default) made
  each consumer simple.
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
  Tried (2026-11-01, `PLIRON_INLINE_SIMP`, opt-in): the bottom-up
  inliner simplifies each finished caller before scoring it (phisimp,
  SROA, const-global loads, const casts/compares, instcombine, constant
  branches, DCE). `append_text` drops to n=99 and inlines at all 19
  sites (with `PLIRON_XCGU_SIZE=200`), yet pulldown does not speed up:
  its call overhead was not the cost. Globally it reshuffles inlining
  (serde's escape loop lands in `serialize_field`, json +8-18%; early
  SROA alone costs pulldown +10%); `=3` (loop-free callers only) is
  pulldown -2%, json/regex/blind-sig +1-3%. Left off; the pieces
  (`run_fn` per pass, `PLIRON_SIMP_PARTS` bisect mask) remain.
- **More allocatable registers** (2026-11-02, vendored Cranelift patch
  95, opt-in): `PLIRON_X17=1` allocates x17 (its post-regalloc users are
  prologue-only, `LoadAddr`'s large-offset path now uses `rd`, and the
  26-bit branch veneer saves it below sp), `PLIRON_X30=1` allocates the
  link register (clobbered by every call; a function that uses it gets a
  frame record). That is 28 registers to LLVM's 29. Tuning geomean x17
  1.006, x30 1.007, both 1.001: montgomery drops one spill pair, nothing
  else moves. With INNERPRESS in, register count does not limit these
  benches; this lever is closed for now (x16 would need emit-time
  scratch for large offsets, i.e. register scavenging).
- **`parse_line` closure, again** (2026-11-02): `PLIRON_INLINE_FREEMUL=12`
  now gives pulldown -2.0% and -2.3% in two layout-averaged runs
  (before tonight's flips it was +2.2%; spill CSE removed the scan
  loop's reload/store pair). The other four move +-1.5% between runs
  (geomean 0.991, then 1.003). 14 and 16 are no better (pulldown -1.4%,
  -1.6%).
- **Why bigger cross-CGU copies hurt blind-sig** (2026-11-02): at
  `PLIRON_XCGU_SIZE=100`, `SmallVec<[u64; 4]>::extend::<Cloned<..>>`
  (598 ops) is copied into all nine num-bigint-dig CGUs. It is too big
  to inline at any site, so every caller calls its CGU's copy, and in a
  copy its own callees `reserve`, `size_hint` and the `Cloned` fold have
  no body (they live in the home CGU): "reject (ineligible)" in the
  inline log, out-of-line calls in the profile. The home definition has
  them inlined. Copies that stay called are therefore sometimes worse
  than the home definition, and sometimes better: sending every called
  copy home (`PLIRON_XCGU_HOME=all`) halves blind-sig's loss at 100
  (+5.3% -> +3.5%) but also pulldown's gain (-3.9% -> -1.6%), since a
  local copy feeds spec/sret2reg/deadarg and the internal call ABI.
  `PLIRON_XCGU_HOME=1` sends home only *degraded* copies (calls a fn of
  this crate whose body is in another CGU and not copied here); test
  `test.sh xcguhome`.
  On held-out (batch 47) `PLIRON_XCGU_HOME=1` and `FREEMUL=12` together
  give 1.125 -> 1.121, each alone off 1.125; both on by default.
  What is left of blind-sig's loss at 100 is inlined degraded copies:
  `SmallVec::clone` inlines a copied `from_iter` whose `Cloned::next`
  has no body in that CGU, so the copy loop calls it per element.
  `PLIRON_XCGU_HOME=skip` (opt-in) makes no degraded copy whose home is
  linkable (iterated: dropping one can degrade another). Five benches x
  3 layouts x 2 pads: at 100 it fixes blind-sig (+2.9% -> +0.6%) and
  keeps pulldown's gain (-2.6%), but regex +2.4%, json +1.6% (geomean
  1.0045); at 50 it is 1.016 (pulldown +2.6%, regex +3.7%): inlining a
  degraded body usually still wins. "Degraded" is too coarse; a callee
  missing inside a copy's loop would be the finer test.
- **Placement: code order, stack offset, alignment** (2026-11-02). Four
  tuning benches, each binary run at four environment pads (moves the
  initial stack; `bench/heldout/run.py --pads`), LLVM at the same pads:
  - Stack offset alone moves one binary 1-5% (LLVM 1-2%), as much as
    function order; random function orders never vary it, so the
    `--layouts` A/Bs understated noise.
  - Profile-guided hot-first function order (`scratchpad place.py`,
    ld64 `-order_file` by samples) was best or tied in the first run
    (json 1.078 -> 1.064, regex 1.144 -> 1.139) but json did not
    reproduce; regex gained twice. Worth ~0.5%, mostly by avoiding bad
    layouts. `PLIRON_FNORDER=1` (loop fns first, `#[cold]` last within
    an object) changes nothing: ordering within one object is too
    local.
  - Loop-head alignment above 16 hurts (`PLIRON_LOOPALIGN`: regex +2.5%
    at 32, +4% at 64; pulldown +2.5% at 64: NOPs on every inner-loop
    entry). Loop alignment off (`PLIRON_ALIGN=0`) and 64-byte function
    alignment (`PLIRON_FNALIGN=64`) are within +-1.5% across 3 layouts x
    4 pads (FNALIGN=64 looked -0.6% in one layout, then regex +2.4%
    across layouts). No alignment setting is systematically better; the
    remaining sensitivity is which hot code and data alias, not
    alignment. Next: a cold text section (`#[cold]` and panic-only fns
    out of `__text` across all objects), and pads in every A/B.
- **pulldown, warm loop by function** (2026-11-02, `scratchpad warm-pd`,
  `push_html` looped; 1.15x LLVM): escaping is even (`escape_html_scalar`
  1544 vs 1631 samples). The excess is the special-byte scan with the
  `parse_line` closure inlined (FREEMUL=12) plus `parse_block`: 1956 vs
  LLVM's `parse_line` + `new_with_broken_link_callback` 1606; `Parser::next`
  +130, `item_to_event` +99, and two scanners LLVM inlines
  (`scan_nextline` 278, `scan_closing_code_fence` 262). The hottest
  single spot (parse.rs:2853, 268 samples on one instruction) is a stack
  reload heading the per-special-byte dispatch block, right after the
  LUT test's branch: register pressure in the inlined closure, the same
  pattern as regex's `exec_byte`.
- **regex, warm loop by function** (2026-11-02, `scratchpad warm-rx`,
  start..end looped, 1.14x LLVM like the held-out run): `next_state`
  (with `exec_byte` inlined) 1496 vs 984 samples, `Utf8Sequences::next`
  470 vs 269, `cached_state` 1725 vs 1537; `follow_epsilons` is even.
  In `next_state`:
  - the `for &ip in qcur` loop keeps `b: Byte` in a stack slot and
    re-tests `b == EOF` per `Bytes` state; LLVM keeps the byte in w22 and
    unswitches the test. The loop calls `follow_epsilons`, so everything
    live across it needs a callee-saved register.
  - the state-decode loop (`for ip in state.inst_ptrs() { qcur.insert(ip) }`)
    reloads qcur's `dense` pointer, `sparse` pointer and length every
    iteration; LLVM loads them once. Two blockers, in order: the dead
    `Vec::push` grow call (`len < cap` asserted, then `len == cap`)
    leaves qcur escaped -- `PLIRON_LICM_DOMCOND=1` folds it (verified in
    the CLIF) -- and then INNERPRESS refuses the hoist (the varint inner
    loop doesn't use the fields). With both (`DOMCOND=1 INNERPRESS=0`)
    the reloads go (12 -> 0) but stack loads rise 15 -> 27 and the
    varint loop grows `cmp`/`csel` triples (the iterator's `ptr == end`
    select re-derived on the path where `cbnz` already proved it
    false); no faster. Needs: selects folded under a dominating
    `brif select(c, 0, p)` with `p` nonnull, and INNERPRESS limited to
    loads whose loop has calls.
  - the `b == EOF` test is `v13 = icmp eq b, 256` at entry; the qcur
    loop branches on it per `Bytes` state, and lowering re-derives it
    from the spilled `b` each time. Unswitching skips it twice: the test
    doesn't run every iteration (an opt-in "any invariant brif" variant
    was tried), and the loop's closed region (everything after the
    loop, which only the loop reaches) is "too big" to clone. LLVM
    clones just the loop. Small win even if done: `b` stays live across
    `follow_epsilons` for the byte compare.

## Paused here (2026-11-02, 16:30 EST)

Where the register-allocation work stands, for whoever picks it up.

**Landed on main (all opt-in unless noted; validated by gate, fuzz with
`PLIRON_RA_CHECK=1`, held-out):**
- Defaults flipped today: `PLIRON_XCGU_HOME=1`, `PLIRON_INLINE_FREEMUL=12`
  (held-out 1.125 -> 1.121). Current held-out geomean: **1.121**.
- `PLIRON_RA_EXPLAIN=<symbol substring>` (debug): regalloc2 logs every
  split, eviction (with spill weights), requirement conflict and
  minimal-bundle fallback for matching functions as `ra2x:` lines, plus
  their pre-regalloc VCode. **Use this before guessing why a value is
  reloaded in a loop**: find the vreg in the VCode (e.g. `%v196`), then
  `grep 'v\[196\]'` the `ra2x:` lines.
- `PLIRON_RA2_LOOPDEPTH=1`: loop depth from dominance-checked back edges
  only. The stock estimate counts cold paths that jump back into hot code
  as loops (regex `next_state` reached "depth" 16; 12 bundles saturated at
  the weight cap, so ties picked who kept a register).
- `PLIRON_RA2_LOOPSPLIT=1`: when a bundle must split, split at the entry or
  exit of the innermost cycle around its hottest use (shortest back edge
  covering it), without trimming the loop piece. pulldown's byte-scan loop
  becomes LLVM's 6-instruction shape with no stack access.
- Also opt-in from today, measured neutral: `PLIRON_SELFACT`,
  `PLIRON_LICM_INNERPRESS=2`, `PLIRON_UNSWITCH_LOOP`, `PLIRON_X17`,
  `PLIRON_X30`, `PLIRON_FNORDER`, `PLIRON_LOOPALIGN`, `PLIRON_FNALIGN`,
  `PLIRON_XCGU_HOME=skip` (details in docs/loop-unswitch-pressure.md and
  the entries above).

**Last measurement (batch 50, held-out, 3 layouts x 2 pads):** both RA
knobs on vs off: geomean 1.121 vs 1.121. pulldown -3.0%, html-rewriter
-0.9%, json -0.7%; regex +1.3%, protobuf +2.3%, blind-sig +0.6%. Not
flipped.

**Next steps, in order:**
1. Branch `wip/ra-cycle-index` (pushed, d884aa1, unverified): replaces
   the loop split's per-use scan of every back edge (quadratic on huge
   functions) with a per-block innermost-cycle table. Check it gives the
   same allocations as main (compare disassembly ignoring addresses and the
   file-name header line), then gate + fuzz. Required before any default
   flip of `PLIRON_RA2_LOOPSPLIT`.
2. Run `PLIRON_RA_EXPLAIN` on protobuf's and regex's hottest functions
   with both knobs on vs off, and find which split makes them slower.
   Candidates seen so far: in regex's unrolled DFA loop the split adds
   register copies on the hot path (values used both in the inner cycle
   and in the search loop at the same depth). A "split only when the rest
   of the bundle is colder" rule fixed that but cost pulldown (its scan
   loop and special-byte handler share a header, so loop depth can't tell
   them apart); a frequency-aware cost (reloads saved in the cycle vs
   moves added at its boundary edges) is the likely fix.
3. If the losers are fixed, flip `PLIRON_RA2_LOOPDEPTH` and
   `PLIRON_RA2_LOOPSPLIT` in separate commits through the usual batch
   (gate, crates, fuzz with `PLIRON_RA_CHECK=1`, held-out with an old
   variant).

**Measurement notes:** single-binary warm-loop timings move 1-5% with
code placement and stack offset alone; use `evalcfg.sh` (layouts x pads)
or held-out `--layouts 3 --pads 0,2040` for decisions. regex is the
noisiest bench (layout spread up to 10%).

## 2026-11-02/03: the three frameworks as opt-in steps (W5, W3, W2)

Built in one session, each behind a knob, measured on the five tuning
benches (`evalcfg.sh`: 3 relinked layouts x 3 rounds x 2 stack pads,
ratios to the same-day base build). All three pass the regex corpus
diffcheck (1917 functions) and their filetests.

**W5 placement** (`PLIRON_PLACE=1`, Cranelift patch 98): emission-time
chain placement with static probabilities (`machinst/placement.rs`), with
`PLIRON_BRHINT=1` (last CLIF step) supplying compare-cascade and zero-test
hints through `Layout::set_unlikely_arm`. What the A/Bs taught:

- Emission-time loop rotation is a loss (1.012): it costs a taken jump per
  loop entry; `looprot`'s IR-level rotation gets a fallthrough guard
  instead. Off by default (`PLIRON_PLACE_ROT=1`).
- A 50/50 tie on a compare cascade (a `br_table` turned chain) must keep
  the chain contiguous: without the hint regex lost 5-10%
  (`follow_epsilons`). With `brhint`: regex neutral.
- Chain heads must not be padded when entered by a conditional
  fallthrough (directly or through an empty edge block): the NOPs turn
  the fallthrough into `b.cond head; b other` — a taken branch per
  iteration.
- Result: 0.993-0.995 geomean of base on the tuning set (blind-sig -2%,
  pulldown -1%, json -0.6%). Full held-out batch (11-03, 5 rounds x 3
  layouts x 2 pads, release dylib): 1.111 -> 1.104; the check set moved
  with it (html-rewriter 1.199 -> 1.172, protobuf 0.973 -> 0.977,
  compression 1.292 -> 1.300), json 1.064 -> 1.030, regex 1.118 -> 1.140
  (its layout spread is 1.05-1.23, so within its noise). With vrp and
  ivexit added: 1.100. Gate (both default sets) and 400 fuzz seeds clean.
  **Flipped on** (`PLIRON_PLACE=0`, `PLIRON_BRHINT=0` restore); vrp and
  ivexit stay opt-in (no measurable runtime gain, vrp costs 0.8% compile
  time).

**W3 ranges** (`PLIRON_VRP=1`): `ranges.rs`, one memoized interval engine
(unsigned + signed, definitions and dominating `brif` facts) and `vrp.rs`
as its first consumer (icmp/brif folds, redundant masks and extensions,
urem/udiv below the divisor, carry-free overflow checks). regex corpus:
139 of 1917 functions change, -86 instructions net; with `celim` off,
vrp recovers 60% of celim's effect (celim's remaining edge is symbolic
facts, `x <u y`, which constant intervals cannot carry). Runtime on the
tuning set: neutral (within +-1%). Compile time: 81 ms of 10.4 s on the
regex corpus (0.8%). Three miscompiles found by `PLIRON_DIFFCHECK=step`
on blind-sig before it was clean: an unmodeled type (i128) read as a
1-bit range, a band fold that used the bound as the bit cover, and
sign-extended narrow constants. Lesson: every rule needs the width in
hand, and `diffcheck=step` on a real crate catches what the corpus
replay's random inputs miss.

**W2 SCEV-lite** (`scev.rs`): add recurrences per header param
(constant or invariant step), affine exit tests with their net step, and
unit-step symbolic trip counts, as a shared analysis. The census over the
regex corpus (`PLIRON_SCEV=1`, `PLIRON_TRACE=pliron::scev`): 2005 loops,
1470 with a constant-step IV, 744 with a computable trip count, 133
same-step IV pairs after `ivrefold`, 19 loops with two exit tests on
distinct IVs (the LFTR case is rare here). First consumer `ivexit`
(`PLIRON_IVEXIT=1`): exit-value replacement of a recurrence that is only
returned (`q = p + (q0 - p0)` at the exits, forwarding params stripped).
7 loops in regex; `next_state`'s counter also feeds a second header slot
(previous-iteration value) and stays. Runtime: neutral on the tuning set.

Where this leaves the plan: the frameworks exist and are correct, but
the runtime gains on the held-out programs are small (placement ~0.5%,
the rest noise), consistent with the 10-31 census: the gap is calls,
frames, loads and spills, not branch layout or range facts. The next
consumers worth building on them are the ones that touch those: trip
counts for `willreturn` and unroll decisions, ranges for `dem64`-style
narrowing of address arithmetic, and placement's frequencies exported to
regalloc2 as spill weights (today it uses its own loop-depth estimate).

## 2026-11-03: placement frequencies as spill weights (W4 x W5)

The first cross-framework consumer: `placement::frequencies` (the loop
forest, edge probabilities and packaged-loop frequencies split out of
`place`) runs before regalloc (`VCode::compute_block_freq`) and reaches
regalloc2 through `Function::block_hotness`; `CFGInfo::hot_bonus`
replaces `1000 * 4^depth` in the spill weights (`PLIRON_RA2_FREQ`,
regalloc2 patch log; Cranelift patch 99). What the A/Bs taught:

- Raw frequencies (modes 1/2, floored at 1/64 of the entry, additive
  def/constraint bonuses) fail where the hot code sits behind a cold edge.
  regex's `Fsm::next_state` is a fast-path return and a cold slow path
  holding the loops; every block was below the entry, the floor flattened
  them, the constant bonuses decided, and the state index went to the
  stack (`PLIRON_RA_EXPLAIN` showed bundle weights of 45..271 against
  millions). Tuning set: neutral; regex +2%.
- Normalized per function (modes 3/4: `freq / max` onto the depth
  scale's top, bonuses multiplicative): tuning set 0.996 (mode 3), 0.986
  with `PLIRON_RA2_LOOPSPLIT=1 PLIRON_RA2_LOOPDEPTH=1` (pulldown -2.5%,
  regex -2.3%, every bench down). The multiplied depth fallback overflowed
  a u32 move cost on blind-sig (capped at the depth scale's ceiling).
- Held-out (8 programs, quiet machine, 5 rounds x 3 layouts x 2 pads):
  1.126 -> 1.123 for mode 3 alone and with the split knobs. pulldown,
  compression and html-rewriter -2%; regex, json and protobuf +1%. The
  same split as the 11-02 RA batch, so the frequency weights do not
  resolve why those three lose from loop-aware allocation. Gate and 400
  fuzz seeds with `PLIRON_RA_CHECK=1` clean. **Opt-in.**
- Note: the held-out base moved from 1.104 (11-03 morning) to 1.126 in
  the afternoon with an identical build (protobuf 0.977 -> 1.068, LLVM's
  own times moved too); compare variants only within one run.
- `RA2STAT` lines now carry the symbol (`pliron_set_fn_name` from the
  lowering), so per-function spill/reload diffs between variants are one
  script (`ra2stat_regex_*.txt` in the session scratchpad).

## 2026-11-03 (evening): why frequency spill weights were a wash, and mode 5

The regex side of the mode-3 split has a mechanism now (`RA2STAT` per
function plus the explain and placement dumps, base against mode 3):

- The function totals moved the right way (stores 6518 -> 7974, reloads
  16953 -> 14601), but the two hot DFA functions got more reloads
  (`cached_state` 4 -> 9, `next_state` 19 -> 29), all of them in the
  straight-line code around loops that stayed identical.
- Placement rates a loop body at ~32x its surroundings; regalloc2's depth
  scale says 4x. regex's loops walk a DFA state's short instruction
  list, so the code around them runs about as often as the loop, and
  32x made the allocator spill the state pointer out of the fast path
  (twelve reloads in `next_state`; base kept it in a callee-saved
  register). The held-out regex base itself spreads 1.11..1.22 across
  layouts, so the +1% there was never the signal; the per-function
  statistics are.
- The lesson: of the static model, the cold marks and unlikely hints
  are facts, the trip-count guess is not. A spill weight should use the
  first and not the second.

`PLIRON_RA2_FREQ=5` ("demote only") keeps stock's depth scale and
additive bonuses and lets the frequency only lower a block relative to
the hottest block at its depth (floored at 1/64). regex reloads 16953 ->
14867 at stores 6518 -> 6785, `cached_state` identical to base, 225
functions changed against 418. Tuning set (same run as mode 3): 0.9928
(regex 0.982, pulldown 0.992, blake3 0.993, json 0.996, blind 1.000),
mode 3 0.9943.

Held-out needed a better harness first. Two runs that afternoon were on
battery with macOS Low Power Mode on: the unchanged base moved 1.122 ->
1.107 between back-to-back runs (LLVM's own blake3 took 4.09 ms there and
2.89 ms on AC), and mode 3 flipped sign on four programs against the
morning. `bench/heldout/run.py` now saves every side's per-round
samples, prints a bootstrap 95% interval for each ratio (whole rounds
resampled so the sides stay paired; `*` = excludes 1) and for the
geomean, has `--aa` (the pliron binaries timed again as a variant, the
noise floor), and records and warns about the power state. On AC, 10
rounds x 3 layouts x 2 pads:

| | / base | interval |
|---|---|---|
| mode 5 | 0.995 | [0.990, 0.999] |
| mode 3 | 0.992 | [0.987, 0.998] |
| A/A | 1.000 | [0.992, 1.005] |

Per program the A/A interval is +-1..2% (pulldown's A/A even came out
"significant" at 0.990, so a single program's star is not evidence);
the geomean floor is +-0.5%. Head to head from the same samples, mode 3
/ mode 5 is 0.997 [0.994, 1.002], and only mode 3 beats the A/A side
outside the floor (0.993 [0.989, 0.999] against 0.996 [0.990, 1.002]).
The regex per-function reload counts that argued for mode 5 did not
predict the runtime (regex 0.988 under mode 3 relative to mode 5): the
reloads landed on a path that costs nothing measurable while mode 3's
larger cuts in the inlined matchers paid. **Mode 3 is now the default**
(`=0` off); the "facts only" mode 5 stays opt-in. Compile time: no
visible cost. The "best of both" candidates, measured on the same
harness against mode 3 as the base (10 rounds x 3 layouts x 2 pads):

| / mode 3 | ratio | interval |
|---|---|---|
| mode 3 + `LOOPSPLIT`/`LOOPDEPTH` | 0.998 | [0.994, 1.003] |
| mode 4 | 1.003 | [0.997, 1.007] |
| mode 5 | 1.004 | [0.999, 1.009] |
| A/A | 0.996 | [0.985, 1.000] |

Nothing beats mode 3 outside the floor (the split knobs, 0.986 on the
tuning set, are neutral held-out and stay opt-in). pliron/LLVM with the
new default: 1.101 [1.096, 1.106], from 1.113 with the depth-only
weights in the previous run.

Next steps if mode 5 holds: frequency-aware split placement (hoist a
split to the coldest block boundary between the bundle's start and the
conflict, LLVM's region split in spirit) uses frequencies where they
are reliable in relative terms; real frequencies (a sampling feed into
`block_hotness`) are the only answer for a cache hit rate.

## 2026-11-03 (evening): warm calls in the inliner's loop gate; frequency-aware split placement

**`EvaluateNode`.** LLVM's compression build inlines brotli's
`EvaluateNode` into `UpdateNodes` and keeps `UpdateNodes` and
`ComputeDistanceCache` out of line. Ours rejected it at cost 692 against
320: the callsite sits in a loop, but the loop bonus is gated on the
callee being leaf-ish (`PLIRON_INLINE_LOOPCALLS`, at most 4 calls of its
own), and `EvaluateNode` carries 18 calls, 13 of them bounds-check
panics and the rest `#[cold]` or already inlined. A panic call never runs
on the warm path and moves no call traffic into the loop, so the gate
now counts warm calls only (`PLIRON_INLINE_WARMCALLS=1`: not noreturn,
not `#[cold]`): `EvaluateNode` has one. Two things learned on the way:

- The gate at 4 warm calls is far looser than 4 total calls was. 59
  regex sites flipped, among them `follow_epsilons` (16% of regex's
  samples, 3 warm calls, 322 ops) into the per-byte `exec_byte` loop,
  which LLVM does not do: regex +4.8%. The old limit of 4 was calibrated
  against counts that panics inflate by two or three, so the warm gate
  wants 2: `follow_epsilons` out, `EvaluateNode` in, regex flat.
- Pricing the callee by its warm ops instead (`=2`, the ops outside
  panic blocks plus 20 per warm call; `EvaluateNode` 317 against 320,
  `follow_epsilons` 372) looked clean on the decisions I diffed and was
  not: it re-prices every panic-heavy callee, regex's binary grew 4% and
  ran 2.2% slower. Kept as a documented negative.

Tuning set (K=3 x 3 rounds): warm gate at 2 calls 0.996 (pulldown 0.983,
blind 0.993, regex 1.001), at 1 call 0.998; rust-compression 0.988 at 2,
0.993 at 1. Held-out (10 rounds x 3 layouts x 2 pads, A/A floor 0.998
[0.994, 1.004]): the gate at 2 warm calls / base 0.997 [0.994, 0.999],
pulldown 0.986 [0.978, 0.990], blind-sig 0.993 [0.985, 0.998], regex
1.000, compression 1.002 [0.996, 1.007] (the tuning-set 0.988 there did
not reproduce with intervals). Gate and 400 fuzz seeds clean with the
flags on. **Default now:** `PLIRON_INLINE_WARMCALLS=1` with
`PLIRON_INLINE_LOOPCALLS=2` (`=0` and `=4` restore the old gate).

**Frequency-aware split placement** (`PLIRON_RA2_FREQSPLIT=1`, regalloc2
patch log): the split point moves to the cheapest block boundary between
the bundle's start and the conflict, costing a boundary by its forward
edges at `min(pred, block)`. Tuning set 0.999, compression 1.010. The
depth hoist already lands on loop entries; a colder boundary further
back rarely exists and did not pay where it did. Held-out 0.999
[0.995, 1.003] of base. Opt-in.

## 2026-11-03 (night): compression at the new defaults; post-index chains serialize fills

**The profile, redone.** The earlier note that `BrotliEstimateBitCostsForLiterals`
ran 37% slower than LLVM's came from a 50-sample profile taken before the
zip-loop chain went on. At today's defaults (dev2 rebuilt at 41c583e; the
profiling scripts had been using a dylib that predated the warm-gate flip,
which still had `EvaluateNode` out of line) and 300 process runs per side:

| function (self samples) | pliron | LLVM |
|---|---|---|
| `BrotliCreateHqZopfliBackwardReferences` (with `EvaluateNode` inlined on both) | 893 | 589 |
| `FindMatchLengthWithLimit` | 564 | 581 |
| `BrotliEstimateBitCostsForLiterals` | 448 | 429 |
| `set_from_literal_costs` | 231 | 244 |
| `StoreAndFindMatchesH10` | 137 | 115 |
| ZopfliNode / Command fills (`extend_with` vs LLVM's alloc + `bzero`) | 132 + 78 | 126 + 65 |
| miniz_oxide `compress_inner` | 104 | 18 |
| `_platform_memmove` | 98 | 27 |

So the literal-cost estimator is at parity and off the list. The brotli
side is about 685 samples (19%) behind, and HqZopfli alone is 304 of it.
Per instruction, three things in HqZopfli, in order of size:

1. *`BrotliInitZopfliNodes`' fill* (196 samples vs LLVM's 86). The CLIF is
   right: four stores at `p+0, +4, +8, +12` and `p += 20`. The emit-time
   post-index sweep (patch 90's) then telescoped the bump into a writeback
   on every store, `str [b], #4; str [b], #4; str [b], #4; str [b], #8`,
   so each address waits on the previous store's writeback and the next
   iteration's first store on the last: a 4-cycle recurrence through the
   base where the `add` it deleted cost one. LLVM's loop-strength
   reduction puts the base 16 bytes into the record, two `stur` at
   negative offsets and one `str wzr, [x10], #20`. Built the same:
   `PLIRON_POSTIDX_ONEWB=1` gives the last offset-0 access the whole bump
   and re-addresses the ones after it below the bumped base (`stur`),
   refusing the rewrite when an offset does not encode. The loop is now
   `str wzr, [x1], #0x14; stur s1, [x1, #-0x10]; stur w0, [x1, #-0xc];
   stur d0, [x1, #-0x8]`; HqZopfli's post-index stores 18 -> 5, the
   binary's 2109 -> 1395. Test `pliron-clif-opt/tests/postidx_onewb.rs`.
   Gate 94/94 and fuzz 400/400 with the knob on; held-out (10 rounds, 3
   layouts, 2 pads, A/A floor 0.999 [0.993, 1.005]): compression 0.980
   [0.974, 0.991] of base, geomean 0.997 [0.992, 1.001], no program
   worse than its interval (worst pulldown 1.010 [0.998, 1.024]). Default.
2. *The Kahan sum in `set_from_literal_costs`' inlined copy* (261 samples
   vs about 210): `literal_costs[i+1] = literal_costs[i] + carry; carry -=
   literal_costs[i+1] - literal_costs[i]`. LLVM keeps `literal_costs[i]`
   in a register across iterations; we reload it twice per iteration
   through store-to-load forwarding (141 samples on one of them). The
   final CLIF has the store's address `v695 + v4934*4` and the latch's
   reload address `v695 + v4934*4` as two different values: looprot
   cloned the header loads into the latch, and by the time
   sameargs/paramsink made their operands equal (the last `lcse`/`dgvn`
   ran at step 127524 of the dump; the operands became identical after
   ivretro/latchspec), no value numbering ran again, so `loadfwd` saw two
   roots and forwarded nothing. The loop-carried half (this iteration's
   `cost[i]` is last iteration's `cost[i+1]`) is LLVM's
   LoopLoadElimination and needs the IV relation; the same-iteration half
   only needs a late CSE. Not built yet.
3. *miniz_oxide's `compress_inner` dictionary-insert loop* (104 vs 18
   self samples, 120 vs 27 inclusive). Not the pointer reloads: LLVM
   also reloads the three `Box` field pointers every byte (`ldp x16,
   x15, [x27, #0x20]`). It is where the fields sit: `d + 0x10018`,
   `+0x10020`, `+0x10028`, past the scaled 12-bit immediate, and the
   aarch64 lowering's `amode` folds any `iadd base, K` into the access
   offset and then materializes an unencodable offset with `mov`+`movk`
   at *every* access (224 `movk ..., lsl #16` in the function; LLVM
   hoists `x27 = d + 0x10000` once and addresses off it). The CLIF had
   the whole offset in the load (`load.i64 v1+0x0001_0018`), lcse's
   `memoff` having folded it there. Built `PLIRON_BIGOFF=1` (aarch64):
   `memoff` refuses a fold whose sum the encoder would not take
   (`amode_off_fits`: `ldur`'s -256..255 or `4095 * size` scaled), and
   splits such an access into `q = base + hi` (hi a multiple of 4096,
   shared per block by `(base, hi)`, hoisted and merged by licm/dgvn
   later) plus `[q + lo]`; the lowering's `amode` rules 1/2 stop folding
   an `iadd base, K` when the sum does not encode (`amode_fold_ok`), so
   the hoisted `q` stays a register. First cut (offset-in-access only)
   took compress_inner's `movk` from 224 to 88; the rest were the
   `iadd base, K` shape with `K` a licm-hoisted register (three of them,
   spilled in the loop), now split the same way (224 -> 4). Tests
   `pliron-clif-opt/tests/bigoff.rs`. Gate and fuzz pass with both knobs
   on. Held-out: `bigoff` alone compression 0.989 [0.981, 0.995], json
   0.984 [0.976, 0.996]; both knobs compression 0.962 [0.954, 0.972],
   json 0.978 [0.966, 0.990], geomean 0.993 [0.988, 1.003], no
   significant loss (regex 1.012 [0.997, 1.035] inside its interval; the
   run's A/A floor was wider than usual, protobuf aa 0.984*). Both are
   on by default now (`PLIRON_POSTIDX_ONEWB=0`, `PLIRON_BIGOFF=0`
   restore); held-out 1.098 -> 1.088 vs LLVM.

`_platform_memmove` (98 vs 27) is still unattributed; LLVM's inlined
copies or elided moves, callers not yet listed.

## 2026-11-03 (afternoon): one hot loop, end to end (brotli's byte compare)

rust-compression is the worst held-out ratio (1.32). Its profile against
LLVM's build is not one slow function but a 30% tax on the two hottest
(`FindMatchLengthWithLimit`, `BrotliEstimateBitCostsForLiterals`) plus
one missed inline (`EvaluateNode`, 6% of samples, inlined by LLVM). The
byte compare is a plain `s1[..n].iter().zip(s2[..n]).enumerate()` loop.
pliron emitted 11 instructions with two taken branches per byte against
LLVM's 7 with one, and each extra instruction had a different owner:

- a `cbz` per byte: the `Option<(&u8, &u8)>` niche test. The lowering
  knows both slice pointers are non-null and marks the `iadd base, i`
  inbounds, but `indvars` rebuilt that add as a stepped header param
  without the mark, the first `jumpthread` ran before edgespec and
  mergechain had reduced the test to `select 1, p, 0`, and the only
  null-fold (`PLIRON_NULLSRC`) looked through `jump` edges only.
  Fixes: `indvars` carries the derived mark onto the param and its edge
  values (always on; a tombstoned base is skipped); `fold_null_tests`
  sees through `brif` arms and constant select conditions
  (`single_edge_source`); a late fold (`PLIRON_NULLFOLD2=1`, start of
  `jtround`) rebuilds the lattice when the shape is decidable.
- three address adds and a `mov`: the zip's pointers are recomputed as
  `base + i'` from the stepped index, so `ivrefold` (which matches
  `p + k`) never refolded them. New alias rule: a slot whose back-edge
  value is `iadd B, q'` (`q'` another slot's edge value, or the slot's
  current param one step behind) with a matching entry is affine with
  `q` and refolds onto it (`bp = B`); loads become `[base, index]`
  amodes. The first `ivrefold` still runs before the late cleanup, so a
  second run (`PLIRON_IVREFOLD2=1`, after `constdedup2`) catches the
  loops the null fold just simplified.
- top test plus `jump` back: `looprot2` (existing opt-in, moved after
  `constdedup2`) rotates once the niche test is gone, but copied the
  header's increment into a second carried index (`i` and `i + 1`, a
  `mov` each iteration). `PLIRON_LOOPROT_SINK=1`: header instructions
  used only by the latch terminator (the increment and its constant)
  move to the latch first, so the clone tests the stepped index in
  place.

With all five knobs the reproducer (`t/zipcmp.rs`) is LLVM's loop
exactly: `ldrb [x2, x9]; ldrb [x4, x9]; cmp; b.ne; add x9, x9, #1; cmp
x9, #n; b.lo`, and so is brotli's. Filetests `ivrefold_alias.clif` and
`looprot_sink.clif`; regex corpus diffcheck clean under the chain (1579
ok, 338 inconclusive, 0 mismatches). Measured (evalcfg, 3 layouts x 3
rounds): **rust-compression 0.899 of base with all five knobs**, while
the null-fold half (0.992) and the rotation half (0.990) alone are
worth 1% each — the loop needs the whole chain. Tuning set: 1.012 at
first, all of it blind-sig (+7%): montgomery's inner loop, now
index-addressed, had a post-index store between the sum's `add` and its
carry `cmn`, and the flag-twin sweep (patch 91) stopped at it because
`nzcv_free` did not list the writeback forms. With that fixed the chain
is 1.002 on the tuning set (blind-sig 0.998, regex +1% inside its
spread) and compression stays at 0.899. Validation: gate clean with
the chain on and off, 600 fuzz seeds clean, held-out (8 programs, 5
rounds x 3 layouts x 2 pads) **1.117 -> 1.099** (compression 1.402 ->
1.281, json -1.8%, regex -1.2%, pulldown -1.2%, blind-sig +0.7%;
rotation alone 1.107). **All five flipped on** (`PLIRON_NULLSRC=0`,
`PLIRON_NULLFOLD2=0`, `PLIRON_IVREFOLD2=0`, `PLIRON_LOOPROT2=0`,
`PLIRON_LOOPROT_SINK=0` restore). The `jt_backedge_stale` shape test
now accepts a back edge with no arguments (the counters refold onto one
index).

Follow-ups seen in the same profile: `EvaluateNode` (6% of samples)
stays out of line — the inliner scores it 692 against a 320 threshold at
all three callers, and its 18 nested calls disqualify it from the
in-loop bonus; LLVM inlines it. `BrotliEstimateBitCostsForLiterals`
runs 37% slower than LLVM's and is not yet looked at. Lesson: every one of the four owners was a pass-ordering
or fact-propagation gap, not a missing optimization; the late pipeline
(`jtround` → `sameargs`/`niche`/`constdedup2` → `ivrefold2` →
`looprot2`) is where iterator loops become canonical, and anything that
wants their final shape has to run after it.

## Direction (2026-11-03, after the Kahan-loop trace): equality as an invariant, then the egraph

The HqZopfli trace above is the first concrete case of a structural
problem: `sameargs` proved two block params equal and rewrote their
users, and nothing drew the consequence that two users (the header's
and the latch's `v695 + i*4`) had become the same value, because value
numbering is a pass that had already run. Every pass that merges values
(`sameargs`, `paramsink`, `webmerge`, `ivretro`, jumpthread's param
folds) is such a site. The pipeline has about seventy steps, each
recomputing its analyses from the IR and dropping them; the only thing
that flows forward is what a pass writes into the IR, and "these two
values are equal" is only written when one is aliased to the other.

The order of work, each step measured on the held-out set and on
compile time (CMP stays at or under 0.85x LLVM):

1. **Equality as an invariant on plain CLIF** (`clifpeep::rekey`,
   `PLIRON_REKEY`). A merging pass hands the values it merged to a
   re-key worklist: each user of a merged value is re-hashed with
   `cse_key`, aliased to a dominating equal if one exists, and its own
   users are queued. One scan when a merge happened, nothing otherwise;
   transitive; no new pass and no ordering to get right. Done for the
   three param-merging passes first; the rest follow as their traces
   show misses.
2. **One placement policy for the egraph's elaborator.** The egraph's
   elaboration places each node as early as its operands allow (LICM by
   default); `splink`, `PLIRON_CONSTSINK` and `PLIRON_REMATSINK` undo
   that for one-instruction values inside loops with calls. Two policies
   fight regardless of order. Give elaboration the placement cost
   `splink` and regalloc2's spill weights already encode (loop depth,
   call clobbers, pressure), so the repairs retire and the egraph can
   run late or twice.
3. **Pure rewrites out of `clifpeep` and into ISLE egraph rules**
   (`vendor/cranelift-codegen/src/opts/pliron.isle`). `clifpeep.rs` is
   13,900 lines and 136 functions run as an ordered sweep several times;
   the egraph's rules are 2,800 lines applied to saturation once, where
   rules compose and an equality created late cannot be missed. Migrate
   in batches by rule family (addnorm/icmpk/bandk, extend-reduce chains,
   mem-offset folds, neg/shift folds, select folds), each batch verified
   by the clifpeep rule's hit counter dropping to zero on the corpus
   before the Rust rule is deleted, and each measured, since extraction's
   cost model will choose differently than the sweep did. Rules that
   depend on facts the egraph does not carry (ranges, non-null, region
   tags, loop shape) stay in Rust until the fact becomes a node
   attribute.
4. **What stays a pass.** Loop rotation, unrolling, jump threading,
   inlining, store forwarding, DSE, allocation: graph and memory
   transforms with no saturation form (LLVM runs about a hundred passes
   and reruns GVN/InstCombine for the same reason). Their rule is the
   existing one: write down what you proved so the next pass does not
   re-derive it.

**Measured so far (2026-11-03, night).** Step 1 is built: `clifpeep::rekey`
(`PLIRON_REKEY`, wired into sameargs, paramsink, webmerge). On HqZopfli
it removes the latch's duplicate address and the reload of the value
just stored; gate 94/94, fuzz 400/400 (a first run reported 305
"compile-errors" that were the memory watchdog killing rustc under an
overlapping dev2 build; the clean rerun passed). Held-out pending AC
power (the battery run's A/A floor moved 1%).

Step 3's first batch (`opts/pliron.isle`: addnorm's constant chains,
icmpk's constant-across-equality, `PLIRON_EGRAPH_RULES=1`) exposed the
real precondition: the mid-pipeline `egraph` step is
`Gate::OptIn("PLIRON_EGRAPH")` and at defaults runs on **zero**
functions; the only egraph that runs is Cranelift's compile-path one,
after every pliron pass. The step also crashed on first use
(`BlockCall::update_args` on a value list the egraph had freed, hit by
`resolve_all_aliases` in cparm; patch 103 makes the walk skip it), and
skips functions with `try_call`, which on rust-compression is 75 of 925.
The gate with the step on then failed in five stanzas: the wasm ones
(`wasm_smoke`, `lowerfree-wasm`, `pgo`'s wasm32 std) with "egraph broke
<fn>: Verifier errors" on wasm32 CLIF, and `rusttailcc` with the
post-egraph check finding a `return_call` operand whose def the
elaborator left out of the layout. Both are skipped now (`p.wasm`,
`ReturnCall*` alongside `TryCall*`); diagnosing them is part of step 0.
With the guards: gate 94/94 with `PLIRON_EGRAPH=1 PLIRON_EGRAPH_RULES=1
PLIRON_EGRAPH_MIGRATE=1 PLIRON_REKEY=1`, fuzz 400/400, gate 94/94 at
defaults. All four stay opt-in until the held-out run (armed to start
on AC power) gives intervals for rekey, egraph alone, and egraph with
the migrated rules.
With the step on, the counters (`rule:<name>` under PLIRON_STATS,
patch-style per-rule hit counters in clifpeep) say: the early sweep's
10,493 addnorm and 101 icmpk hits leave only 184 and 99 survivors for
the late sweep when the early rules are off (`PLIRON_EGRAPH_MIGRATE=1`),
and the ISLE rules then take 12 of the 184 and 87 of the 99. So the
early Rust rules are already nearly redundant with what runs after them,
icmpk migrates cleanly, and addnorm's survivors are shapes the egraph
does not see (created after it) or forms the rules do not match yet.
Firing counts are not the criterion, though: a rule that fires 10,000
times and changes nothing downstream is noise. The criterion is the
pipeline with the egraph step on by default being correct (gate, fuzz),
no slower to compile, and no slower at run time; that validation is
running. Until it passes, the migrated rules and the migration knob stay
opt-in, and the plan's step 3 is blocked on step "0": make the
mid-pipeline egraph a default pass.

Later, not now: **an offline autotuner over the numeric knobs**
(inline budget and loop bonus, placement weights `W_*`, spill-weight
scales, unroll factors; after step 3, the extraction cost model's
weights). The pieces exist: a tuning set separate from the held-out set,
`bench/heldout/run.py`'s bootstrap intervals and A/A floor, knobs as
environment variables. The loop proposes a knob vector, builds the
tuning set, accepts only moves whose interval excludes 1, validates on
held-out, and freezes the result into defaults. Offline only: a learner
inside the compiler would make builds non-reproducible and would be
training on this machine's noise; "no tuning on the check set" stays.
Learned cost models (as LLVM's MLGO did for inlining and eviction) need
a corpus larger than our benchmarks and are research, not a week.

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

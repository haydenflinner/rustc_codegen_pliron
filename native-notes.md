# Native aarch64 opt notes (wt/native-opt)

## Landed this session (rev_copy32 follow-up + reduction epilogue)

Prior uncommitted WIP (~44 lines in loopvec.rs) was inst-major hoisted
load emission; kept and extended (two-pass lane reversal, see below).

- `loopvec apply()`: non-direct stream bases are now strength-reduced
  into `vh` block params (initialized in the check block `cb` from
  `subst(base, iv, iv0+adj)`, stepped ±vf*UNROLL elements on the
  backedge). Kills the per-iteration scaled-index `mul`+`iadd` tree and
  lets all group offsets ride load/store immediates. Descending (`neg`)
  streams anchor at the UNROLL block's lowest byte so offsets are
  non-negative (`UnsignedOffset` amodes — required for pair fusion).
- Hoisted loads emit inst-major AND lane-reversals in a second pass, so
  the `ldr` run and the `rev`/`ext` run each stay contiguous.
- Reduction epilogue: group accs combine lane-wise, then a log2(lanes)
  rotate-shuffle+op tree replaces extractlane-per-element
  (extractlane×lanes×UNROLL → ~3+2log2 ops).
- vendored aarch64 `fuse_with_next` now also accepts `AMode::Unscaled`
  (`ldur`/`stur`) inputs, not just `UnsignedOffset`.

Codegen for rev_copy32's vector loop went from ~25 insts to:
`ldp [x4,#32]; ldp [x4],#-64; rev/ext x4; stp [x5],#32; rev/ext x4;
stp [x5],#32` + loop control — the same 2x ldp + 2x stp(post-index)
shape as LLVM (its 15 insts vs our ~19, remainder is loop-control movs
and a remat'd trip bound). `scaled` similarly got 2x post-indexed
ldp + 2x post-indexed stp + 4x fmul.

Note: the store pairing succeeded *despite* producers sitting in the
ALU gap — ok_down fires when regalloc gives the pair's data operands
and the gap producers disjoint phys regs (luck of allocation). It's not
guaranteed; kernels where the producer regs collide will still miss
stp. Not structural this round.

## Measurements (loaded box, load avg ~7; ratios over absolute)

`wide` suite pliron vs stock rustc (-Ctarget-cpu=apple-m1):
- rev_copy32: 0.316 vs 0.317 — PARITY (was a stable ~11% deficit)
- scaled: 0.639 vs 0.640 — parity (previously ~-8% wobble)
- dot_u8: 0.147 vs 0.291 — 2x win
- has_val 0.329 vs 0.744, cnt_vowel 0.310 vs 1.163, find_off 0.082 vs
  1.164, fillzero 0.156 vs 0.189 — wins
- max_u/min_i/sum_sq/even_sum/xor_fold/sum2d — parity
No regressions observed.

Validated: ./test.sh green; targeted correctness tests for
rev/neg-store/neg-load/dot/early-exit kernels across edge lengths
(0..1000 incl. non-multiples of VF*UNROLL).

## Follow-up (post-merge fixes)

- `scaled` regression (0.65→0.72): NOT loopvec's dispatch — the x64
  punroll pass partial-unrolls the now-small vector body (strength-
  reduction pushed it under MAX_BODY=24), adding a per-group guard
  dispatch `(iv+8<bound) && (bound-iv-8>16)` plus an immediate-offset
  4x clone. Fix: `punroll::run_loop` bails on loops containing
  vector-typed values (block params, inst args/results) — SIMD loops
  are already vf*UNROLL-wide, cloning just pays a fresh guard.
  Back to single post-index ldp/stp loop; 0.61 vs 0.61 stock.
- Trip-bound `and` remat: egraph remats `band x, iconst` (remat.isle)
  into every use block, so `nm` was re-executed per backedge. loopvec
  now emits `nmv = isub(base, band(base, K-1))` — identical value,
  not remat-eligible (no resimplification rule in opts). Latch is
  `add; cmp; b.lo` — matches LLVM's 3-inst control.
- `.iter().zip().map().sum()` stayed scalar: rustc lowers
  `CheckedBinaryOp` to two-result `sadd_overflow`/`smul_overflow`;
  MIR drops the `Assert`, leaving DEAD flags that defeat loopvec's
  op matching. New clifpeep `deflag` (PLIRON_DEFLAG, default on)
  rewrites dead-flag `*overflow` ops to the plain wrapping op —
  zip_sum now vectorizes to the same ldp+mla loop as the indexing
  form (0.327 vs stock 0.323, was ~scalar). MIR shape aside, nothing
  MIR-level is needed — the iterator loop is a normal pre-tested
  affine-counted loop once flags are dead.
- rev_copy32 residual: ~4% vs LLVM comes from backedge mov ping-pong
  (`mov x0,x8; add x0,#16; mov x8,x0` = 3 insts for iv+=16, regalloc
  arg-copy artifact) — LLVM counts down (`sub; cbnz`, 2 insts). A
  count-down iv would need direct-stream addressing rework; left.
- Store pairing across producer gaps: assessed — vcode's fuse pass
  already crosses ≤8 `pair_fusion_crossable` insts with up/down
  placements + preg hazard checks. NOT guaranteed (window bound,
  non-crossable producers, hazard failures); guaranteeing it needs
  source-order emission (done: inst-major hoist) or a real scheduler.
- General-reg AMode::PostIndex for single ldr/str: not pursued —
  marginal (only scalar epilogue paths benefit); the paired streams
  already telescope to post-index writebacks.

## This round (scatter/hist, countdown iv, cold-edge RA effects)

- `bcheck`: skip loop versioning when fewer than 2 checks fold and
  other cold checks remain. Versioning duplicates the loop for ~2
  insts of per-iter gain, and in scatter/RMW loops the tighter clone
  is actually slower — `cnt[a[i]]` is a same-address store->load
  chain, and shrinking the loop widened the memory-order replay
  exposure (measured: versioned 9-inst loop 1.12ms vs unversioned
  15-inst loop 0.61ms, all-3s data). `hist`: 1.123 -> 0.612.
  `gather` keeps its win (2 of 3 checks fold): 0.478 vs stock 0.599.
- `clifpeep::coldargs` (PLIRON_COLDARG, default on): integer args of
  calls in cold blocks defined in hot code get rebound through
  `iadd x, 0` inside the cold block, so the ABI arg-register pinning
  materializes at the call site (LLVM's shape) instead of
  constraining the value's whole hot live range. Keeps CLIF honest;
  measured effect is small — see residual below.
- `clifpeep::fusechains` (PLIRON_FUSE, default on): splices a
  `jump`'s target into its predecessor when the target has exactly
  one predecessor — removes per-edge regalloc copy shuffles on
  straight-line chains (bcheck clones, split bodies). Conditional
  edges cannot merge (one terminator per block).
- `loopvec` count-down induction: when no body inst reads the scalar
  iv (all stream bases strength-reduced, no early exits, step > 0)
  the vector loop carries `rem = end - iv` and guards `rem != 0`.
  rev_copy32's latch is now `sub x6,#0x10; cbnz x6` — LLVM's exact
  shape, and both stores emit post-index `stp`s. Timing parity
  (0.316 vs 0.316; bandwidth-bound) with strictly better code; the
  earlier mov ping-pong was already gone after stream strength
  reduction.

## Bench sweep this round (same loaded box; hist numbers repeat
consistently within a binary but identical code has measured
0.61-1.16 across runs — treat <2x deltas as noise-prone)

- scatter: gather 0.478 vs 0.599 (win), hist 0.612 vs 0.438
  (residual loss, was 1.123)
- mixed: memchr 0.080 vs 1.178, itersum 0.442 vs 0.732, strsum 0.109
  vs 0.236, fnv/copyrev/chaindep parity
- main: matmul 10.53 vs 12.07 (1.15x), sum_u8 0.074 vs 0.219 (3x),
  axpy/vadd/clamp/dot parity
- wide: rev_copy32 0.316 parity (cbnz latch now), scaled parity,
  dot_u8/cnt_vowel/find_off/has_val wins
- early2/earch/es/cv/dtest2/dtest3/condred/splitacc: correctness
  identical both backends

## This round (RMW closed-form loop deletion + post-merge reassessment)

`loopidiom::plan_dead` previously rejected any loop containing a load
or store. That miss showed up as a ~50x loss on same-address RMW
loops (`for _ in 0..n { *p += k }`): licm had already promoted the
location to a loop-carried accumulator and punroll×8'd the body, but
LLVM deletes the loop entirely (`init + k*n`, one store). Now closed
too:

- `DeadPlan` gained `escapes`, `iters`, `rmw`. Escaped loop values
  (e.g. the licm-promoted accumulator read by an exit-block `store`)
  become appended `exit_dest` params: existing in-edges bind the
  original value, the fast edge binds the closed form (`arg_ins` over
  entry args + `iters-1` for post-tested counts). Previously any
  direct in-dest use of a loop value aborted the transform.
- `rmw_locs` classifies in-loop plain `load`/`store` access as
  per-location same-address RMW chains: invariant address roots,
  one store per location, non-overlapping ranges, store dominates
  every latch, loads dominate the store, stored value is `ld`,
  `ld ± inv`, or an invariant. Fast path emits load →
  `init ± delta*iters` (or invariant set) → store per loc. Only
  post-tested counts fold (provable ≥1 iteration; zero-trip loops
  keep the store on the loop path).
- Exit edges proved dead by a loop-INVARIANT `brif` (rustc's hoisted
  bounds checks, e.g. `0 < len` in `a[0] += k`) now add a one-shot
  pre-loop pred instead of blocking deletion — a one-branch slice
  of unswitching, sound because `outv` only accepts `Param::Inv` /
  loop-external values.
- `trips_idx` learned the do-while `iv+step < n` shape
  (`ceil((n-iv0)/step)`, guarded `iv0 < n`), and the count prover
  now requires the tested `iv+step` value to be the exact latch
  arg — a different `iv+k` would miscount.
- Constant preds are folded at plan time; a statically-false one
  skips the transform rather than emitting a dead fast path.

`rmw.rs` (new microbench): rmw_add 1.18→0.023us, rmw_idx
5.15→0.023, rmw_two 5.72→0.024 — all at stock parity (~0.023).
`rmw_dep` (changing-address chain `a[i] += a[i-1]>>2`, true
dependence) stays a loop at 1.19us vs stock 1.72 — we win there
because LLVM can't promote it either.

### matmul assessment (asked: can loopvec do register tiling?)

No, not within the current model. `loopvec`'s `Reduc` abstraction
lifts ONE scalar loop-carried reduction into a lane-wise vector
accumulator; float `fadd`/`fmul` reductions are deliberately kept
scalar (rounding-order changes, no `reassoc` flag). A 2×4/4×4
register tile needs loop unroll-and-JAM, several independent scalar
acc chains, and float reassociation policy — none expressible. That
said, the benchmarked `matmul_f32_256` stock codegen is itself a
scalar serial `fmul`/`fadd` dependency chain (~54-line fn), not a
tiled kernel — the gap it leaves is FP latency, not vectorization.
Pliron already leads 10.69ms vs 12.22 (1.14x). Further gain is a
punroll-side multi-acc / unroll-and-jam item — x64-owned, noted not
attempted.

### scatter::hist post-merge (re-measured only, x64 agent owns RA)

Merged punroll side-exit work (bc1faa1) emits the ×2 histogram body.
hist remains noisy/bimodal: 0.52 typical vs stock 0.44 (~1.19x);
identical code has measured 0.38–0.73 across runs — treat as
noise-prone. gather: 0.38 vs stock 0.60 — win. Residual copies are
the edge-copy/regalloc artifact already scoped to the x64 agent.

### Sweep re-check (same loaded box; ratios only)

wide.rs re-run after the RMW work: no regressions; scaled 0.585 vs
stock 0.65, rev_copy32 0.272 vs 0.317 (the countdown-iv shape holds).

### Validated this round

cargo build clean, ./test.sh green, PLIRON_VERIFY=1 on the RMW
kernels, rmw_check.rs (0/1/3/1000 trips, add/sub/set/keep deltas,
two locations, iv+acc escapes, variable-delta non-fold, conditional
store non-fold, changing-address chain non-fold, bounds-check traps
still firing on the slow path) — pliron and stock byte-identical
behavior. Perf numbers above are from a loaded box — label
"not perf validated" for <1.5x deltas.

## Remaining opportunities (ranked)

1. hist residual (~1.19x post-merge): per-iteration regalloc edge
   copies in the unversioned checked loop. RA-level fix (regalloc2
   operand/edge coalescing), scoped to the x64 agent's
   investigation — re-measure after their next merge.
2. matmul deeper win: unroll-and-jam with multiple independent f32
   acc chains needs punroll-side support (multi-exit inner loops)
   plus a float reassoc policy — outside loopvec's model, x64-owned.
3. Dependent-address RMW chains (`a[i] += a[i-1]>>2` style): can't
   close-form; gains would need dependence-aware spacing of the
   str->ldr replay (scheduler knob, unexplored). We already win
   this vs LLVM 1.19 vs 1.72.
4. Store pairing across producer gaps: assessed — vcode's fuse pass
   already crosses ≤8 `pair_fusion_crossable` insts with up/down
   placements + preg hazard checks. NOT guaranteed (window bound,
   non-crossable producers, hazard failures); guaranteeing it needs
   a real scheduler.

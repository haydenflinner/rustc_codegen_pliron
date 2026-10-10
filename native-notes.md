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

## Remaining opportunities (ranked)

1. hist residual (~1.4x): the unversioned checked loop still emits
   ~6 regalloc copies per iteration (`x0=x5; x4=x15; x13=x3; ...`)
   vs LLVM's zero. CLIF is minimal (one carried param; cold call
   args rebound); the copies are per-block-boundary RA artifacts —
   LLVM's 3-MBB loop has the same structure but a global live-range
   assignment. Fix is RA-level (regalloc2 operand/edge coalescing),
   not a CLIF pass: bigger blast radius, not attempted.
2. matmul further win (1.15x now): deeper k-unroll / f32 fmla
   pairing / register tiling. loopvec's model can't express tiling;
   punroll (x64-owned) is the nearer lever — multi-exit inner loops.
3. Same-address RMW loops: tight 7-9 inst loops LOSE to 14-15 inst
   versions on Apple silicon (store->load replays). bcheck now
   avoids creating the tight clone for hist-like shapes, but
   LLVM-checked hist still beats our unversioned loop — spacing the
   dependent str->ldr gap (a scheduler knob) is unexplored.
4. hist_unchecked path takes scalar punroll and loses (1.48 vs
   0.61); punroll multi-exit handling is the x64 agent's domain —
   documented only.

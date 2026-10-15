# Coordination ledger (overnight run)

## Merged to devin/1791392401-pliron-cranelift-backend
- e0e2416: keep cranelift host-arch on shared branch (x64 wt keeps all-arch)
- 97e3f0b/b41ba7f/730dcdf (wt/x64-codegen): -Ctarget-cpu→Cranelift ISA flags,
  Mach-O variadic trampolines, Intel-syntax asm stubs on macho. Verified:
  host std diff-clean + unwind; agent verified x64-apple-darwin under Rosetta.
- e89a849 merge: 4f21915 punroll pass (scalar partial-unroll, exit-cone clone,
  PLIRON_PUNROLL=0 off) + 7dd0f49 x64 lowering audit notes. Verified on shared
  checkout: cargo build, std diff-clean, unwind pass, cpubench+wide re-run —
  no regressions; sum2d now parity; clamp earlier-9% deficit was load noise.

## Perf-validated on shared HEAD (M3 Max, quiet box)
- micro: parity across axpy/vadd/clamp/dot_i32/dot_i8; matmul ~1.15x win;
  sum_u8 earlier "2.9x win" was NOISE (stock 0.077, not 0.216).
- wide: find_off 14.3x, cnt_vowel 3.7x, has_val 2.2x, dot_u8 2x; sum2d parity;
  rev_copy32 0.263 vs 0.237 (11% wobble — re-check, possible noise).

## Not perf-validated
- punroll on aarch64 beyond benchmark kernels (agent tested x64+host test.sh).
- x64 min/max/dot_i32 "gaps" largely explained by missing ISA flags
  (baseline SSE2); psadbw for sum_u8 blocked on vendoring
  cranelift-assembler-x64 — documented in x64-notes.md.
- ffe20b3 loopvec operand-emitability guards: conservative by design —
  some count-if conds that previously vectorized may now stay scalar
  (any `band x,1`/bool-tree operand that is neither lane-replicable nor
  splattable). Correctness-required (was an ICE / silent-miscompile
  path), but the perf delta was not measured.
- 75cb4c6 bittab reanalyze-per-fold: CFG/dom/ublocks are rebuilt after
  each applied fold — correct-by-construction but measurably more
  compile-time than the old single-snapshot plans; not measured.
- b134d48 fusechains/merge_chains self-alias guards: bail rate on
  param-permutation/dead-cycle chains unmeasured; could theoretically
  forgo some profitable fusions.
- 655947a wasm frame-fold merge: perf validated on the wt/wasm-perf
  worktree only (wbig ~1.3%, SP ops -17%); not re-benchmarked after
  merging onto the current optimizer stack.

## Follow-ups
- rev_copy32 aarch64: stable ~11% deficit vs stock (0.26 vs 0.234, 3 runs).
  NOT punroll (same with PLIRON_PUNROLL=0). Assign to native agent: check
  store-pair/lane-reversal interaction in loopvec output.

## rev_copy32 root cause (for native agent)
Vector loop emits per-load `mov #-N; add base; ldr q` (3 instrs/load) + a
`mul` for scaled index; 4 adjacent str never fuse into stp across rev64/ext
ALU gap; no decrementing-pointer addressing. LLVM: 2x ldp + 2x stp(post-idx),
~15 instrs/64B vs our ~25. Fixes needed: (a) induction strength-reduction of
scaled index in vector loop, (b) ldp/stp fusion crossing pure-ALU gaps in
both directions (f0efcec/cfe150f may only cover same-direction or forward
order), (c) post-index addressing for the store stream.

## Merged: psadbw (x64) — commit 23446fa
Vendors cranelift-assembler-x64{,-meta} under [patch.crates-io] (same
pattern as regalloc2/rsasm). New x86_psadbw CLIF op + x64 ISLE rules;
loopvec emits it for u8->u64 unmasked sums on x64 only (aarch64 keeps
uaddlp tree). sum_u8 x64: 0.384->0.135 vs stock 0.693 = ~5x vs LLVM
(was 1.8x). Verified: agent ran PLIRON_VERIFY + edge sizes + host test.sh;
coordinator re-verified std diff-clean + unwind on merged tree.
Host builds unaffected (host-arch retained; assembler crates only compile
for x64 feature anyway).

## Fresh x64 table (Rosetta, v3, all x64 work landed): ALL WIN/PARITY
kernel: pliron/stock (ms) — has_val 0.355/1.519 (4.3x), max_u 0.211/0.231,
min_i 0.212/0.235, sum_sq 0.889/1.067, dot_u8 0.464/1.443 (3.1x),
fillzero 0.666/0.679, scaled 0.727/0.938, rev_copy32 0.370/0.643 (1.7x),
cnt_vowel 0.671/1.579 (2.4x), even_sum 0.621/1.833 (3x), xor_fold
0.424/0.465, find_off 0.085/1.208 (14x), sum2d 0.793/0.745 (-6%),
axpy 0.367/0.588 (1.6x), vadd 0.520/0.641, clamp 0.090/0.142 (1.6x),
dot_i32 0.089/0.132 (1.5x — was 2.3x SLOWER pre-ISA-flags),
sum_u8 0.141/0.694 (4.9x, psadbw), dot_i8 0.312/0.446 (1.4x),
matmul 11.82/12.42 (~parity).
KEY: earlier x64 regressions were baseline-SSE2 codegen; the
-Ctarget-cpu ISA mapping (97e3f0b) fixed them, not lowering changes.

## Merged: native loopvec (d6fe695 -> 4ede697)
Strength-reduced stream bases (block-param pointers, ±vf*UNROLL steps),
inst-major hoisted loads, ldp/stp fusion for Unscaled amodes, shuffle-tree
reduction epilogue, vendor aarch64 emit.rs fuse_with_next Unscaled.
Verified on merged tree: rev_copy32 now WINS (0.255 vs 0.315, was -11%);
max_u/min_i parity; dot_u8 2x; cnt_vowel 3.7x; find_off 14x; sum2d parity.
POSSIBLE REGRESSION: scaled (f64) 0.65->0.72 (~10%, stable across runs;
new dual-loop runtime dispatch + remat'd trip bound). Follow up with
native agent — either gate the split or drop the remat.

## Native follow-up: scaled regression FIXED + zip() vectorization
wt/native-opt (ff to 9991f2b + fixes). Root cause: NOT loopvec — x64
punroll unrolls the now-sub-MAX_BODY vector body, adding per-group
guard dispatch + a 4x immediate-offset clone (wide_p3 disasm). Fixes:
- punroll: bail on vector-typed loops (params/args/results is_vector)
  — cloning SIMD loops just pays a fresh guard per K*vf*UNROLL elems.
- loopvec: trip bound `nmv = isub(base, base & (K-1))` — numerically
  identical to `band base, -K` but NOT egraph-remat-eligible; latch
  is now `add;cmp;b.lo` (LLVM-parity control, no remat'd `and`).
- clifpeep deflag (PLIRON_DEFLAG, on): rewrite dead-flag
  sadd/ssub/smul/uadd/usub/umul_overflow -> plain wrapping op. rustc
  keeps CheckedBinaryOp in iterator chains (e.g. .iter().zip().map()
  .sum()) with the Assert MIR-opt'd away — dead flags defeated
  loopvec matching. zip_sum now vectorizes identically to the
  indexing form.
Native box numbers (quieter; pliron vs stock): scaled 0.60-0.61 vs
0.59-0.62 PARITY (regression gone); rev_copy32 0.25 vs 0.24-0.28;
sum_sq 0.221 vs 0.222; zip_sum 0.327 vs 0.323 (was scalar+punrolled);
dot_u8 0.148 vs 0.295. ./test.sh green.
Residual: rev_copy32 latch still ~3 insts of regalloc mov ping-pong
vs LLVM's sub+cbnz count-down; guaranteed store-pairing across
producer gaps needs source order (done) or a scheduler — assessed
only; general-reg AMode::PostIndex not pursued (marginal).

## Merged: native fixes (82ba7c8, ff to shared HEAD)
- scaled regression ROOT CAUSE: punroll was partial-unrolling the new
  smaller SIMD loops. punroll now bails on vector-typed loops (21-line
  eligibility check in src/punroll.rs — x64 agent's file, coordinated).
  Verified merged: scaled 0.561 (parity), rev_copy32 0.241 win retained.
- Trip-bound remat killed via isub(band) trick — latch now LLVM-parity.
- NEW: `deflag` pass in clifpeep.rs (PLIRON_DEFLAG, default on) rewrites
  dead-flag CheckedBinaryOp results to wrapping ops — `.iter().zip()`
  chains now VECTORIZE (big real-world win; dead overflow flags had
  defeated loopvec op matching). zip_sum at parity vs stock.
- Verified: cargo build, std diff-clean, unwind, wide suite.

## Merged: x64 load-sink relaxation + punroll single-block (wt/x64-codegen)
Two cooperating changes close the last wide.rs gap (sum2d was -6%):

- **punroll single fused block** (src/punroll.rs): the K unrolled copies
  now go into ONE straight-line block (`uf`) instead of one block per
  copy; block params thread through a `carry` map keyed by source param.
  Rationale: Cranelift's `optimize()` egraph parks pure `fadd` combines
  in the last block when copies are chained by jumps, separating each
  `load` from its consumer by a block boundary — colors are never
  adjacent, so no load-sinking form can ever apply. Same-block copies
  keep `load; fadd` adjacent through elaboration.
- **direct-use load sinking** (vendor cranelift machinst/lower.rs):
  `get_value_as_source_or_const`'s side-effect path additionally accepts
  `value_direct_uses[val]==1` *when `cur_inst` itself is that use and
  `value_lowered_uses[val]==0`, alongside upstream `Once`. The `Once`
  state is transitively coarsened to `Multiple` whenever a consumer
  fans out (e.g. an `fadd` accumulator feeding backedge AND exit args),
  so upstream could never fuse `addss (mem), %xmm` in a reduction loop.
  Two safety conditions learned the hard way (both previously ICE'd):
  * `cur_inst` must be a direct user — rejects `(store (iadd (load) k)
    addr)`-style probes where a multi-used pure node still needs the
    value;
  * `value_lowered_uses==0` — an earlier-lowered inst may already have
    materialized the value in a register after its own probe rejected
    sinking; fusing then would leave that register undefined.
- Kept as opt-in diagnostics: `PLIRON_VCODE=<substr>` prints Cranelift
  VCode for matching functions (lower.rs set_disasm), and
  `PLIRON_COMPILE_EGRAPH=0` ablates the whole egraph pass (lib.rs).
- Verification: host ./test.sh green; x86_64-apple-darwin
  nostd/std/unwind/asm/unroll/licm all pass under Rosetta.
- Perf (Rosetta, v3, pliron/stock ms): sum2d 0.74/0.74 PARITY (was
  0.79/0.75); matmul 11.3/12.2 (+7%); scatter gather 0.56/0.50 (-12%),
  hist 0.47/0.44 (-6%) — only remaining x64 gap; LLVM 4x-unrolls the
  multi-exit gather loop, punroll bails on its exit cone.
  Everything else ≥ parity: axpy 0.37/0.59, memchr 0.08/1.21,
  itersum 1.18/2.89, strsum 0.23/0.90.

## Merged: x64 round 3 (e6876bf -> e6976bd)
- punroll: unrolled copies fuse into ONE block with carry map — enables
  load+fadd adjacency sinking (was color-boundary blocked).
- vendored machinst/lower.rs: value_direct_uses + guarded UniqueUse sink
  (direct_uses==1 && cur_inst direct user && lowered_uses==0). ISA-GENERIC.
- x64 results: sum2d parity, matmul ~7% win; scatter gather -12%/hist -6%
  remains (multi-exit punroll budget — documented).
- aarch64 re-verify on merged tree: std diff-clean, unwind pass,
  rev_copy32 keeps post-index ldp/stp shape (0.32-vs-0.315 was box noise,
  stock moved identically), all wide kernels parity or wins.
- Remaining x64 gap: scatter (multi-exit unroll), noted.

## x64: punroll mid-chain side exits (wt/x64-codegen)
gather/hist were the last x64 deficit: rotated bounds-check loops carry a
mid-chain `brif` to a cold panic block, and the old chain walk treated any
`brif` as the latch — "extra body blocks" bail, loop stayed scalar.

- **Chain walk**: a mid-chain `brif` with exactly one fresh in-body dest
  is a *side exit*; the chain continues through it. Latch is the `brif`
  with a dest back to `h`. Still bails on diamonds, double exits,
  re-entry, cold chain blocks, try_call/br_table cones.
- **Emission**: a side exit splits the copy into segments (`usegs[kk][j]`)
  — the fused single block is kept only when there are no side exits
  (sum2d's `addss (mem)` form unaffected). Copies link by `jump`; each
  copy keeps its own data-dependent side tests, exactly like LLVM.
- **Exit cones**: a side exit whose dominance cone references loop values
  gets ONE shared clone per unique target — the clone root takes the used
  loop values as appended block params, so every copy's `brif` carries its
  own bindings on the edge (LLVM's one-cold-panic-block shape). Cone-free
  exits just retarget the original block. Budgets: <=8 exits, <=8
  blocks/48 insts per cone, <=96 cloned insts total.
- Verified: scatter gather 0.485 vs stock 0.491 (parity; was 0.563/-12%),
  hist ~0.467 vs 0.436 (-7%, was -6%; hist times are bimodal ~0.46/~0.82
  on BOTH binaries — Rosetta system noise, use the low mode). Deterministic
  n=0..4096 hash test identical to stock; bc_sem panics identically through
  catch_unwind; PLIRON_VERIFY clean on scatter/main/wide; host test.sh
  green; x64 nostd/std/unwind/asm/unroll/licm pass under Rosetta.

## Merged: native a247049 + x64 4371c82 (combined -> bc1faa1)
Native: count-down vector IV (rev_copy32 latch now sub+cbnz LLVM-shape),
bcheck skips <2-fold versioning (hist 1.12->0.61), clifpeep coldargs +
fusechains passes (PLIRON_COLDARG/PLIRON_FUSE, default on, bisect-gated).
x64: punroll mid-chain side exits — segmented emission + shared cold-block
clones; scatter gather -12% -> parity (0.485 vs 0.491).
Verified on combined merge: cargo build clean, FULL test.sh green
(nostd/std/unwind/asm/proc-macro/wasm/wasi/EH all pass).
Remaining: scatter hist ~7% x64 / ~1.4x aarch64 (regalloc edge-coalescing
artifact, bigger blast radius — ranked #1 residual in native-notes.md);
matmul deeper tiling; RMW tight-loop store-forward replay; punroll
multi-exit for hist_unchecked.

## Merged: native 03cd4db + x64 notes (fd6ea2d, 728c534)
- loopidiom: same-address RMW loops closed to load;init±delta*iters;store.
  Escape params + invariant-guard predication + trips_idx. Verified merged:
  rmw_add/idx/two 0.024us = stock parity (closed-form both sides),
  rmw_dep 1.19 vs 1.73 WIN, rmw_check output bit-identical incl. panics.
- x64 hist study: 4 cold-transfer mechanisms measured, all neutral-or-worse;
  committed shape locally optimal. Next direction: cold-cone remat.
  PLIRON_PREVCODE=<pat> + PLIRON_RA2_EDITS=1 debug dumps now in vendored
  compile.rs (env-gated).
- matmul register tiling: assessed NOT loopvec-expressible (needs
  unroll-and-jam + multi-acc chains + f-reassoc). Documented handoff.
- Not perf-validated: some sub-1.5x deltas on loaded box (native-notes.md).

## Native: pair fusion virtual/stack amodes (wt/native-opt, pending)
- vendored aarch64 emit: `fuse_with_next` gains `&Self::State`
  (trait default unchanged; aarch64 is the only implementer — no x64
  file touched). `uoff` now resolves `RegOffset`/`SPOffset`/`FPOffset`/
  `IncomingArg`/`SlotOffset` exactly as `mem_finalize` does at emit
  (frame layout is final there), so stack-slot and far-offset accesses
  can pair-fuse. `UnsignedOffset`/`Unscaled` path unchanged.
- regex-syntax rlib: 102,875 insts (was 103,041); vs no-fuse build of
  the same code `str [sp]` 2894 vs 3010, `stp [sp]` 1174 vs 1066.
  Overall fusion effect in-crate: str 6190 vs 10986, ldr 12315 vs
  16123. Gap remains: mov 20.5k / b 9.7k / udf 4k — layout+edge-copy
  problem, not addressing.
- Verified: cargo build, FULL ./test.sh green, harness tier 0 clean.
- Shared-file note: vendor/cranelift-codegen/src/machinst/mod.rs +
  vcode.rs touched (trait sig + call site) — flagging for x64 agent
  since machinst is common ground.

## Merged: native dse 1435a5e + x64 jam aaefc3b (-> f4c30bd)
- src/dse.rs new: backward may-read fixpoint, kills stores overwritten
  before any read (top_non_iso vs top_non_stack demand split; atomics/
  volatile/calls structurally excluded). PLIRON_DSE=0, bisect-gated.
- LATENT BUG FIXED: adjacent volatile stores were egraph-DSE'd (CLIF
  MemFlags has no volatile bit); now fence-bracketed via intrinsic.rs.
- constraint-elimination investigated -> ~0 residual (jumpthread already
  folds dominated conds; bcheck fallbacks are deliberate). Skipped.
- punroll unroll-and-jam (PLIRON_JAM, DEFAULT ON — agent mislabeled it
  "opt-in"; verified enabled): matmul_f32_256 4.17 vs 12.21 stock =
  2.93x on merged aarch64 build. x64 ~2.5x. Identical outputs n=0..256.

## clifpeep: coldedges adapters + coldargs sadd_overflow wrap (wt/native-opt)
- mov taxonomy via new PLIRON_RA2_EDITS/PLIRON_RA2_VERBOSE dumps
  (machinst/compile.rs; regalloc2 Ctx::debug_annotations now `pub`):
  (a) fixed-ABI uses on cold-block call args reach back through hot
  live ranges -> ion minimal-bundle splits pay a mov per hot use +
  backedge (hist ~6/iter, both arches); (b) shared param'd cold
  blocks (punroll `block21(v46) cold`) force incoming parallel copies
  into hot pred tails; (c) residual high-arity block-arg edges +
  layout — no safe minimal RA2 tweak, merge_vreg_bundles already
  covers blockparams/reuse.
- src/clifpeep.rs::coldargs: hot int args of cold-block calls rebound
  via `sadd_overflow(x,0)` (survives egraph `iadd_x_plus_zero`; flag
  result DCE'd -> one `adds` in cold code); extended to
  ValueDef::Param (bundles merge with hot edge sources).
- src/clifpeep.rs::coldedges (new, PLIRON_COLDEDGE=0, bisect'd):
  per-edge cold adapter `a: jump C(args)` for hot->cold param edges;
  edge-indexed so two-edge terminators/br_table are correct; skips
  edges carrying TryCallRet/TryCallExn args.
- Numbers: hist loop 17i+6mov -> 11i+0mov; gather 0.607->0.342
  (beats stock 0.44); hist 0.66->0.53 (stock 0.436; residual =
  per-element bounds check not merged into exit test).
  regex-syntax rlib: mov 20519->19570, uncond b 9676->8753; total
  +2.7k cold adds/cset (~2740 rewrites/217 fns).
- Verified: cargo build, FULL ./test.sh green, PLIRON_VERIFY clean,
  scatter/hist/gather correct + timed. punroll.rs/wasm.rs untouched.
- Shared-file note: machinst/compile.rs + regalloc2
  data_structures.rs touched (env-gated debug dumps only) — flagging
  for x64 agent.

## Merged: coldargs/coldedges (87909ba) + edgefwd (6197594 -> aa02e84)
- Cold-edge mov tax fixed TWO ways: clifpeep::coldargs now rebinds
  hot-defined cold-call args via sadd_overflow (survives egraph), and new
  coldedges pass adds per-edge cold adapter blocks for hot->cold param
  edges. hist checked loop: 17 insts+6 movs -> 11+0; gather 0.607->0.342
  (~1.3x FASTER than stock). regex-syntax: mov 20.5k->19.6k.
- New src/edgefwd.rs (PLIRON_EDGEFWD): edge forwarding past forwarder
  blocks for ALL terminators incl try_call, iconst-forwarder remat,
  inline trap tails. regex-syntax b 9.8k->9.1k (-6.8%).
- Combined merge verified: cargo build + FULL test.sh green.
- Root cause confirmed: block COUNT is the residual gap (~22.7k vs ~10.4k
  blocks) — diamond-merge jumps dominate, needs block-count reduction.

## BUG FOUND (pre-existing): x64 splat.i32x2 lowering gap
Blocks x64 regex-syntax builds entirely (identical with EDGEFWD off).
Track as x64 agent follow-up.

## Still open (ranked)
- hist residual ~1.5x: per-element bounds check not merged into
  loop-exit test (constraint-elimination/indvars territory).
- Block-count reduction: diamond-merge trampolines ~3.9k vs ~704.
- sum2d -5% x64 residual.
- cold-cone remat direction for remaining hist copies.

## clifpeep: foldf multi-pred forwarder fold + coldedges/edgefwd reorder (wt/native-opt)
- MERGE CONFLICT FOUND: edgefwd::bypass_round (no is_cold check) ran
  after coldedges and retargeted straight through the cold adapters —
  movs regressed 19570->20351 on regex-syntax. coldedges now runs LAST
  in the CLIF pipeline (after edgefwd+foldf); adapters survive, mov
  back to 20028 (+640 cold adapter blocks, intended).
- New clifpeep::foldforwarders (PLIRON_FOLDF): multi-pred param
  forwarders — jump retarget incl. escaping params (hazard =
  escape-use in Reach(target\{b}), BFS-checked) and brif-only
  forwarder absorb into jump preds. Edge-indexed writes, fixpoint
  to 8, unreachable cleanup via jumpthread::remove_unreachable_blocks.
  0 hits on regex-syntax post-edgefwd (edgefwd covers the jump
  shapes; brif forwarders don't survive jumpthread's selects); fires
  364x/85 fns with PLIRON_EDGEFWD=0, verify-clean — coverage pass.
- Block anatomy (jump-table data excluded — ~3.1k udf words are
  br_table contents, not insts): real blocks 13,489 vs stock 4,714.
  Dominant residual: ~3.6k small edge-split mov;b copy blocks on
  arg-carrying conditional edges (jumpthread/shape problem);
  186 udf trap stubs; 640 cold adapters; 241 b-tramps (stock 204).
- Verified: cargo build, FULL ./test.sh green, PLIRON_VERIFY on
  regex-syntax rlib + scatter; scatter/hist timings unchanged.
  punroll/edgefwd/wasm/regalloc2 untouched.

## Merged: x64 splat fix (471e2b4) + native ordering fix (e0920af -> c390025)
- splat.i32x2 + all sub-128 vector splat types now lower on x64 (reuses
  128-bit broadcast sequences; emitted by OUR slp.rs Pack::Splat for
  adjacent same-value stores). regex-syntax rlib now compiles x64 e2e.
- INTERACTION BUG caught by native agent: edgefwd bypass_round had no
  is_cold check and ran after coldedges, undoing adapters (movs regressed
  19.5k->20.35k). Fix: coldedges now runs LAST (after edgefwd+foldf).
  Merged tree verified: coldedges at lower.rs:767, post-edgefwd.
- foldf (clifpeep): multi-pred forwarder folding; 0-hit on regex-syntax
  (edgefwd covers) but fires 364 rewrites w/ EDGEFWD=0 — kept as coverage.
- Real residual identified: ~3,608 small b-terminated edge blocks
  (1,799 mov-led) = regalloc critical-edge splits on arg-carrying
  conditional edges — block-param pressure problem, not forwarders.
- x64 jmp count ~4x stock confirmed same root cause (block count).

## Native: vmax icmp-range fold + edge-split provenance (pending)
- `vmax(func, v)` in clifpeep: unsigned ceiling through
  uextend/band/ushr/urem/iconst; the `icmp cc x, imm` fold now fires
  when imm is outside [0, vmax] (ult/ule->1, ugt/uge->0, eq/ne, and
  signed ccs when the ceiling is in the signed-positive half).
  PLIRON_VMAX toggle, default on. Folds `cnt[u8]` checks on
  fixed-size arrays (hist `&mut [u32; 1<<20]` check eliminated —
  loop now 8 insts/iter, no check, no copies).
- Edge-split provenance (crate ablations): 1,799 mov-led splits are
  ra2 critical-edge blocks on arg-carrying cond edges. jumpthread's
  param'd merges = +505 net (removing it costs +1.7k blocks);
  looprot = +75 net but is perf-positive (gather 0.74 vs 0.80);
  punroll = net 0. No single-pass fix — lowering-shape property.
- hist residual resolved: stock LLVM keeps BOTH per-element checks
  (param-len case) — gap is unroll/loop-shape (punroll domain),
  not a missing guard/latch merge.
- Verified: cargo build, ./test.sh green, PLIRON_VERIFY on
  scatter/hist/bc_check/bc_sem (panics still fire), regex-syntax
  block counts unchanged. punroll/edgefwd/wasm/regalloc2 untouched.

## Merged: vmax fold (adccc15, FF) + try_call trap materialization (0e36154)
- clifpeep vmax: tight unsigned ceiling through uextend/band/ushr/urem/
  iconst folds out-of-range icmps -> hist loop 8 insts, 0 checks, 0 movs.
- hist residual resolved: stock LLVM keeps both per-element checks too;
  remaining gap is unroll cleanliness. gather ~parity.
- vendored TryCallInfo.continuation_trap: `call;jmp ud2` -> inline
  `call;ud2` (x64 jmp -11%, a64 b -9.5%, __text -912B). Verified merged:
  build clean, unwind test passes (site registration untouched).
- Edge-block root cause PROVEN: regalloc2 critical-edge splits on
  arg-carrying conditional edges — intrinsic to block-param style, no
  single safe fix. jumpthread is largest source but net-positive.
- sum2d x64 residual: ~2% = noise. Closed/accepted.

## Merged: wt/wasm-perf (87c4e0e..850517e) — wasm correctness + perf work landed
- wseal_exits: domination-safe carrier sealing; 0 skipped leaks (was ~122);
  /tmp/wchk node output byte-identical to stock.
- wtab eq-chain->Select fold + use-escape check — fixes a REAL miscompile:
  folded chain-block defs lost dominance -> backend stub -> unreachable
  trap in WASI create_dir_all. wasi std now green.
- wpeep bytecode peephole (local-shuffle noise); wasm indvars affine
  sites + fixed point.
- Wasm bench vs stock (this build): vadd 1.46 vs 2.05 WIN, axpy 1.53 vs
  1.75 WIN, cnt_ab 1.43 vs 1.66 WIN (was worst gap), sum_f32 ~parity-win,
  matmul_256 12.1 vs 11.3 still ~7% behind (improved from ~12.3), dot_i32
  /sum_u8 marginally slower.
- Verified on shared HEAD: build clean, tests/wasm/run.sh green incl.
  wasi std + unknown-EH.

## Merged: wt/wasm-perf round 2 (eafe6f9) — wpeep forwarding fixpoint + wbcheck Chk::Ind
- wpeep store-forwarding fixpoint: copy-cluster producers retarget onto
  destination locals (structured-context tracking); matmul back-edge
  shuffle gone. wbcheck Chk::Ind: k*n+j affine-IV checks proven via
  uniform latch step.
- matmul_256 wasm residual ~7% DOCUMENTED, not fixable in runway: V8
  TurboFan normalizes both sides to near-identical native code; wunroll
  refuses loops containing check diamonds/panic sinks (>16 blocks) —
  needs waffle-level unroll across unreachable sinks.
- NOT perf-validated timing-wise beyond the agent's /tmp/wbench runs.
- Verified on shared HEAD: build clean, tests/wasm/run.sh fully green.
- KNOWN LIMITATION (all workstreams closed): wasm matmul gap is a
  waffle-level unroll problem; native/x64 remaining gaps are regalloc2
  critical-edge splits intrinsic to block-param style.

## wt/native-opt — sameargs param reduction + ra2 split-invariant audit (uncommitted->committing)
- `sameargs` (clifpeep, post-foldf/pre-coldedges): drops params fed by
  the same value on every edge; merges duplicate params; TryCallRet/Exn
  pseudo-args kept distinct after a real miscompile (fused call rets).
  regex-syntax: 13,896 blocks (−11), 19,987 movs (−41), 1,665 mov-led
  splits (−16), 103,607 insts (−209).
- Dead end DOCUMENTED in native-notes: skipping arg-free critical-edge
  splits in blockorder is unsound — ra2 `inter_block_dests` boundary
  moves need insertion points even with zero CLIF edge args.
  Vendor changes reverted; no vendor diff remains.
- Verified: cargo build, test.sh green, PLIRON_VERIFY=1 clean on
  regex-syntax, bc_check/bc_sem/dse_check/rmw_check green. gather
  neutral (0.35); hist-const is layout-noise-dominated (identical loop
  code flips 0.54<->2.03 across builds).

## Merged: sameargs (b0feedc) + x64 notes (45a00ba) + wasm fwd fix blob
- sameargs (clifpeep): drops same-value block params, merges duplicate
  arg vectors to fixpoint. -37 movs/-10 blocks on regex-syntax — small;
  PROVES the ~1.6k remaining splits are intrinsic to ra2's
  critical-edge-free CFG (no backend patch possible — boundary moves
  still need sites for live-in vregs). Option-3 regalloc patch audited
  as unsound, reverted.
- Fixed: try_call TryCallRet(0)/(1) arg-fusion bug caught by std segfault.
- x64: UNROLL=4 in loopvec already = quad-128-bit groups; UNROLL=8 probe
  LOSES (port pressure). All wide x64 kernels beat LLVM ymm codegen —
  a real ymm regclass confirmed NOT on the critical path.
- x64 hist residual: 1.39x, = ~3 cold-edge ABI-pinning copies
  (r11->rdi->r9->rdi) around each jae per checked element. Fix domain:
  coldargs pin-direct. Stock keeps per-element checks too.
- wasm: BIG uncommitted blob reviewed + committed — wpeep fwd coverage
  fix (conditional defs don't cover, back-edge readers, stale decode
  ctx), wunroll_flat, bound_u64/as_check, fused memargs. All env-gated
  default-on. Full test.sh green on merged HEAD.
- TODO next round: coldarg pin-chain elimination (native), general
  constraint-elimination (x64), matmul check-diamond flattening (wasm).

## wt/native-opt — coldargs umulhi disguised-zero (uncommitted->committing)
- `clifpeep::coldargs` rebind upgraded: `sadd_overflow(x,0)` ->
  shared `umulhi(x,0)` zero per int type per cold block + plain
  `iadd x, z` per arg. No egraph fold for umulhi*0, so the disguise
  survives; flagless `add` replaces `adds`+dead `cset`. i128 keeps
  sadd_overflow (no scalar umulhi lowering).
- regex-syntax: cset 3,206->748, adds 2,465->7, umulh 160->1,746,
  real insts ~103.6k->102,477.
- REJECTED: isle `umulhi(x,0) -> umulh x,xzr` — ra2 reuses the
  materialized zero-vreg elsewhere; removing it cascaded to +218
  insts/+524 blocks despite -392 movs. Vendor diff reverted; no
  vendor change remains.
- Verified: cargo build, PLIRON_VERIFY=1 whole-crate clean,
  ./test.sh green, bc_check/dse_check/rmw_check/bc_sem pass.

## Merged: coldargs umulhi (a981fc4) + celim (ab4fb59) + wasm innermost-first wbcheck (3daf617)
- coldargs: shared umulhi(x,0) disguised-zero per cold block + flagless
  iadd — regex-syntax cset 3206->748, adds 2465->7, umulh 160->1746;
  102,477 real insts (was ~103.6k). Speculative umulhi(x,0)->umulh x,xzr
  ISLE measured WORSE (ra2 loses reusable zero vreg), reverted.
- celim (NEW src/celim.rs ~1100 lines): dominating-condition icmp
  elimination — +243 folds on regex-syntax beyond domcond's 2360, sees
  checks materialized late (switchmap/bcheck/loopvec/punroll/ifconv).
  Facts: brif/br_table edge facts, block-param translation w/
  uniform_slot, +-1 tightening, transitivity, signed<->unsigned. 4
  soundness bugs self-audited pre-commit. Micro-bench: neutral (hist's
  checks aren't dominated — expected). Value is real-world density.
- wasm: wbcheck innermost-first + wunroll loop recollection —
  matmul_256 12.1->10.33ms = ~8% FASTER than stock (was 7% behind).
  wunroll_flat fires; new hot path: static guards -> k<249 group test ->
  flat x8 pure f32 loads/mul/add.
- Wasm remaining: sum_u8 +5%, dot_i32 +2% (V8-normalization noise).
- Native deferred items logged: strength-reduce non-direct stream
  bases, two-phase reverse, residual loopvec epilogue work.

## wt/native-opt — offset-affine bounds-check guards (committing)
- `guard_dead`/`guard_pred` generalize to `iv ± k` and `c - iv`
  compared index exprs (`iv_add_k` destructuring in loopidiom).
  neg_off 0.459 -> 0.078 ms/iter (5.9x, stock 0.058); pos_off/
  off_read parity. guard_pred now appends multiple preds via
  `&mut Vec<Pred>` (no-wrap `iv0 >=u |k|` + `satsub` bound covers
  + post-tested first-iter covers); loopidiom::plan updated.
- Host-side overflow hygiene: checked_add/checked_neg on the
  i64 proofs (iv0/k can be MIN-shaped consts).
- hist re-audit post-umulhi: hot unrolled loop is mov-free — no
  pin bounce around b.hs bounds checks; all rebind work lives in
  cold blocks. hist-rand 0.373 vs 0.436 (+17% vs stock).
  hist-const / same-address-hist variance (0.8-2.0 both compilers)
  is microarchitectural store-forwarding noise, not codegen.
- celim on aarch64 regex-syntax: 105,545 -> 102,857 insts
  (-2,592), blocks -569; PLIRON_VERIFY clean — helps here too.
- Verified: cargo build, test.sh green (incl wasm), PLIRON_VERIFY
  on probes + panic/wrap tests; wide suite all >= stock.
- Still scalar (next candidates): desc_sum `(0..n).rev()` —
  descending counted iv needs step<0 in count() or a rev-norm
  pre-pass; stride2 non-unit step; rev_inplace half-trip.

## wt/native-opt — revnorm descending-iv rebasing (committing)
- NEW src/revnorm.rs (PLIRON_REVNORM, after licm, before loopidiom):
  rebase `iv: hi→lo` desc counted loops to `j: 0→hi-lo` with iv=hi-j.
  Exact for `eq`/`ne` stay tests any step; `ugt 0` for unit decrement.
  `iv±k`/`k-iv` defs become `(hi±k)-j` with preheader constants; bare uses
  get per-block `isub(hi,j)`; icmp single-use check before operand rewrite.
- desc_sum 0.304 -> 0.074 ms/iter (4.1x; stock 0.073) — full neg-stream
  NEON path (descending ldp + rev64/ext lane fixup + uaddw accum).
  step_by(2) normalizes too but stays correctly scalar (strided load).
- Bug caught+fixed by test: keep icmp RAW polarity (body-on-false brifs
  mean the cond is the exit test) — inverted exits returned empty sums.
- Verified: cargo build, PLIRON_VERIFY=1 ./test.sh green incl wasm,
  rev probes (rsum/rcopy/rrange/rwhile/ridx/panic) match stock exactly
  (incl panic index), stride2 2550 both, r42 rustlantis 6k-line clean.
- Remaining: stride2-class non-unit-step vectorization, rev_inplace
  (half-trip swap, 0.21 vs 0.17 stock), shuffle-tree epilogue.

## Merged: revnorm+affine-guards (5805a7f,0e5dc33) + celim v2 (3b647d0) + wasm flat-unroll cursors (0b6614b)
- revnorm.rs (NEW): descending counted ivs rebased to ascending via
  iv=hi-j bijection — desc_sum 0.304->0.074 (parity). loopidiom/loopvec
  affine iv+-k guard proofs — neg_off 0.459->0.078.
- hist PIN-CHAIN ELIMINATED (umulhi coldargs worked): unrolled hist loop
  mov-free; hist-rand 0.373 vs stock 0.436 = +17% WIN. hist-const
  0.8-2.0ms flips are Apple memory-renaming noise (same-addr RMW chain).
- celim v2: 310 folds/101 fns on regex-syntax; also verified on aarch64
  (-2592 insts, -569 blocks). Guard-band collapse fired in hist.
- wasm: sum_u8->parity, max_u32 -38% WIN, matmul -11% WIN (9.97 vs 11.21).
  Only deficit: dot_i32 +2.5% (noise; stock doesn't unroll it at all).
- v128 SIMD verdict (wasm): NOT cheaply wireable — wasm path bypasses
  CLIF entirely (pliron-dialect -> waffle -> bytecode); vectors
  scalarized via wleaves. A waffle-level vectorizer is a multi-day
  project. Documented as deferred.
- NEW x64 gap identified: check-exit fusion — LLVM exits loops on
  i==len doubling as bounds check; we keep per-iter i<len checks.
  Fix = bcheck preheader versioning (hoist i<len for all i<n to a
  single n<=len preheader test). Also multi-pointer loop copy chains
  (~5 movq/lea per iter on gather/scatter via rotation merge params).
- CAUTION: x64 agent's "larger workloads" table showed anomalies vs
  earlier runs (matmul 2.5x slower at that size, new kernels chars/
  tokenize/prefix/dot_u32 slower). Different sizes/harness — needs
  reconciliation before treating as regressions.

## bcheck affine coverage (wt/native-opt, uncommitted)
- Follow-up to the x64 "check-exit fusion" finding. bcheck already
  versioned fully-affine loops (gather, sum2d); this round extended
  the affine analysis: `isub` both directions (`a[i-k]`, `a[c-i]`),
  invariant consts as sign-extended K terms (was zext'd V — every
  `a[i-k]` guard silently failed), dead `brif iconst` cold brifs no
  longer counted as residual checks, and min/max guards for
  decreasing sequences (`sum_b u< len && hi sge 0`, min-check elided
  for provably non-negative affines).
- New fires: rev_copy32, scaled (both now fully check-free fast
  loops), plus neg_idx/off_idx/shift_cp/gather_chk probes. Outputs
  and panic indices match stock incl. n>len and wraparound edges.
- Partial (subset-covered) versioning deliberately stays gated for
  single-foldable-check loops with non-affine stores: it wins 28%
  on random-key hist but costs 44% on same-key hist via same-addr
  store-forward replays — the removed branch's issue slots are
  load-bearing spacing. Affine-store (`dst[i]`) and load-only
  partial loops DO version now. PLIRON_BCHECK_PARTIAL=1 overrides.
- regex-syntax -O: same 7 versionings, +17 insts net, cond-branch
  count unchanged. test.sh PLIRON_VERIFY=1 green.

## Merged: bcheck affine versioning (9db2ec2) + ivrefold-optin (7c52988) + wasm GC+fusion (abe6e62,942cb56)
- bcheck: affine() now handles a[i-k] (isub) and a[c-i] (negated terms,
  Lin::N runtime slopes); consts sign-extend into Lin::K — prior
  zext'd-const failures SILENTLY killed every i-k guard. Min/max guards
  for decreasing sequences + dead-const residual accounting. Fires on
  rev_copy32, scaled, gather_chk, shift_cp — check-free fast clones.
  PLIRON_BCHECK_PARTIAL=1 folds RMW-store checks too but regresses
  same-address RMW +44% (issue slots feed store->load forwarding;
  stock keeps a per-iter check on that shape too). Default keeps it.
- ivrefold.rs (NEW, PLIRON_IVREFOLD=1 opt-in): refolds loop-carried
  affine params onto one anchor IV — works mechanically (780 slots
  gone, LLVM-shaped amodes) but NET-NEGATIVE (ra2 serializes
  recurrences; gather/scatter lose). Correct pass, wrong regime.
- Benchmark anomaly RECONCILED: chars/tokenize/prefix/dot_u32 are NEW
  kernels; matmul anomaly was a 64x64 size not the 256 win. Real x64
  residuals: hist 1.4x, gather 1.6x, scatter 1.2x — and NEW: tokenize
  (split_whitespace) ~5x — UTF-8/iterator codegen, cross-target
  candidate.
- wasm-ld GC: mark-sweep dead-function/data/GOT/table collection —
  wmini 1.41MB->204KB (was 30x stock, now 4.3x; residual = per-function
  codegen volume: locals, SP traffic, GOT indirect addressing).
- wload fusion: load+ext -> i64.load{8,16,32}_{s,u}; dot_i32 emits
  stock shape. Bulk-memory ops (memory.copy/fill) already emitted.
- Next: tokenize 5x (native/x64), gather 1.6x (x64), wasm code-density
  4.3x (wasm), rev_inplace/stride2 (native).

## wt/native-opt — tokenize crash+perf, rev_inplace (committing)
- FIX (correctness): vcode pair fusion could merge `ldr x0,[x0]` with a
  dependent `ldr xN,[x0+8]` — two vregs sharing one preg across disjoint
  live ranges makes the amode base test pass while the second load's
  address semantically uses the FIRST load's result. Emitted
  `ldp x0,x20,[x0]` → EXC_BAD_ACCESS in chars().filter().count(). The
  adjacent-inst fast path skipped the partner-uses-vs-defs hazard check
  the gap>0 paths apply; now applied there too. tests/pairfuse covers it.
- NEW src/brchain.rs (PLIRON_BRCHAIN, last pre-define_function): br_table
  with ≤4 live entries → icmp eq+brif chain (default-identical entries
  skipped). split_whitespace count 20.6→13.2 ms (-36%). Cross-target —
  same transform should lift most of x64's 5x split_whitespace gap.
- licm: MAX_MOVES was a global per-fn budget — one loop's iconsts starved
  all later loops (SplitWhitespace end-ptr reload per char). Now per-loop
  cap only, global guard 96*32. Added skip reasons under LICM_DEBUG.
- tokenize residual ~2.6x documented in native-notes: Option<char>
  sentinel plumbing + branchy is_whitespace boolean + param movs —
  ~28 insts/char vs ~11; chars/bytes filter same class (bytes filter:
  stock NEON-vectorizes, we don't).
- rev_inplace a.swap(i,n-1-i): 0.227 vs 0.224 = PARITY (old 0.212/0.173
  was stale; memory-bound scalar both sides). slice::reverse 0.228 vs
  0.091 is the real gap — mirror-swap vectorization (loopvec stream).
- Verified: cargo build, PLIRON_VERIFY=1 ./test.sh green + pairfuse,
  wide all ≥ stock, bc_sem/rmw_check/dse_check match stock outputs.

## Merged: brchain+licm+pairfuse (b69a68c) + uloads32/revnorm-fix (43330d0) + wasm density (f8adc73)
- REAL MISCOMPILE x2 fixed: (a) dependent-load pair fusion — ldp
  x0,x20,[x0] with base redefined; hazard check now applies on the
  adjacent-instruction path; tests/pairfuse wired into test.sh.
  (b) revnorm exit-edge — Phase B skipped iv uses in exit blocks;
  descending loops read ascending j; segfaulted insertion_sort (x64
  ~50% std fail rate; latent on aarch64). Rewrites all blocks.
- ivrefold deadrec fix: inst_fixed_args missed call/try_call varargs.
- brchain.rs (NEW): <=4-entry br_tables -> icmp+brif chains; tokenize
  split_whitespace -36% (20.6->13.2ms native). licm MAX_MOVES now
  per-loop (was per-function, one loop starved the rest).
- uloads32: uextend.i64(load.i32)->uload32.i64+ireduce.i32 kills a
  stray movq; x64 gather now PARITY (9-insn check-free fast loop).
- tokenize residual ~2.6x native / ~5x x64(pre-brchain): Option<char>
  sentinel plumbing (select->ireduce->icmp), branchy is_whitespace vs
  stock's lsr+tbnz constant bitmask, ~5 spilled loop-carried values.
- wasm density: GOT->const fold (334->22 globals), local decl merging —
  residual 4x vs stock is FUNCTION COUNT (390 vs 99, inlining depth);
  per-function density ~parity (345 vs 336 B).
- NOTE: a merge commit accidentally carried conflict markers into
  lower.rs — caught and repaired via amend; resolution keeps uloads32
  then brchain order.
- Next: bittab (set-membership bitmask) + Option-sentinel fold
  (native), scatter non-versioned-check investigation + tokenize
  re-verify (x64), function-count inlining (wasm).

## wt/native-opt — edgespec + bittab for tokenize (committing)
- src/edgespec.rs (NEW): threads pred edges through pure
  brif/br_table/jump dispatch blocks decided by supplied block-param
  args (Option/enum sentinel plumbing); symbolic eval of
  const/select/ireduce/icmp/band chains; PLIRON_EDGESPEC=0 off,
  PLIRON_EDGESPEC_LIMIT=n bisects rewrites per function.
- src/bittab.rs (NEW): sparse icmp/brif membership trees (accepted
  set ⊆ 64-window, pure interior, 2-outcome) → guarded bitmask
  isub+icmp.ult.64+select+ushr+icmp.ne+brif. char::is_whitespace in
  SplitWhitespace::next → mask 0x1_0000_3e00. PLIRON_BITTAB=0 off.
- MISCOMPILE FIXED: edgespec threaded past a dispatch block whose
  params carry the advanced iterator cursor — the rebind was skipped,
  cursor went stale, split_whitespace spun forever (verifier-clean).
  Fix: per-path skipped-block set; uses/args/conds of skipped defs
  rejected unless the def re-dominates the use in the landed cone.
  Regression: tests/edgespec/main.rs (in test.sh).
- tokenize 4.4MB vs stock: count 13.0/6.7 (1.95x), collect 15.5/9.1
  (1.7x), chars-filter 6.7/2.96 (2.3x, was 8.6 → -22% from edgespec),
  bytes-filter 4.3/0.95 (4.5x — stock NEON-vectorizes predicate).
- rev_inplace u8/u64 parity (0.221/0.215, 0.128/0.127; memory-bound).
  slice::reverse u8 0.244 vs 0.022 (11x — SIMD byte reversal gap),
  u64 0.156 vs 0.091.
- Verified: build clean; PLIRON_VERIFY=1 ./test.sh green (incl new
  edgespec test); harness tier-0 smoke+determinism OK; cpubench
  rmw/dse/bc/drev outputs match stock.

## Merged: bittab+edgespec (4390fd3) + x64 bt fold + winline (7eb7090)
- bittab.rs (NEW): sparse set-membership -> select(guard,mask,0)+ushr+
  icmp ne 0 bitmask. is_whitespace fires (mask 0x100003e00). x64 bt ISLE
  rules fold it to btq+setb.
- edgespec.rs (NEW): edge specialization threading — CAUGHT real
  verifier-clean infinite loop (threaded past cursor-rebinding block);
  skipped-block tracking + PLIRON_EDGESPEC_LIMIT bisect. tests/edgespec
  wired into test.sh.
- tokenize: 5.0x->2.56x x64, ~2.6x->1.95x aarch64. Residual = 11 live
  params -> ~5 spill slots/char (structural regalloc pressure) + mask
  iconst remat per-iter (ra2 remat vs LLVM hoist).
- winline (wasm): waffle-IR inliner — callee<=12 always, single-
  callsite <=300. wmini 194->183KB, 390->294 funcs, 1339->113 calls.
- scatter: PLIRON_BCHECK_PARTIAL=1 beats stock (0.450 vs 0.52) but
  default stays off (hist same-addr RMW regression).
- BIGGEST remaining kernel gap: slice::reverse u8 11x (LLVM SIMD byte
  reversal vs our scalar) — mirror-swap vectorization.
- wasm remaining: multi-callsite mid-size callee inlining needs
  callgraph-aware cost model; dot_i32 +2.4% noise.

## wt/native-opt: mirror-swap vectorization — slice::reverse 11x → parity
- src/lower.rs: constant-size `llvm.memcpy`/`memmove`/`memset` inline
  expansions + `small_copy_or_call` fast path now use `plain_mf(op)`
  (`notrap` at -O). Without it, `ptr::swap`-expanded memops bailed
  loopvec as side-effecting `other` insts.
- src/loopidiom.rs: `deadend` follows `try_call` normal-return edges
  (last exception-table target) and cold `jump` chains to `trap` —
  unwinding panic paths in functions with cleanup are diverging dead
  ends for guard proving (guard_dead/guard_pred callers).
- No loopvec changes needed: `neg`-stream load/store `vreverse`
  machinery already handles the crossed two-sided swap.
- slice::reverse u8 0.244→0.022 (stock 0.022, was 11x), u64 0.156→
  0.092 (0.091, was 1.7x). rev_inplace u8 0.221→0.022 (stock scalar
  0.213 — now ~10x faster than stock), u64 0.128→0.092 (0.127).
- Side effect: any loop over small-copy/memset-expanded memory is now
  vectorizer-eligible (notrap on the expanded ops).
- Verified: build clean; PLIRON_VERIFY=1 ./test.sh green; exhaustive
  reverse correctness (sizes 0..=300 + boundary straddles, u8..u64);
  drev/drev2/bc/rmw/dse/pairfuse/edgespec fixtures pass.

## Merged: mirror-swap unblock (a77cb71) + scatter gate (d63f87a) + winline cost model (cf8ac5f)
- slice::reverse CLOSED via two latent gates, zero loopvec changes:
  (1) inline memcpy/memmove/memset lacked `notrap` — loopvec bailed on
  "side effects"; now plain_mf at -O (any loop over small-copy memory
  becomes vec-eligible). (2) loopidiom deadend couldn't see through
  try_call normal-return continuations.
- slice_reverse u8 11x->PARITY (0.022), u64 1.7x->parity; rev_inplace
  ~10x FASTER than stock (LLVM doesn't vectorize manual swap either).
- scatter gate refined: bail only on non-affine store AND non-affine
  load together. scatter versions by default -> x64 WIN (0.452 vs
  0.512). RMW correctly stays gated.
- winline cost model: gc_done/loop-hot/ref.func rules — neutral on
  wmini (residual is structural mid-size multi-callsite callees;
  needs bottom-up inline ordering).
- Fresh x64 table: gather WIN, scatter WIN, sum_u8 5.4x, dot_u32 1.5x;
  residuals: hist 1.44x, chars 1.36x, matmul-64 2.6x, tokenize 2.59x.
- Native residuals: tokenize ~1.95x (11-live-param state machine —
  same gap on aarch64 with ~1 spill -> NOT regalloc), chars().filter
  2.3x, bytes().filter 4.5x (stock NEON-vectorizes byte predicates —
  predicate-count SIMD idiom missing).
- Next: bytes().filter vectorization (native), hist residual decomp +
  matmul-64 (x64), whole-crate wasm (wasm).

## bytes().filter count-if vectorization (wt/native-opt)
- src/lower.rs: second `ifconv` round before loopvec (`PLIRON_IFCONV2`,
  fixpoint ≤8) — loadfwd/egraph had already dedup'd the predicate arm
  re-loads that made jumpthread's ifconv bail on non-pure side arms.
  Dead forwarders removed, then `fusechains` splices single-pred latch
  merges into the header.
- src/ifconv.rs: side-arm speculation cap 4→8 insts (nested
  range-check diamonds clone ~3 compares per level).
- src/loopvec.rs: `Widen::Cnt` — count-if reduction `acc += bool`:
  `bools` lattice (masks ∪ {0,1} consts ∪ band/bor/bxor/select trees,
  mod-2-faithful) + `band 1` normalize + unsigned widen-sum
  (uaddlp/uaddw aarch64, psadbw x64). Readers of bool-tree nodes must
  produce bools. Also: sibling-latch duplicate-exit check no longer
  compares `post_tested` (per-block property, not trip agreement) —
  unblocks `filter().map().sum()` conditional-reduction loops.
- byte filters now BEAT stock (uaddlp chain > movemask+popcnt):
  alpha 0.056 vs 0.157 (2.8x), digit 0.037 vs 0.147 (4x),
  alnum 0.078 vs 0.236 (3x), gt 0.027 vs 0.059 (2.2x),
  eq 0.027 vs 0.142 (5.3x), sum_if 0.032 vs 0.069 (2.2x).
- Residuals documented: chars().filter = UTF-8 decode FSM (2.4-3.4x,
  needs dedicated SIMD decode); (a..b).contains = br_table dispatch.
- Verified: build; PLIRON_VERIFY=1 test.sh green; n=0..300 x 7 fns
  correct; bc/rmw/dse/drev/condred/early2 pass; cpubench unchanged.

## Merged: count-if vec (5e650ca) + UBOUND (2367df8) + wasm real-crate (c2928dd)
- loopvec Widen::Cnt: cnt+=uextend(icmp)/select(pred,1,0) -> band(v,1)
  + uaddlp/uaddw (psadbw x64). bytes().filter().count() 4.5x BEHIND ->
  2.8-5.3x FASTER than LLVM (uaddlp > movemask+popcnt on M3).
  + PLIRON_IFCONV2 second ifconv round + fusechains latch merge to
  flatten filter CFG diamonds.
- bcheck UBOUND: vmax-proven data-dependent index checks version to
  tymax<len preheader guard — hist 1.44->1.28x both arches, multi-key
  hist beats stock. Degenerate single-key ~1.3-1.6x regression
  (forwarding-replay) accepted, matching LLVM's unconditional
  versioning.
- matmul-64 = gather/unit-stride-interchange feature gap (stock does
  lane-gathers + vpmulld; needs gather support — documented).
- wasm real-crate (wbig, regex+std): ~2x total size, ~1.32x runtime,
  identical output hash. Dead-return peep (-1964 opcodes) + wsel
  diamond->Select.
- CORRECTNESS FLAG: pre-existing miscompile in
  symbolic_name_normalize_bytes — stubs with OOB "len is 2, index 2"
  on cold path. Latent backend bug, needs investigation (wasm wt).
- Residuals: tokenize ~1.95-2.6x (state machine shape), chars 2.4-3.4x
  (UTF-8 decode FSM — needs dedicated SIMD decode), wasm runtime
  ~1.32x on real crate.

## WIP wt/native-opt: niche/nonnull decoding (uncommitted)
- context.rs/lower.rs: rustc `nonnull` arg attrs by leaf index ->
  NonNull fact seeding (PLIRON_NNARGS); jumpthread NonNull lattice
  (nonnull loads, args, inbounds offsets, select arms, param fixpoint).
- jumpthread: `icmp (select c, X, K), K` -> c/!c via `ne_params`
  greatest fixpoint (PLIRON_SENTINEL) — folds Option<char> -1 test and
  Option<NonNull> 0 test to the guard flag; param-null fold to guarding
  brif cond (PLIRON_PARAMNULL); `sameargs` const-key + `const_sweep`
  (PLIRON_CSWEEP); iconsts excluded from MAX_BODY (PLIRON_BODYK).
- clifpeep::sel_regions (PLIRON_SELREGION): select arms folded when all
  uses sit under a decided brif region — frees Chars' carried
  select(end?,ptr,ptr+1) to a plain iadd.
- chars: chars_count parity (0.037); ascii 2.4x->1.6x; mixed ~1.9x;
  alpha ~2.9x; tokenize ~2.3x. ASCII hot loop = ldrb+add+cmp+b+tbnz;
  Option selects/sentinels gone. Remaining: multibyte safe-load
  selects (dep-bound cmp+csel) vs stock post-increment.
- contains_rng: NOT br_table — scalar count(lo<=i&&i<=hi) 2.2x; stock
  vectorizes the range reduction (8x u32 cmhs). loopvec gap; also
  spilled-hi reload in loop.
- Fixed en route: vacuous param-null fold on entry params (saw_x);
  k!=0 icmps leaking into x==0 paths; AKey::Other raw BlockArg
  (TryCallRet(0) vs (1) unwind crash); const_sweep eval mask+sext;
  ne_params self-edge/unreachable-pred soundness.
- Verified: cargo build; PLIRON_VERIFY=1 ./test.sh green; chars/contains
  correctness incl. empty/mixed-width/char::MAX edges.

## Merged: niche decoding (d7fed94) + punroll brif-latch (1783829) + wasm stale-param fix (1a634e2)
- jumpthread NonNull lattice (ne_const/ne_params fixpoint), sentinel
  select-fold, param-null fold, const_sweep, sel_regions edge-arg
  folding, nonnull fn-arg attrs. chars_ascii 2.4->1.6x, chars_alpha
  3.4->2.9x, chars_count PARITY.
- punroll: entry-block-param bounds accepted; rotated jump-latch loops
  supported; full-unroll 12->16 (K=8 on versioned checked loops).
  hist ~PARITY both arches (0.24 vs 0.23); gather/scatter now WIN.
- wasm: stale ValueDef::BlockParam miscompile FIXED (waffle params
  shrink, defs stale -> wbcheck indexed past edge args -> OOB stub).
  Ownership checks x4 sites; wbig 0 stubs; PLIRON_WASM_PANIC debug.
- REGRESSION FLAGGED (x64): ifconv cap 4->8 hurts chars().count() ~2x
  on x64 (predictable branches -> selects). Needs profitability guard.
- Remaining: chars multibyte 1.6-2.9x (UTF-8 select chains / SIMD
  decode), tokenize ~2-2.6x, contains 2.2x (needs u32 2-compare range
  reduction — Widen::Cnt extension), spilled-hi stack-slot forwarding,
  matmul-64 (gather/interchange), wasm real-crate runtime ~1.32x.

## WIP: IV-only range-count vectorization + ifconv2 chain/profitability rework (wt/native-opt)
- `(lo..=hi).contains(&i)` count loops now vectorize: 0.588 -> 0.278 ms
  aarch64 (stock LLVM 0.257 — parity), ~2.1x vs shared baseline.
- cparm (clifpeep): narrow const-param fold ahead of ifconv2 — drops a
  block param only when every non-self edge feeds the same iconst,
  chasing through uniform interior params (RangeInclusive's
  latch-carried flag reaches the header via block20 <- block18 <-
  block1). Replaces the early full-`sameargs` call that regressed
  chars_alpha_ascii ~70% (1.466 -> 0.880): V-param merges/dup-param
  merges stay out of the early round. The materialized const also
  rewrites chased interior params so `select flag` arms fold;
  const_sweep then kills the br_table dispatch.
- ifconv: resolve_arm walks pure forwarder chains (<=8 hops) for brif
  and br_table conversions; per-hop param substs remap via `thru`.
  Chained arms gate on `flat_loop` (convert only when the enclosing
  loop ends branch-free — the vectorizer-bound shape); wide
  single-hop arms keep `bool_merge`, so the predictable UTF-8 width
  dispatch stays branchy.
- loopvec: `upd_decomp` recurses nested conditional selects —
  `select(c1, select(c2, acc+d, acc), acc)` = `acc + d & (c1 && c2)` —
  into `Cond.cs` terms; emit_mask bands them; `aux` covers the nested
  selects + `acc ⊕ d` insts. IV-only loops (no body mem op) use the
  reduction update inst as the post-test probe.
- lower: cparm call + `remove_dead_insts` after it — dead iconsts in
  folded arm chains would otherwise eat the SPEC_WIDE budget.
- Tombstone guards: emit_arm's derived-map lookup skips reserved alias
  dests (`value_alias_dest_for_serialization`), same ICE class as the
  earlier sameargs_round crash (deadrecs was already guarded).
- hi-spill: resolved — lo/hi materialize as `movi.4s` splat constants
  in the vector loop; only stack traffic is the frame prologue.
- Generated (contains_rng): dup.4s iv splat + add.4s lane IVs,
  cmhs.4s x2 (lo/hi u32 compares), and.16b mask-AND + &1 normalize,
  uaddw.2d/uaddw2.2d onto i64x2 accs, punroll'd 4x (16 elem/iter).
- Measured aarch64 vs c31afff: chars_count 0.037 (=), alpha 1.11-1.17
  (was 1.447), alpha_ascii 0.83-0.88 (was 0.873 — regression fixed),
  mixed_utf8 1.61-1.70 (was 2.194), tokenize 2.38-2.52 (was 2.595),
  contains 0.263-0.278 (was 0.588).
- Validated: cargo build; PLIRON_VERIFY=1 ./test.sh green; contains
  edge cases (x=0, lo=hi, u32::MAX bound) == scalar ref; filt_edge
  mixed-width boundary lens 0..129; filt_probe/filt2/panic_bcheck/
  charcheck outputs identical to stock. x64 not measured (aarch64 host).

## Merged: contains-parity+cparm (0790709) + punroll sink_test (3112404) + wasm wrange fixes (13b866e)
- contains() at PARITY (0.263-0.278 vs 0.257): Widen::Cnt nested-select
  decomposition acc+d*(c1&c2); u32 two-compare range reduction emits
  cmhs.4s x2 + uaddw accumulators, punroll'd x4.
- cparm: const-fed param drops chasing virtual uniform-param links —
  replaces early sameargs (chars_alpha_ascii regression). ifconv
  chained arms gated by flat_loop — keeps count-if gains, no UTF-8
  branch regression. Fixed resolve_aliases ICE.
- punroll: rotated jump-latch unrolling + sink_test — matmul-64
  3.4x->1.45x (residual = strided B-column SIMD vs scalar, loopvec
  stream_addr can't gather). gather/scatter x64 now WIN.
- wasm x2 REAL MISCOMPILES: wrange use-before-def (appended values
  used earlier in block) + mul->shl wrong operand with both-const
  (dlmalloc OOB). Plus waddr memarg offsets, loop_licm frozen-param
  hoists, wdce dead-deref loads, inline-attr honoring.
- wbig: 1.83x size, ~1.29x runtime vs stock.
- DISK: cleaned /tmp/pliron-head + incremental caches (1.1G->16G free);
  watch target/ dirs (~190G total across 4 checkouts).
- Remaining: chars ~2.2x native (UTF-8 select chains — post-inc idiom
  or SIMD decode), tokenize ~2.4x, matmul-64 strided-vec, wasm
  transition-ptr reload (unhoistable through clobber-unknown call).

## WIP→merge: chars residual decomp + mask01 (wt/native-opt)
- **chars decomposition (chars_alpha hot loop)**: stock LLVM emits a
  branch-oriented UTF-8 decoder — `ldrsb w9, [x8], #1` post-incremented
  first-byte load, direct width dispatch (1/2/3/4-byte cursor bumps),
  no safe-load selects, no sentinel merge. Pliron's loop carries
  `icmp cursor==end` + `select` safe-address chains per byte load and
  a flag/payload niche merge (`select flag, payload, -1`) — ~2x the
  per-iter instruction count of stock; that, not decode math, is the
  residual. chars_alpha 1.11-1.17 vs stock 0.51 (still ~2.2x).
- **inbounds-derived nonnull** (`PLIRON_INBOUNDSNN=1`, default OFF):
  rustc's `getelementptr inbounds` flag now also seeds `derived`
  (our own `inbounds_gep` already did), letting jumpthread's
  null-guard folds collapse the safe-load selects — produces visibly
  cleaner CLIF (direct `uload8` cursor loads, `brif` dispatch) but
  emitted asm is byte-identical in the hot loop and chars gains
  nothing; the extra folds reshape the split_whitespace dispatch
  (~8% tokenize regression). Kept as an experiment knob only.
- **edgespec return-clone** (`PLIRON_RETCLONE`, investigated then
  REVERTED): inlined trivial `return` tails from dispatch blocks —
  cleaner IR (bare `jump` forwarder, direct cursor bumps) but zero
  chars gain, ~8% tokenize cost from clone placement, AND a real
  verifier bug on regex-syntax (non-dominating use surfacing at the
  bittab verify). Reverted to HEAD entirely.
- **mask01 (clifpeep, default ON)**: `band v, 1` where `v` is provably
  0/1 (is01: icmp/fcmp result, uextend/ireduce/select of 0/1) is
  rustc's i1-normalization residue — one `and` per element in scalar
  count_if loops. Runs after sameargs (when the merged predicate
  param has been alias'd to its select def). Scalar-only (vector icmp
  lanes are all-ones masks). 118 hits crate-wide on regex-syntax;
  scalar countif inner loop drops `uxtb + and #0x1` → a single
  `add x0, x0, w3, uxtb` (−2 insts/iter).
- **Fresh regex-syntax rlib counts** (`-O -C cgu=1`, llvm-objdump,
  countbb.py): vs the numbers last recorded ~10 merges ago:
  total insts 102,875 → 99,518 (stock 46,195 — 2.15x); real blocks
  13,489 → 12,102 (stock 4,714 — 2.57x); mov 20,519 → 18,621
  (stock 7,816); uncond `b` 8,753 → 6,583 (stock 2,224, −25%);
  mov-led ≤3-inst b-splits 1,799 → 1,266; pure-b trampolines
  241 → 506 (stock 204); udf data-words still ~2.6k pseudo-blocks.
- Bench (final build): chars_count 0.037 (=), alpha 1.173,
  alpha_ascii 0.880, mixed 1.70, tokenize 2.42-2.52, contains 0.278.
- Validated: cargo build; PLIRON_VERIFY=1 ./test.sh green; filt_edge
  (UTF-8 widths, lens 0..129), contains_test, charcheck, scalar
  countif all pass; gated paths (INBOUNDSNN) verify-clean on
  regex-syntax. x64 not measured (aarch64 host).
- Remaining: chars ~2.2x (carried select chains — needs post-inc
  load idiom or a wider UTF-8 batch decode, not IR polish),
  tokenize ~2.4x, matmul-64 strided-vec.

## Stage2 self-host crash — diagnosis so far (Oct 10 PM)

Reproduced directly: `rust-sh/build/.../stage2/bin/rustc` (pliron-built
stage2) panics compiling `core` — `thin_vec::drop_non_singleton` capacity
overflow inside `rustc_lint::levels::add` during pre-expansion lint. On a
trivial `fn main` it SIGBUSes after E0463. So the stage2 *binary* is
miscompiled, not just one path.

- The embedded backend in rust-sh was **very stale** (Oct 9 rsync): missing
  revnorm/edgefwd/celim/bittab/dse/ivrefold — predates ~15 merges and five
  known miscompile fixes (punroll SSA dominance, edgespec skipped-block,
  wpeep coverage, volatile-store fencing, dependent-load pair-fusion).
- Rebuild hit a second latent issue: `fat_lto_supported` is
  `#[cfg(rustc_in_tree)]` and requires Hayden's rustc-side patch
  `cf54c772450` which never reached rust-sh (worktree was created before
  the patch existed; backend was never rebuilt in-tree since). Applied the
  cherry-pick to rust-sh (detached `4211e93`) and mirrored it as
  `selfhost/rustc-session.patch` + wired into `selfhost/setup.sh` so fresh
  worktrees get it.
- stage1-std + stage2 rebuild in progress against backend `06cdcc6`
  (current HEAD incl. mask01 + noalias-LICM merges). Bad stage2 driver
  snapshotted to /tmp/stage2-bad/ for bisect if needed.

## Strided lane-gather vectorization — matmul-64 closed on both arches (wt/x64-codegen, a730090)
- src/loopvec.rs: `Stream.stride` + `emit_vload` — strided loads
  (whole-element byte multiple >= 2*ebytes, `MAX_GATHER=2048`)
  emit per-lane `load.elem` + `splat`/`insertlane` (LLVM's
  movd/pinsrd column idiom). Loads only; strided stores still
  bail. `VPred::Pair` and strength-reduced steps scale by stride.
- `upgrade_lagged` + `param_coeffs`: `Param::Other` whose latches
  all pass one affine combination of `Step` params upgrades to a
  deferred `Step` — unblocks rustc's lagged bounds-check/ptr
  params (matmul k-loop header was all-"?").
- `exit_bare`: arg-free counting-exit target cloned into the
  vector epilogue; `rem==0` jumps to the clone so post-tested
  loops take the full `iters & -K` count. loopidiom trips gained
  the do-while `ult` arm (`SatSub`/`Div`, `ins_eval` now
  pub(crate)).
- Const-trip gate: strided streams with `iters < vf*UNROLL` bail
  (dead vector clone otherwise).
- MEASURED (best-of-10): matmul-64 0.161->0.123 x64-Rosetta
  (stock 0.088; 1.83x->1.39x), 0.162->**0.074** aarch64 (stock
  0.070 — PARITY). matmul-256 10.40->9.97 / 10.25->9.95 (~stock
  parity both arches). colsum256/sdot/nth2/nth4 neutral-or-win.
  All checksums match stock.
- TRAP: `PLIRON_VEC=0` also disables ifconv2+cparm — matmul-64
  "novec" 0.095 was a confounded baseline; real shipped scalar is
  ~0.16 on both arches. Use `PLIRON_VEC_SKIP=` for per-fn A/B.
- PRE-EXISTING BUG FLAGGED (native wt, cparm): fuzz seeds 0:24 →
  13 `ifconv2 broke` ICEs (non-dominating value); identical at
  919ba07 and 140eb02, `PLIRON_CPARM=0` clears. cparm also causes
  the matmul-64 2x scalar regression noted above (0.16 vs 0.08).
- Merged shared HEAD 06cdcc6/140eb02 first — fast-forward, no
  conflicts (delta didn't touch loopvec/loopidiom). Cargo.toml/
  lock stay dirty locally (all-arch) — not committed.
- Validated: PLIRON_VERIFY=1 test.sh green, harness tier-0 green,
  gather_mm/gather_check strided correctness (N=1..256, strides
  1..128, u32/u64, do-while) ALL OK both arches + Rosetta.
- Remaining: chars ~1.8x x64, tokenize ~2.3-2.6x, nth2_u64 a64
  1.8x (early-exit gate, pre-existing), hist ~parity, matmul-64
  x64 1.39x residual.

## Self-host / cparm dominance fix (3b20008)

- The reported "stage2 crashes compiling core" was a STALE stage2
  binary: its embedded backend predated ~15 merges incl. five
  miscompile fixes. After rsyncing current HEAD, `core` compiled
  cleanly; the failure moved to `alloc`.
- `alloc` ICE root-caused to `cparm_round` (clifpeep): it folded a
  join param to ONE `iconst` in the join block and rewrote mid-chain
  (self-passthrough) params in OTHER blocks to it. A param's uses
  live under its own home block, which the join need not dominate →
  non-dominating SSA. Symptom depended on which pass hit it first:
  egraph `payload < (1 << 30)` (Vec::<u8>::into_boxed_slice),
  verifier `uses value from non-dominating inst` (try_call edges),
  and the 13 fuzz `ifconv2 broke` ICEs the x64 agent flagged.
- Fix (3b20008): each carrier's replacement const is materialized at
  its OWN home block's first insertion point (dominates all its uses
  by SSA). Fuzz seeds 0:24 went 13-ICE -> 24/24 clean; new
  tests/cparm/main.rs covers the shape.
- rust-sh needs Hayden's rustc-side `fat_lto_supported` commit
  (cf54c772450) — applied locally as 4211e9357e1 and added to
  selfhost/setup.sh as rustc-session.patch for fresh worktrees.
- STILL OPEN: (a) stage1 std `alloc`+ rebuild in progress post-fix;
  (b) cparm also causes a matmul-64 2x scalar regression (0.16 vs
  0.08 — flagged by x64 agent, unrelated to dominance; investigate);
  (c) old-log Mach-O `__linkme*,regular,no_dead_strip` section-name
  ICE was from the STALE backend (Oct 8) — re-verify on the new
  build if it resurfaces.

## Self-host / celim retry-operand fix (52c3338) + punroll shared-exit fix (73907a8)

- Stage1 std build completed post-cparm-fix (core/alloc/std/test/
  proc_macro all clean). Stage2 progressed to compiler deps, then a
  pliron-compiled `blake3` build-script binary SIGSEGV'd inside
  `cc`'s env handling — `BTreeMap::IntoIter::dying_next` null-deref.
- Root cause (52c3338): `celim` normalizes an icmp with peeling; if
  that decide fails it retries unpeeled, but passed the FIRST
  normalization's raw operand pair. A `v223 == 0` query was decided
  against `v492`'s range (`v492-1 != 0` from `v492 != 0` — wrong),
  folding `depth-1 == 0` false so `dying_next` walked a dead link.
  Repro: BTreeMap<u32,u32> insert n>=12 then drop -> SIGSEGV
  (opt-level>=1). tests/celim/main.rs covers it.
- Next stage2 failure: `miniz_oxide` `deflate::compress_inner` —
  Cranelift egraph elaborate assert / verifier non-dominating use.
  Root cause (73907a8): `ifconv2` aliased a live-out value to
  in-body loads (`v6129 -> v758`); `punroll` then fed the shared
  exit continuation (a block dominated by no single exit, reached
  via both latch and side exits) from the peeled copies without the
  loads ever running — an ambient use outside every cloneable exit
  cone. `run_loop` never checked for that; `run_jam` already bails
  on the same shape ("body value used outside"). run_loop now bails
  too when a resolved body value is used in a reachable non-body
  block outside `cone ∪ mid_exit.corder`s.
- Not perf-validated: the bail only affects loops whose ambient
  live-outs leak past multi-exit merges (observed: none in test.sh;
  miniz_oxide compress_inner now skips the unroll). An LCSSA-style
  param-threading recovery is possible follow-up if a hot loop
  regresses.
- test.sh: green on 73907a8 (one prior `sleep+instant` 5ms flake,
  passes on rerun — timing-sensitive WASI test, not a regression).
- Stage2 compiler rebuild running on 73907a8 backend.

## Self-host / punroll second shared-continuation fix (f6c551d)

- After 73907a8, stage2 hit `register allocation: EntryLivein` on
  `rustc_lint_defs`; a new catch_unwind in lower.rs (4981375) named it
  `Lint::parse_rust_version` — three sequential `parse_part` loops
  whose exits converge on a shared continuation that ambiently uses
  the third loop's params (`v325` in `block138`).
- Root cause: the 73907a8 guard asked only that each ambient use sit
  inside *some* exit's cone. `block138` is inside the latch cone, but
  the peeled *side-exit* clone (`block181`) jumps to the original
  shared `block136` — reached without the defining inst running.
  The cone clone binds values only for the latch path.
- Fix: per exit target, forward-walk non-body blocks; any ambient
  body-value use must be dominated by that target (i.e. inside its
  own cloneable cone) — otherwise bail. Both repros now clean;
  tests/punroll/main.rs is the regression (ICE'd on 73907a8).
- Over-bail check: no new bails across test.sh corpus.
- Not perf-validated beyond test.sh; stage2 rebuild running on
  f6c551d+4981375.

## wasm: inline-site frame folding (wt/wasm-perf, 4402b11 — merged)

Root cause pursued: spliced callees re-ran the full SP frame dance
(`global.get sp; sub; and; set` + restores) at every callsite — a hot
`FindMatches::count` loop paid ~10 extra ops/iteration.

Fix: `inline_site` now folds callee frame chains into the caller's
reservation — callee fp aliases `caller_fp + K_i` (LLVM-style merged
inline allocas); the caller's `SZ` is bumped and its align mask widened
when needed. Frameless callers get a fresh prologue-shaped reservation
plus `global.set sp` restores on Return blocks that lacked one.

Bugs found via FunctionBody::validate() instrumentation (since removed):
- Emitted base `i32add`s were swept into `after` when the callsite sat
  in `entry` before the insert slot (split at `pos+1` takes everything
  after the call) → "def does not dominate" → misaligned frames →
  `unreachable` trap in regex `Builder::configure`.
- `insts.get(gpos+1)` captured *after* `add_op` appended the new consts,
  so bases anchored before the just-emitted const → same-block
  def-after-use.
Fixes: emitted ops insert before *anchor insts* (not absolute indices),
clamped to the call index when `blk == entry`; the anchored layout is
used only when there is room after `a`'s def (else falls back to a fresh
reservation); `orig` for restores picks the earliest SP read in final
entry order. Post-splice `validate()` now reports 0 failures on wbig
(was 70 with the broken version).

Measured (wbig, regex+std real crate, node):
- runtime: 6.60 → 6.52 ms/iter work(20) (~1.3%, consistent over 5 runs);
  stock 5.12 — remaining gap ~27%.
- size: 2,427,849 → 2,424,804 (stock 1,328,665 — still ~1.83x).
- SP ops (global.get/set 0 in disasm): 4,656 → 3,850 (−17%; stock 2,327).
- functions: 2,872 → 2,871 (stock 1,528).
- verify hash -9077338054421087601n identical to stock; work outputs
  byte-identical.
Validation: bash tests/wasm/run.sh fully green (core/std/WASI/unwind).
Not validated: V8/d8 hot-profile re-run; other wasm crates.
Next frontier: function count (~1.9x) and code density in branch-heavy
regex paths — local.set/if-else/i64-packing traffic per earlier profile.

## Stage2 self-host reached (2026-10-10)

The full stage2 compiler (incl. rustc_codegen_pliron itself, compiled by
the pliron-built stage1 rustc) now builds. Optimizer bugs fixed along
the way, in hit order:

- 52c3338 celim: unpeeled-retry range query used the peeled call's
  operand pair — `x-1 == 0` folded from `x != 0`; SIGSEGV'd BTreeMap
  drop walks (blake3 build script). Test: tests/celim.
- 73907a8 + f6c551d punroll: peeled exits feeding shared continuations
  created ambient (non-dominating) uses of body values — egraph
  elaborate assert / EntryLivein regalloc failure (miniz_oxide,
  rustc_lint_defs). Test: tests/punroll.
- 17ddc11 coldargs: `iconst.i128` is unrepresentable — iconcat halves
  instead (jiff). Test: tests/coldargs.
- 75cb4c6 bittab: stale analysis snapshot across folds + block-param
  uses unaccounted → dangling values / reserved_value OOB
  (rustc_trait_selection).
- b134d48 fusechains + merge_chains: edge args resolving to the
  target's own params wrote self-alias cycles (release builds skip
  change_to_alias's debug_assert) — `Value alias loop detected`
  (rustc_mir_transform).
- ffe20b3 loopvec: `band x,1` mod-2 bool arm + cond-tree walk admitted
  operands that were neither lane-replicable nor splattable →
  emit_scalar hit an in-body load ("side-effecting inst used as scalar
  invariant", vendored cranelift-codegen dfg.rs). Unit regression test
  in src/loopvec.rs builds the shape with FunctionBuilder.
- 4981375 lower: catch_unwind around define_function names the
  function + dumps CLIF on backend panics (kept — it made every ICE
  above reproducible in minutes).

## Stage2 self-host miscompiles (6239c9b, fe3fd90, e8d0fbf)

Post-"stage2 builds" the pliron-built stage2 rustc miscompiled three
successive passes; each blocked stage2 std until fixed. All diagnosed
via `PLIRON_CLIF` per-pass dumps on `rustc_*` crate rebuilds plus
disassembly of the stage2 driver dylib; each got a FunctionBuilder
unit test in the offending pass before the fix.

- 6239c9b bittab: the sparse-set fold emitted `icmp ne (mask >> d), 0`
  — nonzero whenever ANY bit >= d is set, so the whole interval
  `[lo, max]` matched, not just the sparse set. Contiguous ranges hid
  it; `sym == 12` slipped through {21,28,35} in `uninterpolate`, and
  stage2 rustc couldn't parse `pub const fn`. Fix: `band (mask>>d), 1`.
  Test: tests/bittab (fails pre-fix: every x <= max "hits").
  Bootstrap hazard: rebuild used `PLIRON_BITTAB=0` so the loaded buggy
  dylib couldn't poison the new one mid-build.
- fe3fd90 ifconv: `conv_table` deduped `br_table` arms on the RAW
  final-edge args — identical when arms share a last forwarder hop —
  collapsing arms that inject different constants earlier in the chain
  to the representative's value. `param_default_policy` returned
  `Some(Forbidden)` for every `ItemKind` member; stage2 rustc rejected
  generic param defaults on `const trait` (and everything else).
  Fix: resolve final args through `psub` before dedup.
  Test: `ifconv::tests::table_arms_dedup_on_resolved_args` + tests/polsw
  e2e. Rebuild used `PLIRON_IFCONV=0` for the same bootstrap hazard.
- e8d0fbf ifconv: `edge_ok` accepted escaped uses of a bypassed
  forwarder-hop's own defs whenever the hop dominated `b`'s term —
  exactly true when `b` is a loop latch inside the loop whose header
  merge is the hop. The use stays dominated, but the hop param is a
  phi over the hop's edges: dropping `b`'s edge changes the value the
  use observes. `rustc_lexer::eat_hexadecimal_digits` (in the ifconv2
  round): the digit latch's `has=1` edge was bypassed, `has` became a
  phi over only the entry `0` edge — 3030 `E0768 no valid digits`
  errors on valid hex literals in stage2 std.
  Fix: `edge_ok` now takes the union of all arm `chain` blocks; defs
  inside a bypassed chain never qualify on the dominance clause (both
  brif and br_table paths). Test:
  `ifconv::tests::arm_chain_local_escape_blocks_conversion` mirrors the
  sec6 CFG (dominated latch + escaped hop param/result) and asserts the
  fold is rejected; hop's own forwarder fold still fires.
- Perf note: none of the three were perf-measured (correctness fixes);
  the e8d0fbf rejection additionally skips chained ifconv folds whose
  hop locals leak below the merge — a real (unquantified) missed-opt
  class for flag-carrying loop headers.
- 534d57c celim: `off_implied` folded the *weak* reverse compare on
  ±1-offset facts — `x < y` does imply `x + 1 <= y`, so `x + 1 > y` is
  dead, but `x + 1 >= y` is SATISFIABLE at `x = y - 1`; folding it false
  dropped bounds-check exits. In `str::trim_start_matches("0")` on a
  1-byte string (the demangler's `0usize` suffix strip), the
  `ceil_char_boundary` `i >= len` exit was folded away →
  out-of-bounds panic. Recursive panic while demangling a backtrace
  deadlocked stage2 rustc at 0% CPU on any ui test that ICE'd under
  RUST_BACKTRACE — the first stage2 `tests/ui` run stalled mid-suite.
  Fix: only the strict complement decides false; the equality boundary
  stays satisfiable (unsigned lt/gt arms + signed mirrors).
  Test: `tests/celim` boundary pair `x = y - 1` + trim_start_matches
  cases (fails pre-fix, 0-vs-3 assert). Not perf-measured.
- 89f2fe7 selfhost/verify.sh: repeatable stage1-vs-stage2 compiletest
  diff harness — the "run this regularly" process. Handles the Darwin
  rust-objcopy PATH gap, --force-rerun against fingerprint caching, and
  a watchdog for idle-deadlocked rustc (the demangle symptom above).
- 0f99954 bcheck: a foldable bound check's unsafe edge only had to be
  `cold` — but cold *normal* exits are cold too. In `parse_cfg`'s
  `s.contains('=')` the `position` scan's not-found exit was cold (the
  surrounding error-reporting fold is a cold call region), so the loop's
  own `iv < bound` control test was collected as a "bounds check" and
  its `brif` became an unconditional `jump` in the fast clone — the
  scan's exit vanished and it read OOB until it found the byte. The
  `hi < len` guard is vacuous for `idx = i` (`hi = bound-1`), so the
  broken clone was always taken: every string "contained" '='. Found
  via the stage1-vs-stage2 ui diff (cfg-arg-invalid-2, path-kw — the
  "ensure escaping" long diagnostic). Fix: `dead_end` requires every
  path out of the unsafe edge to stay cold and never reach `return`.
  Test: tests/bcheck (needle placed just past len inside Vec capacity;
  fails pre-fix `true` vs `false`). Not perf-measured; the gate only
  tightens check collection.
- slotmerge + lifetime markers (uncommitted): stage2 SIGBUS cluster
  (deep parser/typeck recursion — expr-stack-overflow, match-72933,
  survive-peano, issue-88747, deep-try-153583) root-caused to giant
  frames, not stacker: every `alloca` became its own `ExplicitSlot` and
  Cranelift lays slots out sequentially with no sharing, so
  `parse_item_kind` paid ~18.3 KB/frame vs LLVM's ~896 B (~20 arms x
  136 B aggregates). ~3400 recursion levels x ~26 KB/cycle overflows
  any thread stack. `src/slotmerge.rs` does CLIF-level stack-slot
  coloring: traces each `stack_addr` through pure ops/block params/
  call args, builds a live interval over instruction positions in
  layout order, and greedily best-fit colors disjoint intervals onto
  one representative (absorbed slots shrink to 0). Escape model:
  stores to non-frame memory/indirect calls/writing callees/
  other-side-effect insts extend the interval to function end;
  `stack_addr` itself, `return`ed pointers, `sret` args, write-free
  callees (both `nowrite` flavors for plain calls — unwind kills our
  frame — strict-only for `try_call`, whose landing pads stay
  in-frame), and libc memset count as point uses. Store-of-pointer
  into another slot is NOT an escape: a container edge forces the
  stored slot's interval to contain the container's — a fixpoint with
  memcpy/memmove copy edges (contained pointers flow dst-ward) — so
  escapes through in-frame structures stay bounded by their
  containers. Loads whose results can't hold a scalar pointer
  (vector/float) don't propagate slot provenance — chunked i8x16
  aggregate copies no longer pin every copied slot; scalar-int loads
  still propagate (loaded bytes may be a pointer). Atomic arg orders
  audited: store/atomic_store (val, addr); atomic_rmw (addr, val);
  atomic_cas (addr, expected, new). Backward CFG edges smear the span.
  `llvm.lifetime.start/end` (MIR StorageLive/Dead) are emitted by
  builder.rs, recorded per-CLIF-block in lower.rs (resolved after
  lowering — an inlined alloca can postdate its marker), ignored by
  sroa.rs (not a memory access), and no-op'd in wasm.rs. IMPORTANT:
  markers contribute use positions ONLY — exempting marked slots from
  escape/back-edge extension (LLVM's model: dead storage can't be
  dereferenced) MISCOMPILED stage2 (`lower_to_hir` read a corrupted
  DefIndex; rustc MIR permits raw-pointer access after StorageDead).
  Kept behind PLIRON_SLOTMERGE_MARK for experiments; off by default.
  Synthetic check (tests/slotmerge/big.rs, 20 arms x 2 disjoint
  136-B aggregates): 40 slots -> 2 regions, frame 9.5 KB -> 272 B
  (LLVM: 304 B), `slotmerge 38`. Stage2 `parse_item_kind`:
  18.3 KB -> 9.1 KB (LLVM 896 B — remaining gap is real call-arg
  escapes into non-nowrite callees + stores through the caller's
  out-param, which need marker-authoritative bounds to shrink
  further). Cluster: 1/5 pass under stage2 (match-72933); the deep
  ~3400-level cases still SIGBUS at ~9 KB/frame — documented gap,
  needs the contents-tracking or SROA-coverage follow-up.
  `check_expr_kind` 19.2 -> 11.3 KB, `check_argument_types`
  7.0 -> 3.6 KB. Tests: tests/slotmerge/main.rs runtime check
  (disjoint, overlapping, escaped, edge-carried) + big.rs frame-size
  assert (<1024 B) in test.sh. Not perf-measured (correctness/
  frame-size fix); container edges added after the last local run —
  no measurable change on parse_item_kind (its stores go through the
  caller's sret out-param, not local slots).
## wasm: inline small-const byte equality (6a4d672)

cpu-prof (node --cpu-prof, 200 iters) put `memcmp` at 77ms/1335ms — the
hot sites were constant n=32 compares in the regex-automata DFA
Lazy-cache/state-set path, all libcalls. LLVM expands small-const
memcmp inline; we didn't.

src/intrinsic.rs: `raw_eq` (const type size) and `compare_bytes`
(const n arg) <=32B now emit an or-reduced xor over 8/4/2/1-byte word
chunks (narrow chunks zext to the accumulator width — wasm validate
caught the mix). compare_bytes keeps exact ordering by branching to
the libcall only when the inline scan differs (catch_unwind slot
pattern for the merge).

wbig: 6.5 -> 6.07 ms/iter work(20) (~7%); stock 5.08 — gap 1.28x ->
~1.19x. Size 2,427,742 -> 2,425,383. verify hash identical; memcmp
falls off the profile. tests/wasm run.sh green incl. new
eq_bytes_check (all chunk widths, non-equal slow path, n>32).
REJECTED: mid-size (<=64-inst) <=6-callsite inlining in winline —
~1% runtime for +2% size; wasm calls are cheap. Comment left in situ.
Remaining hot gaps (200-iter profile, pl vs st): find_fwd_imp +79ms,
ReverseInner::search_half +31, hybrid_try_search_half_fwd +27 /_rev
+11, aho PrefilterI::find +36.

## nocapture: callee-body capture inference feeding slotmerge

`DeducedParamAttrs::captures_none` only applies to `PassMode::Indirect`
params — rustc's MIR analysis ignores `*_k` uses and `move`/`copy` of the
param, which is unsound to reuse for `&`/`&mut` (Direct) params (a callee
that returns the pointer reports `captures_none`; the rustix build-script
panic proved it empirically). But the dominant SROA/slotmerge blockers are
exactly those Direct pointer args (~75 call escapes ≈ 3KB of live frame in
`parse_item_kind`, plus `llvm.icmp`/`llvm.br` pointer uses).

src/nocapture.rs: LLVM `captures(none)`-style bottom-up inference over the
pliron bodies of the CGU. Per-param derivation is tracked in two bands —
`exact` (pointer-typed values) and `fuzzy` (ints that could encode one via
`ptrtoint`) — propagated through results, block params, and invoke
exception payloads to a fixpoint; comparisons produce neither so
`p == q`-returning predicates stay non-capturing. Any store of a derived
value counts as capture (even into the callee's own frame), so no alias
tracking is needed; memcpy/memmove capture the src only; unknown
intrinsics/asm/returns capture everything. Optimistic fixpoint like
nowrite.rs handles recursion; cross-CGU and indirect callees stay
conservative.

builder.rs computes `CallInfo.nocap` from `DeducedParamAttrs` restricted
to `PassMode::Indirect` (rustc's own soundness domain — also correct
cross-crate); lower.rs unions it with `st.nocapture` per CLIF arg
(`csrc` maps each lowered arg back to its pliron operand through ABI
expansion, sret, and the vararg rewrites); slotmerge treats flagged args
as bounded uses.

Verified on synthetics: `p == q` predicates and load/store helpers stay
non-capturing, `return p`/`store p` capture. Numbers (rustc_parse
stage2): +123 merges, −1.6KB total frame; `parse_item_kind` unchanged at
9.0KB — its remaining escapes are cross-CGU `parse_item_*` callees and
stores into the caller's sret out-param, both out of scope for this
analysis. LLVM's 896B is mostly SROA coverage, a separate problem.
Not perf-measured; small win by construction, correctness-validated by
./test.sh and stage2 rebuilds.

Also in this change: lifetime markers record the instruction anchored
just before them (`ipos[anchor]+1` start / `ipos[anchor]` end, block-start
and drop fallbacks) instead of conservative whole-block ranges;
`PLIRON_SROA_WHYFN=<substr>` prints per-alloca promotion blockers; the
`why` diagnostic skips lifetime markers; pliron printer switched the
optional-in-practice attrs (`llvm_nneg_flag`, `llvm_fast_math_flags`,
`llvm_fcmp_predicate`) to `opt_attr` so `PLIRON_DUMP_OPT` no longer ICEs.

## taildup const-edge clones + late jtround (79e728f)
- chars_alpha 2.29x -> ~1.85x: `Option<char>` decode flag and the
  `is_alphabetic` bool no longer materialize through a stack-slot phi;
  `add n,#1` sits directly on the a-z/A-Z range-check edges.
- taildup: brif preds get one clone block per edge to a candidate, but
  ONLY when the edge passes a const arg — without that guard it cloned
  the range-check chain into itself (dispatch explosion, regression).
  A second `taildup2` step runs after splink because the flag-merge
  brif shape only settles post-edgespec/celim (early taildup sees the
  same blocks with `jump` terminators).
- `jtround` late step: single jumpthread::round (no fixpoint) after
  sameargs/celim turn edge args const — threads `brif flag` merges the
  early fixpoint couldn't decide. NonNull::is now guards `derived`
  targets with value_is_real (deleted values are dangling aliases;
  resolve_aliases on one panics — latent crash, first hit by jtround).
- New bench kernels recreating the deleted wt/native-opt suite:
  chars_alpha/ascii/mixed, tokenize, contains_rng. Ratios vs stock:
  chars_alpha ~1.85x, chars_ascii ~1.1x, chars_mixed ~1.05x,
  tokenize ~1.9x (state-machine residual — in progress), contains_rng
  ~0.95x.
- Residual on chars_alpha (~16 insts/char vs stock ~11): cbz null-check
  (cursor nonnull unproven — INBOUNDSNN territory), uxtb+cmp#0x80 vs
  stock ldrsb+tbnz sign dispatch, per-edge param movs (post-index
  amode territory), a-z|A-Z as two range checks vs stock case-fold.

## jtround merge-block threading (aabbeb8, subagent work)

- jumpthread::round threads `brif` merge blocks even when the bypassed
  block's values are used downstream of the thread target: exports
  become new params on `s`, repaired on `s`'s other preds. Post-edit
  dominance is re-checked against the planned-edge graph (reach_avoid)
  because a new edge can open a def-bypassing path (print_num
  verifier failure was exactly this); const_env remat covers values
  that lose dominance.
- brif_not + brif_argspec (clifpeep): `brif` on a select/icmp chain
  folds to the raw flag, and edge args built from the branch's own
  decided condition fold per edge — kills the tokenize discriminant
  re-materialization.
- `mergechain` late step (merge_chains + edgespec + brif_fold) now
  default-on; tokenize 11.2ms -> 7.7ms vs llvm 5.98 (1.9x -> 1.28x).
- Also in the batch: isub telescoping in lcse (PLIRON_SUBSUB), bittab
  brif-on-band direct (tbnz, PLIRON_BITBRIF), jtround loops to
  quiescence (8 max), PLIRON_EDGESPEC_DEBUG cone tracing.
- chars_alpha ~4.8ms stable (llvm 2.09, 2.3x) — the subagent batch is
  neutral there; residual is still isel-level (post-index ldr, sign-bit
  dispatch, case-fold trick).
- sieve 13.5ms vs llvm 12.1 (1.12x): u128work is verbose (i128 mul via
  cset/uxtb carry chains, not adds/adc) but only 1K iters — the delta
  is the marking loop / noise, deprioritized.

## rangefold (uncommitted, in progress)

- New pass `rangefold`: branch-chained `[lo,hi)` interval tests become
  `brif (x-lo u< hi-lo)` (bound_pairs), and same-width ranges a pow2
  apart union to `(x|d)-k_hi u<w` with an exactness check on the bit-d
  mod-2d windows (LLVM's `x|0x20` case-fold for a-z|A-Z).
- chars_alpha MIXED input regressed ~60% with union on (7.5ms vs 4.8)
  though union is faster on all-miss (3.47 vs 5.74) — bypassing the
  hi-test block hoists the HIT-edge arg store (mov;stur flag) into the
  unconditional pred. Made range_unions OPT-IN (PLIRON_RF2=1);
  bound_pairs default-on (PLIRON_RF1=0 disables, debug PLIRON_RF_DEBUG).
- tests/rangefold exercises exact/inexact unions + boundaries; wired
  into test.sh as `want rangefold`.
- selfold (icmp-of-select niche fold) removed: inert on all 10 benches
  (byte-identical codegen) — the provable-collapse case needs nonnull
  facts the lattice doesn't produce for these shapes.

## aarch64 post-index isel + clifpeep sgntest (subagent work, uncommitted)

- `try_postindex` (lower.rs): `vN = iadd/isub base, C` fused with the
  most recent load/store on `base` when `base` has no observable use
  after the writeback — `MInst::LoadPost`/`StorePost` +
  `AMode::PostIndex`, full operand/emit plumbing. `PLIRON_POSTIDX=0`.
  Emits real `ldrb [x], #k!`/`ldr [x], #k!` (42 sites in tokenize).
- `clifpeep::sgntest`: `brif (icmp cc x, K)` where the compare is a
  one-bit test (`ult byte,0x80`, `slt x,0`) → `brif (band v, 1<<b)` →
  aarch64 `tbz/tbnz` (`tbnz w22,#0x7` fires on tokenize's UTF-8 lead
  byte). `PLIRON_SGNTEST`, late step.
- tokenize main: 608 → 469 insts, wall-time flat (~7.7ms vs llvm 5.8):
  the residual is regalloc/param-shuffle, not isel breadth.

## dse build-time pass over PLIRON_DSE_PROF log (618f8fd, on main)

- Self-host build log: ~105s CPU in `dse` across >111k calls (log was
  still growing): fix 49s / iso 19s / ps 19s / dec 11s / sweep 7s.
  ~95% of calls end dead=0.
- `dse::run` signature changed: `(func, noalias, nw, cfrozen, an, step,
  fname)` — draws cfg/alias-map/isolated from the ver-keyed `Analysis`
  cache (`dse_inputs`) instead of rebuilding per run. `iso` is keyed on
  `noalias.len()` too (`carry_aliases` grows the set inside a ver; the
  set never shrinks).
- `Analysis::materialize` is cfg-only now; `dt`/`la` are lazy in
  `cfg_dt`/`cfg_dt_la` — `an.cfg()` no longer warms them.
- `amap`/`iso` are reusable by other passes: licm/memspec/indvars/slp/
  celim all recompute `param_src`/`alias_map` today. NOT shareable:
  loadfwd/memspec use `isolated_escapes` with the real `nocap` set —
  dse's cached `iso` is the empty-nocap variant; merging is unsound.
- Early-outs: no plain-notrap-store → return before setup; decode finds
  zero killable stores → return before fixpoint.
- Kill sweep now only walks `store_blks` (blocks holding a killable
  store), not every reachable block.
- Pop budget 32x -> 6x blocks (tail above ~6x is merge churn with ~0
  kills in the sampled log). `PLIRON_DSE_DEBUG` now prints
  "over pop budget … (fname)" — raise `MAX_POP_ROUNDS` if a missed kill
  on a giant fn ever matters.
- `dseprof` line format changed: `dseprof <step> <fname> blocks=…` —
  parsers keyed on `dseprof \S+ blocks=` need updating. Cost can now be
  attributed per dse/dse2/dse3 and per symbol.
- Verify: `cargo test -p pliron-clif-opt` green, `cargo check -p
  rustc_codegen_pliron` clean. NOT yet run through test.sh/harness —
  worth a std diff-clean pass before relying on it (budget cut is a
  codegen-visible change).
- Still open: `Demands` repr (FxHashMap<Root,Ranges> cloned per pop) is
  the biggest remaining `fix` lever; re-profile first.

## Build-dir contention (process)

- `cargo test -p pliron-clif-opt` blocked on "file lock on build
  directory" while the selfhost build held `target/`. Workaround that
  worked: `CARGO_TARGET_DIR=/tmp/pltd_ttarget cargo test -p
  pliron-clif-opt` — the opt crates are stable Rust with no dylib link,
  so a scratch target dir is cheap and parallel-safe.
- Suggested convention: ONE agent owns the shared `target/` (full
  backend builds, test.sh, harness); everyone else uses a scratch
  `CARGO_TARGET_DIR` for `cargo test/check -p pliron-*-opt`. Scratch
  deps cost ~2min once, then incremental is fast. This avoids both lock
  stalls and target-dir cache thrash.

## guardelim demand-driven fixpoint (Devin 2, 2026-10-15, commit 9118201)

- Bench microbench compile time (`cargo rustc --bench kernels`, scratch
  `/tmp/cgbuild/target`, PLIRON_PAR=0): guardelim was 14.9s of 21.4s
  iropt; now 0.2s. Bench wall 34.7s -> 17.7s.
- Prescan: functions with no foldable cond_br (provable icmp pred +
  unreachable-terminated succ) skip domtree+scan+fixpoint (87% of bench
  fns). Dom::new moved after the prescan.
- `relevant` intersected with the needed-slot closure (slots reachable
  from candidate icmp operands through the atom source graph;
  over-approximates a Slot's reaching set with all stores to the slot).
  Untracked slots -> opaque V atoms = weaker proofs only, never unsound.
  Fold count on bench: 48 vs 49 before (one lost through an untracked
  slot) — acceptable; revisit if a suite regression traces here.
- `PLIRON_GE_PROF=1` prints `geprof <cgu> blocks= cands= rel= fix= prove=
  folds=` per fn.
- Remaining bench codegen (~11s): CLIF pipeline 9.7s (jumpthread 2.1s,
  slp 1.5s, ifconv2 0.43, loadfwd 0.36, mergechain 0.35, unroll 0.31,
  sameargs* 0.63, edgespec 0.30, compile 0.78) + iropt 5.6s
  (inline+scan ~2.6s, sroa 0.67, callgvn* 0.68, nocap* 0.58,
  phisimp 0.30) + fnlower 0.85s.
- Gotcha reconfirmed: cargo does not rebuild on PLIRON_* flips —
  `touch` a bench source or expect 0.1s "Finished" no-ops.

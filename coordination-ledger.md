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

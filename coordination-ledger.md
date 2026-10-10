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

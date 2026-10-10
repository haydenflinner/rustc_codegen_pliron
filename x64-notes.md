# x86-64 codegen notes (wt/x64-codegen)

Running log of work + learnings. Bench: `/tmp/cpubench/{main,wide}.rs`,
`rustc +nightly-2026-10-06 -O --target x86_64-apple-darwin -Ctarget-cpu=x86-64-v3
--edition 2021 [-Zcodegen-backend=...dylib]`, run via `arch -x86_64` (Rosetta).

## Session 1: target-cpu → Cranelift ISA flags

- Cargo.toml: cranelift `host-arch` → `all-arch` (required to emit x86_64).
- **`-Ctarget-cpu` was a no-op for x86**: `cpu_features()` in src/lib.rs only
  knew Apple cpus → `internal_target_features` got no sse4.1/avx2 →
  `has_sse41` etc. never enabled → every SSE4.1 lowering rule fell back to
  SSE2 sequences (`umax.i32x4` = pxor-bias + pcmpgtd + pand/pandn/por +
  constant reload *inside the loop*; `imul.i32x4` = pmuludq dance).
  Added an x86 cpu table: x86-64-v2/v3/v4 + mainstream Intel/AMD names.
  `internal_target_features` expands implied features, so entry points
  suffice (avx2 pulls the whole sse chain).
- **Rosetta finding: VEX-128 xmm encodings are ~2x slower than legacy SSE**
  in streaming loops (verified with a faithful micro-repro: 104ms vs 54ms).
  Integer VEX (BMI1/2: mulx/shlx) is fine. ymm VEX is fast too (stock LLVM
  uses it) — but vendored Cranelift x64 has *no >128-bit vector register
  class* (`type_register_class` returns None over 128 bits), so has_avx*
  only bought VEX-128 encodings. `build_isa` now maps only
  sse3/ssse3/sse4.1/sse4.2/popcnt/bmi1/bmi2/lzcnt/cmpxchg16b for x86_64;
  `PLIRON_X64_VEX=1` re-enables the VEX families for real-hardware runs.
- Result (Rosetta, v3 flags): everything in wide.rs/main.rs at parity or
  faster than stock LLVM -O:
  max_u 0.193 vs 0.233, min_i 0.195 vs 0.237, dot_i32 0.095 vs 0.132,
  sum_u8 0.388 vs 0.702, dot_i8 0.320 vs 0.447, axpy 0.384 vs 0.581,
  clamp8 0.092 vs 0.144, plus prior wins held (find_off 14x, has_val 4.4x,
  dot_u8 3.1x, even_sum 2.7x, cnt_vowel 2.3x, rev_copy32 1.7x).
  Residual gaps: sum2d ~5% slow (ordered fadd reduction — extractlane fold
  per group + serial scalar acc; LLVM splits vector accs and folds with
  shuffle+add tree), fillzero/xor_fold parity, matmul ~parity (LLVM sees
  n=256 constant; black_box blocks it for us).
- 256-bit vectors: not feasible without writing ymm register-class +
  lowering support in vendored Cranelift (upstream never did it; only
  aarch64 has real vector-legalization for >128). loopvec stays 128-bit;
  UNROLL=4 gives 512 bits/iter anyway.

### Mach-O x64 fixes (backend already ran on aarch64; these unblock x86_64-apple-darwin)

- `va_tramp_asm`/`va_tramp_ind_asm` (src/lower.rs) emitted ELF-only stubs:
  `.section .text.<sym>,"ax",@progbits` + `.type`/`.size`/`.hidden`. On
  Mach-O rsasm parsed `.text.__pliron_va8.snprintf` as a segment name →
  >16 chars → "a Mach-O segment name is at most 16 characters" panic in
  objmerge. Now both take a `macho` flag (from `isa().triple().binary_format`)
  and emit `__TEXT,__text` + `.private_extern` for Mach-O, ELF path
  unchanged. Verified: `snprintf = 42|2.500000|A|-7` through the `%al=8`
  trampoline under Rosetta.
- `llvm_intrinsic_stub` MachO branch (src/asm.rs) dropped the
  `.intel_syntax noprefix`/`.att_syntax` wrappers the ELF branch adds, so
  every Intel-syntax x86 stub body (xgetbv/pclmulqdq/vzeroupper/ud2/…)
  mis-assembled ("no form of `movq` accepts a memory operand"). Now emits
  `{syntax}{body}{back}` on both paths. Verified: `pclmulqdq ok` under
  Rosetta.

x64 smoke status (Rosetta): std/unwind/unroll/licm/asm all compile+run;
std & unroll outputs identical to expectations (stock x64 can't link
tests/asm — the test's global_asm is ELF-styled, no Mach-O underscore).

### Scalar partial unrolling (src/punroll.rs, `PLIRON_PUNROLL`, default on)

- sum2d was the last wide.rs regression: LLVM doesn't vectorize the
  ordered fadd reduction but scalar-unrolls the inner loop 8x. Existing
  `unroll.rs` only handles constant-trip full unrolls.
- `punroll::run` (after loop rotation, before lowering) detects a
  latch-tested linear counted loop (`a cc bound`, single-entry chain of
  jump/brif blocks, body <= 24 insts), then emits a guarded K-copy chain:
  `hu` checks `bound - a0` covers the group (widened, no wrap), then K
  clones run; only the last copy re-tests the count and exits, so all
  intermediate entry tests are implied. Scalar remainder keeps the
  original loop (tail iterations + re-entry on non-multiple trips).
- SSA: the exit block's dominance cone may use loop values directly
  (e.g. `return acc`). If so the whole cone is cloned (<= 16 blocks /
  128 insts, no try_call/br_table) and the unrolled exit targets the
  clone; cone-internal edges retarget via a block map. Verified with
  `PLIRON_VERIFY=1`; earlier verifier error "uses value v76 from
  non-dominating inst63" was this exact pattern.
- Result (Rosetta, v3): sum2d 0.79 vs stock 0.75 — parity; the serial
  addss dependency chain is the bound, unrolling only removes the
  per-iteration loop overhead. asm confirms 8 consecutive addss in the
  fast path (K=8 for bodies <= 16 insts).
- Host `./test.sh` passes; x86 target std/unwind/asm/unroll/licm all
  run correctly under Rosetta.

### 128-bit lowering audit (post ISA-flag work, x86-64-v3 dumps)

All three flagged kernels already emit 4 independent `i32x4`-lane
accumulator chains per iteration (loopvec UNROLL=4) — the "two
independent 128-bit accumulator chains" stretch goal is effectively
already there.

- min/max (`max_u`, `min_i`): in-loop `umax.i32x4`/`smin.i32x4` →
  single `pmaxud`/`pminsd` per group (SSE4.1 now mapped). Epilogue is
  `extractlane`×4 + 3 scalar ops per accumulator instead of LLVM's
  psrldq+pminud shuffle tree — but it runs once per call (~30 uops vs
  ~2.5M in the loop), so it cannot explain any measurable gap. A
  shuffle-tree epilogue would be a loopvec.rs change; it IS expressible
  in CLIF today without ISLE work (`shuffle` uimm128 masks already lower
  to `pshufd`/`palignr` via the rules at lower.isle:4632+), but it's not
  worth the churn — ~30 uops once per call.
- `dot_i32` (indexing `a[i]*b[i]` form): `imul.i32x4` + `iadd.i32x4` →
  `pmulld`+`paddd` — optimal for i32 lanes (pmaddwd only applies to
  i16). Note: the `.iter().zip(b)` formulation does NOT vectorize
  (bounds-check shape hides the induction) — only the indexing form.
- `sum_u8`: **fixed** — see `psadbw` section below.

### `psadbw` byte-sum fold (vendored assembler + new CLIF op)

- `sum_u8` (`a[i] as u64` widening sum) emitted a 3-level `uwiden` tree
  per u8x16 (6 unpacks + 2 iadd_pairwise + 2 iadd ≈ 10 vec ops) where
  LLVM emits `psadbw x, zero` + `paddq`. The widen tree's per-byte lane
  mapping doesn't align with psadbw's group sums, so no ISLE-only fold
  was possible — the epilogue sums all lanes anyway, so any lane
  grouping of the accumulator is legal.
- Vendored `cranelift-assembler-x64` and `cranelift-assembler-x64-meta`
  (0.135.5, path deps via `[patch.crates-io]`; meta is the assembler's
  build-dep table crate — the actual encoding addition is one
  `inst("psadbw", ...)`/`inst("vpsadbw", ...)` pair in
  `instructions/avg.rs`). `cranelift-codegen-meta`'s assembler.isle
  generator then emits `x64_psadbw_a_or_avx` for free.
- New CLIF op `x86_psadbw` (i8x16,i8x16 → i64x2, real |a−b| group-sum
  semantics) in codegen-meta `shared/instructions.rs`; `x64_psadbw`
  decl in `isa/x64/inst.isle`; `lower.isle` rules lower it and fold a
  `splat(iconst 0)` operand to `xmm_zero`.
- loopvec: `widen_vec_into` emits `iadd(acc, x86_psadbw(chunk, splat0))`
  for the unsigned u8→u64 unmasked-sum case, gated on a new `x64` flag
  threaded from `run` (aarch64 keeps the pairwise-widen tree — uaddlp
  folds already). Diff is ~12 lines plus signatures.
- Result (Rosetta, x86-64-v3): sum_u8 **0.135 ms vs stock 0.693 ms**
  (was 0.39 pre-fold; now ~5x stock). Disasm: one `psadbw xmm,zero` +
  `paddq` per 16-byte group, zero reg hoisted. Correctness checked
  n∈{0,1,7,…,4093,4096,1M} vs scalar reference; host ./test.sh and x86
  std/unwind/asm/unroll/licm all green under Rosetta.

### `scatter::hist` side-exit transfer study (checked histogram, Rosetta)

Investigated the remaining checked-histogram gap: the unrolled loop's
mid-chain `brif` side exits (index + bounds panics) transfer live values
(`i`, `a[i]`, `alen`) to shared cold panic blocks via edge args. RA2
splits each value's bundle at the hot/cold boundary — the cold segment
has fixed-reg requirements from the panic call — producing per-element
boundary movs incl. `movq %rdi,%r9` save/restore pairs around each
bounds check.

Four transfer mechanisms measured (same-address `vec![3;1<<20]` /
varied input, ms/iter, Rosetta):

| mechanism | same-addr | varied |
|---|---|---|
| stock LLVM | ~0.44 | ~0.34 |
| shared cone + edge params (committed) | ~0.47 | ~0.47 |
| per-copy cold trampolines binding params | ~0.47 | ~0.47 |
| stack-slot hand-off (store hot, load cold) | ~0.70 | ~0.44 |
| per-copy cone clones reading globals | ~1.02 | ~0.41 |

Findings:

- The boundary movs are **structurally unavoidable** with a shared cold
  target: any value live into a cold block whose use needs a fixed reg
  gets its bundle split in hot code — via edge args, explicit cold
  trampolines, or continue-edge rebinding (all equivalent).
- Eliminating the movs entirely requires LLVM's shape — each check's
  cold code reads globals directly — i.e. per-copy cone clones. That
  produced the *cleanest* hot asm (~10 insts/element vs ~12) yet ran
  ~2.2x **slower** on same-address input. Likely a Rosetta translation
  artifact of multiplied cold branch targets, not instruction count.
- Stack slots avoid the register split but the hot-path stores lose
  more on the store-bound loop than they save.
- Conclusion: the committed shared-param shape is locally optimal; the
  residual ~7% same-address gap vs LLVM is the boundary-mov cost, and
  the larger varied gap is worth more future effort (cold-cone
  rematerialization — reload `a[i]` inside the cone from already-live
  `aptr`+`i` — is the most promising remaining transfer reduction).
- Debug tooling added (uncommitted, env-gated):
  `PLIRON_PREVCODE=<pat>` dumps pre-regalloc VCode and
  `PLIRON_RA2_EDITS=1` dumps regalloc-inserted moves in
  vendor/cranelift-codegen/src/machinst/compile.rs.

### regex-syntax unconditional-branch audit + `edgefwd` pass (src/edgefwd.rs)

Task: explain ~9.7k `b` vs stock ~2.2k in regex-syntax aarch64 output.

Taxonomy of the 9,819 unconditional `b` (aarch64, `llvm-objdump -d`):

| category | pliron | stock |
|---|---|---|
| backward loop branches | 1,568 | 571 |
| diamond-merge jumps | 3,752 | 588 |
| after-cond forward jumps | 227 | 260 |
| jump-to-next-block (missed fallthrough) | 2 | 16 |
| other forward jumps | 4,270 | ~812 |

Blocks: ~22,751 vs ~10,418 stock; small (≤3-inst) `b`-terminated
trampolines ~3,929 vs ~704. So the gap is **block-count driven**, not
layout: missed fallthrough (2) and jump-to-jump chains (~0) are noise.

Root causes found:

1. MIR `bbN: { _x = const k; goto -> T }` blocks lower verbatim to
   `iconst; jump T(iconst, ...)` forwarders — the dominant residual after
   earlier passes (all remaining small tramps in `visit_post` were this
   or `call; jump`).
2. `try_call` normal/exception edges to forwarder and trap blocks — the
   earlier `jumpthread::bypass_forwarders` whitelists jump/brif/br_table
   preds, so `try_call` edges never got retargeted.
3. `call; jump T` forwarders can't be edge-forwarded (the call result
   lives in the block) — residual.
4. `fusechains` only merges single-pred targets; most tramp targets are
   multi-pred → doesn't reach this shape.
5. `bl; b shared_udf` machine-level pairs are mostly `try_call`
   normal-edge artifacts (~162 crate-wide), not ordinary jumps — the
   trap-tail inline only nets ~3 `b`.

What landed: `src/edgefwd.rs` (new, `PLIRON_EDGEFWD`, default on,
`PLIRON_BISECT=edgefwd`-able), run in lower.rs after fusechains, before
`define_function`. Three pieces:

- `bypass_round` — bypass_forwarders generalized to ALL predecessor
  terminators incl. `try_call`/`try_call_indirect`; non-Value edge args
  (TryCallRet/TryCallExn pseudo-values) forward verbatim on the same
  edge. Criteria otherwise identical (empty or pure locally-consumed
  body, `jump` tail, non-escaping params, args must dominate pred edge).
- `remat_round` — `iconst; jump T(iconst,...)` forwarders whose args
  don't dominate the preds: substitute a provably-equal value already
  valid on the edge (same-typed `iconst` among other targs / pargs /
  pred-block iconsts), else clone the ≤4-inst pure body into the pred
  before its terminator and retarget.
- `inline_trap_tails` — `jump trap_only_blk` → `trap` in place (minor).

Results (regex-syntax rlib, aarch64 disasm): `b` 9,819 → 9,147 (-6.8%),
total insts 105,033 → 105,540 (+0.5% — the cloned constants are
materializations LLVM emits per-edge anyway). Per-function: `visit_post`
(hir) 502→432, `Translator::visit_post` (ast) 630→587, `alternation`
376→356, `extract` 170→147, `translate` 214→201; a few funcs moved the
other way on block-order reshuffles (`Display::fmt` 193→221,
`concat` 248→259). Remaining small tramps in visit_post are all
`call; jump` (can't forward past a call). Still ~4x stock's `b` count —
the residual is dominated by diamond-merge/other forward jumps from the
~2x CLIF block count vs LLVM's CFG.

Correctness: `PLIRON_VERIFY=1` clean on the whole regex-syntax crate;
test.sh all green (nostd/std/unwind/asm/licm/unroll/proc-macro/wasm);
Rosetta x86_64-apple-darwin std+unwind pass; scatter kernel output
correct (hist 0.660 ms/iter vs stock 0.797). x64 regex-syntax compile
was initially blocked by a pre-existing `splat.i32x2` x64 lowering gap —
identical with `PLIRON_EDGEFWD=0`, unrelated; fixed by the sub-128 splat
rules documented below.

### x64 sub-128 vector splat lowering (`vendor/.../x64/lower.isle`)

x64 whole-crate builds failed in `Hir::alternation` with
`Compilation(Unsupported("should be implemented in ISLE: inst = v21083 =
splat.i32x2 v4529"))` — identical with `PLIRON_EDGEFWD=0`. Origin: pliron's
own SLP pass (`src/slp.rs`, `Pack::Splat`) packs two adjacent i32 stores of
one scalar into `splat.i32x2` + an 8-byte vector store (regex-syntax's
`extend_trusted`/`map_fold` `(u32,u32)` pair fill). x64 only had splat
rules for the six 128-bit vector types.

Fix: five wildcard rules at priority -1 keyed on
`(is_xmm_type (multi_lane <bits> _))` + the scalar source's `value_type`,
covering i8x{2,4,8}, i16x{2,4}, i32x2, f16x2, f32x2. Sub-128 vectors live
in xmm registers and consumers read only the low lanes, so broadcasting
across all 128 bits is correct — the rules reuse the same sequences as
the 128-bit splats (movd/bitcast + punpcklbw/pshuflw/pshufd/shufps). The
`is_xmm_type` guard keeps the wildcard lane-count from matching a >128-bit
type if Cranelift ever legalizes one. Priority -1 puts them below the
concrete 128-bit rules so nothing existing is shadowed.

Validation:
- minimal repro: `fn fill2(x:u32, out:&mut [u32;2]) { for i in 0..2 {
  out[i]=x; } }` — SLP emits `splat.i32x2` + `store.i32x2`; disasm
  `movd %edi,%xmm3; pshufd $0,%xmm3,%xmm5; movsd %xmm5,(%rsi)`; plus
  i8x8/i16x4/f32x2 variants (punpcklbw+pshuflw+pshufd / pshuflw+pshufd /
  shufps), all runtime-verified under Rosetta (`splat ok`).
- Full x86_64-apple-darwin regex-syntax rlib now compiles end-to-end
  (previously hard-failed); `PLIRON_VERIFY=1` clean.
- x64 disasm `jmp` count: pliron 10,534 vs stock 2,491 — same ~4x
  block-count-driven gap as aarch64 (9,147 vs 2,247).
- test.sh green; Rosetta std+unwind pass.

Rejected experiment (edgefwd): privatising `try_call` normal edges to
shared trap-only blocks — per-site `try_call f, ret=private_trap, [pad]`
with the private trap placed after the call — aimed at `callq; ud2`
fallthrough instead of `callq; jmp shared_ud2` (~1.3k sites crate-wide).
`Function::is_effectively_cold` marks every trap-terminated block cold,
so the private traps always sink to the cold section and the jmp stays:
+1,193 `ud2` for only -187 `jmp`. Reverted; kept as a code comment.

### try_call → trap continuation inline materialization (vendored emit)

`callq; jmp shared_ud2` after try_call (~850 direct + ~74 via edge-block
forwarders crate-wide in regex-syntax x64) could not be fixed at CLIF
level: `Function::is_effectively_cold` marks every trap-terminated block
cold so shared traps always sink and the jump never becomes fallthrough.
The CLIF-level private-trap variant was tried and rejected (prior entry).

Fix is in the vendored backend, at emit time:

- `TryCallInfo` gains `continuation_trap: Option<TrapCode>`
  (machinst/abi.rs).
- `try_call_info` (machinst/isle.rs) fills it when the exception-table
  `normal_return()` BlockCall targets a block whose only inst is a
  `trap` and whose edge carries no args. Detection is on the CLIF block,
  so critical-edge forwarders to the same trap are bypassed too.
- x64 emit (inst/emit.rs, `CallKnown` + `CallUnknown`): when set, emit
  `ud2` in place of the `jmp continuation` — `callq; ud2`, matching
  LLVM's `call f; unreachable` shape. Trap record is registered via the
  assembler's `add_trap`; exception-handler edges (add_try_call_site)
  are unchanged.
- aarch64 emit gets the same treatment (`bl; udf` via `Inst::Udf`).

Results (regex-syntax rlib):

- x64: `callq;jmp` 2,701 → 1,067; total `jmp` 10,189 → 9,069 (−11%);
  `callq;ud2` 1,144 → 2,774; `__text` 171,764 → 170,852 B (−912 B).
- aarch64: `b` 9,161 → 8,287 (−9.5% vs the edgefwd-era build).
- Stock x64 reference: 534 `callq;jmp` (incl. 141 → ud2) + 71 inline
  `callq;ud2` — the residual pliron gap is try_call site COUNT (2.7k
  invoke sites vs ~600 LLVM) and non-trap continuations, i.e. the same
  block-count inflation, not this pattern.

Correctness: unwind semantics unaffected — the ud2 only executes on a
normal return into `unreachable`; landing pads come from
`add_try_call_site` at the call offset, untouched. cargo build clean;
test.sh green (incl. aarch64 unwind); PLIRON_VERIFY=1 clean on whole
x64 regex-syntax; Rosetta x86_64-apple-darwin std+unwind pass.

sum2d residual check (x64, Rosetta): pliron 0.788–0.795 ms vs stock
0.766–0.777 — ~2%, inside earlier-run noise; the ledger's "-5%"
residual is stale. Accepted as parity.

### Dual-128 loopvec probe + hist re-audit (x64-v3, Rosetta)

Dual-128 turns out to be already implemented: loopvec's vector body
unrolls by `UNROLL` (`src/loopvec.rs:2419`), currently **4×128-bit
groups per iteration** — quad-128, not dual. Probe of `unroll=8` on
x64 (reverted): no kernel improved, several regressed — even_sum
−21% (0.607→0.733), dot_i8 −10%, dot_u8/sum_sq −5%; the extra
accumulator regs + epilogue cost lose to port pressure. **4×128 is
the saturation point; keep UNROLL=4.** A per-arch tunable is
possible (`apply` already takes `x64`) but measurement says don't
bother.

Wide-kernel results, `-Ctarget-cpu=x86-64-v3` under Rosetta
(pliron / stock ms/iter): axpy 0.364/0.593, vadd_u32 0.525/0.653,
clamp_u8 0.090/0.142, dot_i32 0.088/0.130, sum_u8 0.128/0.694
(psadbw), dot_i8 0.300/0.430, matmul_256 4.832/12.062, has_val
0.382/1.491, max_u 0.194/0.353, min_i 0.191/0.227, sum_sq
0.882/1.057, dot_u8 0.461/1.445, scaled 0.727/0.941, rev_copy32
0.362/0.636, cnt_vowel 0.666/1.562, even_sum 0.607/1.844, xor_fold
0.365/0.422, find_off 0.084/1.198, fillzero parity, sum2d
0.780/0.750 (-4%, ordered fadd — accepted). **Every vectorizable
kernel beats LLVM's ymm codegen** — dual-128-in-spirit is already
winning; a real ymm regclass is not on the critical path.

scatter/hist re-audit on shared HEAD (coldedges + coldargs + edgefwd
+ foldf + vmax all merged): gather 0.455 vs stock 0.495 — now a
**win** (+8%). hist 0.619 vs 0.444 — 1.5x -> 1.39x residual, not
closed. Per-element in the 4x-unrolled hot loop: movzbq + cmpq + jae
+ addl RMW (4 core insts, same as stock's movzbl/cmpq/jbe/incl) plus
**~3 cold-edge ABI-pinning copies**: `movq %r11,%rdi` (iv -> panic
arg reg), `movq %rdi,%r9` / `movq %r9,%rdi` (a[i] save/restore around
the `jae` to `panic_bounds_check(index=rdi,len=rsi)`). ~8.5 vs ~5.7
insts/element. The residual lives in coldedge adapter/ABI-pinning
copies (native agent's coldedges work, 87909ba) — not edgefwd's
domain. A deeper fix would hoist the per-element `a[i] < len` check:
a[i] is u8 and cnt.len()=256 makes it statically provable —
constraint-elimination territory (LLVM doesn't do it either; we'd
*beat* stock if we did).

Verify: cargo build, test.sh green, Rosetta std+unwind pass at
0b88590.

### Constraint elimination (src/celim.rs, PLIRON_CELIM)

General dominating-condition compare elimination on final CLIF — a
superset of jumpthread's `fold_dominated_conds` (PLIRON_DOMCOND),
run late (after foldf, before sameargs/coldedge) so it sees checks
materialized by switchmap/bcheck/loopvec/punroll/ifconv and so a
folded check's dead panic edge never gets a cold adapter.

Beyond domcond it handles: multi-predecessor successors when every
other pred is dominated by the dest (loop headers — entry via the
guard edge is the only way into the region); fact operands restated
through edge-arg -> block-param maps (only for slots every edge
passes identically — the uniform_slot check keeps loop re-entry
sound); `brif v` non-icmp facts (`v != 0`); +-1 operand offsets both
directions (`x<y` => `x+1<=y`, `x-1<y` fact => `x<=y`; wrap cases are
excluded by the fact holding); `x-1<k`/`x+1>k` range tightening; one
transitivity hop; signed<->unsigned range transfer (`x s<0` =>
`x u>= half`).

Measured (PLIRON_STATS, regex-syntax -O): domcond folds 2,360
icmps; celim adds **+243 in 77 fns** (~10%) — translate/visitor,
IntervalSet::difference, literal extract, Display fmt, `Pattern::
is_contained_in`, allocator reserve/spec paths. Also fires in
cpubench mains + `find_byte_off`. Verification: `func.replace` keeps
the iconst type; folded `brif` retargeted via fold_const_branches.
`PLIRON_VERIFY=1` clean on x64 regex-syntax; test.sh green; Rosetta
std+unwind pass; targeted checks (`a[i]` folded, `a[i+1]` kept under
`i<len`; `i<len && j<len` elides both) disasm-verified. Soundness
fixes en route: block-param substitution only on uniform slots, and
the `(x-y)==0` norm reports sub operands as range raw (was comparing
`x-y`'s range against `y`'s const — wrong predicate).

Perf: neutral on micros — hist 0.596-0.600 (same as baseline; its
per-element `a[i]`+`cnt[v]` checks aren't dominated by anything, the
residual stays cold-edge adapter copies); gather 0.452 vs stock
0.491; wide kernels unchanged. The win is check removal on real
code paths (bounds-check-dense Rust), not the hist hot loop.

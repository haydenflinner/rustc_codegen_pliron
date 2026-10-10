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

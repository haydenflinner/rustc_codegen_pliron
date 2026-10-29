# 2026-10-29 CubeCL speed burst (branch `burst/cubecl-speed`)

Goal: close the kernel-throughput gap vs LLVM on aarch64 (geomean 1.35x over 45
rows; max-pool forward 2.7-3x, f16 GEMM ~2x). Burst rules: fast tests only.

## 1. PLIRON_VSELECT / PLIRON_FMINMAX: semantics pinned, default flip prepared

- `34c93e4` moves the emitters (`fminmax_num`, `pliron.vcmp.*`,
  `pliron.vbitselect`) from `pliron-clif-lower/src/lower.rs` into
  `pliron-clif-opt/src/vecsel.rs` (no behavior change; `lower.rs` calls them), so
  tests build from the exact production code without rustc or pliron IR.
- `pliron-clif-opt/tests/vecsel_native.rs` (aarch64 macOS) compiles those
  emitters with the vendored Cranelift (opt `none` and `speed_and_size`), runs
  them in a MAP_JIT page and compares with a reference:
  - min/max: all 18x18 pairs of {+-0, +-1, 1.5, -2.5, +-inf, qNaN, -NaN, sNaN with
    payload, negative NaN with payload, +-min subnormal, max subnormal,
    MIN_POSITIVE, +-MAX}; every lane of f32x4 / f64x2 / f32x2 (lane-by-lane path)
    meets every pair; f32, f64 scalar. Reference = LLVM `maxnum`/`minnum` (one NaN
    -> the other operand, both NaN -> NaN, +-0 either). Result: no mismatch, the
    lowering is correct as written. Mixed zeros: the sign equals what this
    host's `f32::max`/`f32::min` return (test prints a note if not; none seen).
  - select fusion: 6 fcmp predicates (oeq/une/olt/ole/ogt/oge; NaN lanes: ordered
    false, `une` true) x {f32x4, i32x4} select arms, exhaustive special pairs plus
    4000 random patterns each; 10 icmp predicates (signed + unsigned) on i32x4,
    4000 random + edge values each. No mismatch.
  - Mutation check (done by hand): dropping the NaN fixup or mapping `gt` to
    `ge` makes 6 of the 9 tests fail.
- Not covered by a fast test: `vselect_fuse` itself (the pliron-IR pattern match:
  which selects qualify, operand type checks, erasing the dead compare) and the
  `fminmax_num` call sites (f16 widen/narrow, leafwise split). Those run in
  cubecl-cpu (1068/1068 with both flags, recorded 2026-10-29) and need the corpus.
- `08dd5ae` flips both to `DEFAULT_ON` in `pliron-cfg` (`=0` kills; there are no
  vendored-Cranelift sites for these two). It is a separate commit; revert it
  alone to go back to opt-in. Needed before merging (slow, not run here):
  `harness/gate.py`, `harness/gate.py --full`, the rustc corpus/fuzz/crates
  (`f32::max`/`min` is `llvm.maxnum`/`minnum` in every Rust program), the full
  cubecl-cpu suite, `harness/parity.py` (native bench kernels that use
  `f32::max`).

## 2. f16 GEMM: where the gap is (diagnosis only, no code)

Dumped the objects of the existing `gemm_cpu` bench binary (cubek, built against
an older worktree of this repo, so it measures the Oct-29 lowering) with
`CUBEK_BENCH_STRATEGIES=cpu_gemm CUBEK_BENCH_SAMPLES=1` and
`CUBECL_PLIRON_DUMP_OBJ`, once per path (9 `cpu_gemm_kernel_e_f16*` kernels).

- Both paths use the packed `fmla.8h` (8-24 of them per kernel): the FMA itself is
  not widened. CLIF objects are 1.6-2.6 KB vs LLVM's 0.9-1.4 KB.
- The gap is the inner loop around it:
  1. **Scalar f16 -> f16x8 broadcast** (the A element times a B row): CLIF emits
     `ldr h; fcvt s,h; mov.16b v,zero; mov.s v[0]; fcvtn; fcvtn2; fcvtl; mov.16b;
     mov.s v[0..3] x4; fcvtn; fcvtn2` = 13 instructions per broadcast (and again
     for every second operand); LLVM emits `ld1r.8h` / `dup.8h`. The shape is
     `fptrunc(shufflevector-splat(fpext(fptrunc(insertelement(zero, fpext(h), 0)))))`
     in the f32 domain: CubeCL builds the f16 line through f32 lanes and the
     narrow/widen pairs between the steps are never folded (`fpext(fptrunc(x))`
     is not an identity in general, but is when every lane of x came from
     `fpext(half)`).
  2. **Spills**: 25-68 `ldur`/`stur` to the frame per kernel and 22-51 `mov.16b`
     register copies vs LLVM's `movi.2d` zeroes and `ldp q`/`stp q` only (LLVM keeps
     the 8-16 accumulators and operands in v-registers; the CLIF loop reloads loop
     bounds and indices from `[sp, #..]` each iteration). That is regalloc
     pressure from the integer index math (`madd`, `lsr`, `and` per iteration, not
     strength-reduced), plus the temporaries of (1).
  3. Prologue: `movi; fcvtl; fcvt; mov.s; fcvtn; fcvtn2` build a zero f16x8 the
     same roundabout way (cold, but it shows the pattern is general).
- Next step (not done, needs the cubecl rebuild against this worktree to measure):
  a default-off IR or CLIF peephole `PLIRON_F16SPLAT`: `insertlane/splat` of an
  `fpromote`d f16 into an f32 vector that is immediately `fvdemote_f16_pair`ed
  becomes one `splat.f16x8` of the original scalar (-> `dup.8h`); then re-look at
  the loop's spills (hoist the index math, `PLIRON_PUNROLL` budget).

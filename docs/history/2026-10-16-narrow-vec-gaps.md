# Narrow-vector (<128-bit) gaps in the pliron→CLIF path

Diagnosis of the sub-128-bit vector throughput gaps found in the cubecl
CPU-JIT parity sweep (compute_direct, Xeon 8375C — AVX512+F16C, no
AVX512-FP16/BF16):

| kernel | llvm GOPS | clif GOPS | ratio |
|---|---|---|---|
| f16v2  | ~107 | ~13  | ~8.5x |
| f16v4  | ~122 | ~26  | ~4.6x |
| f32v2  | ~60-100 | ~14 | ~4-7x (noisy) |

Two distinct mechanisms, both in the CubeCL `<N x T>` ops path
(`scalarize.rs` — rustc-generated SIMD goes through `pliron.v*`
intrinsics and never sees this pass).

## Repro

From a cubecl checkout (backend pinned to rev 4dfdd1d):

    rm -rf ~/.cache/cubecl target/environment
    CUBECL_CPU_JIT=clif PLIRON_CLIF=compute_direct_throughput_i_f32_n_2 \
      ./target/release/examples/compute_direct   # writes CLIF dumps to stderr

The dump for `i_f32_n_2` shows the `<2 x f32>` hot-loop op decomposed
into per-lane moves around one scalar `fma` per lane:

    block7(..., v16: f32x2, ...):
        v84  = vconst.f32x2 const0
        v85  = extractlane.f32x2 v19, 0
        v86  = extractlane.f32x2 v18, 0
        v87  = fma v85, v86, ...        ; scalar fma, lane 0
        v88  = insertlane v84, v87, 0
        v89  = extractlane.f32x2 v19, 1
        v90  = extractlane.f32x2 v18, 1
        v91  = fma v89, v90, ...
        v92  = insertlane v88, v91, 1   ; 2 extracts + scalar fma + insert, x2

The `i_f16_n_4` dump is subtler: the `fma` itself is already `f32x4`
(the kernel accumulates in f32), and the gap is the *casts* — every
`<4 x f16>` ↔ `<4 x f32>` `fpext`/`fptrunc` runs per lane:

    v179 = fpromote.f32 v28             ; scalar f16 -> f32 (vcvtph2ps)
    v180 = insertlane v178, v179, 0
    ... 3 more lanes ...
    v205 = fma v186, v195, v204         ; f32x4 — fine
    v214 = extractlane v205, 0
    v215 = fdemote.f16 v214             ; scalar f32 -> f16 (vcvtps2ph)
    ... 3 more lanes ...

~8 scalar converts + 8 lane moves per input pair, vs LLVM's one
`vcvtph2ps xmm` per `<4 x f16>` operand. No libcalls appear on hosts
with F16C (`fpromote`/`fdemote` lower to vcvtph2ps/vcvtps2ph);
without F16C the same per-lane cost is paid as `__extendhfsf2` /
`__truncsfhf2` calls.

## The gates

Three independent gates, in order of what's hit first:

1. **`pliron-clif-lower/src/scalarize.rs:92-97` (`simd_leaves`)** — a
   vector op is only kept leafwise when every leaf `is_vector() &&
   bits() >= 128`. `F32X2`/`F64X1` are legal leaves
   (`types.rs` allows 64-bit leaf widths), so loads/stores/params stay
   packed, but every *arithmetic* op on them is split per lane by
   `split()`. The comment on `native_leafwise` (`scalarize.rs:58`) says
   why: "64-bit vectors (`f32x2`) stay split: x64 mis-lowers some of
   them."

2. **`pliron-ir-opt/src/types.rs:230-236` (`native_vec`)** — the FP16
   lane case returns `None` unless `n == 8 && NATIVE_F16` (AVX512-FP16,
   unavailable here). So `<4 x f16>`/`<2 x f16>` never get a leaf —
   unlike `<8 x bf16>`, which uses the `I16X8` bit-pattern container —
   and every op touching them (including the fpext/fptrunc casts that
   dominate the f16n4 kernel) splits per lane.

3. **`pliron-clif-lower/src/scalarize.rs:305-321`
   (`f16_vec_promotable`)** — the promote-f16-arith-to-f32-twin path
   requires `wide_f32_candidate(f32, n)`, i.e. `(n*32) % 256 == 0` —
   n >= 8. `f16x4` (twin `f32x4`, 128-bit native) never qualifies.

And the reason gate 1 can't just be deleted —
**`vendor/cranelift-codegen/src/isa/x64/lower.isle` has no rules for
64-bit float-vector arithmetic.** `ty_64` appears only in
load/store/`scalar_to_vector` rules; `fadd`/`fsub`/`fmul`/`fdiv`
(2974-3069) and `fma` (3247-3313) only match `$F32`, `$F64`, `$F32X4`,
`$F64X2`. Un-gating `simd_leaves` to 64-bit makes codegen panic on the
first `fma.f32x2`. Likewise the vendored F16C rules
(`fvpromote_f16`/`fvdemote_f16`, ~5562-5565) only arm `$F32X8`/`$F32X16`
→ `vcvtph2ps_{y,z}` — no xmm/`$F32X4` form.

## What it'd take

**f32v2 / f64v1** — LLVM's approach is type legalization: it widens
`<2 x f32>` to `<4 x f32>` with undef lanes and uses the standard 128-bit
ops. Two ways to get there here:

- *Lowerer option:* a pad-promotion in `scalarize.rs` — treat
  `<2 x f32>`/`<1 x f64>` leafwise ops like `wide_op` does for ymm/zmm:
  pad the value into the 128-bit twin once per op cluster (`insertlane`
  into an `F32X4` leaf works today — the dumps show lane ops on `F32X2`
  lower fine), run all leafwise ops on the 128-bit leaf, `extractlane`
  back where a `f32x2` result is demanded. Needs the same "every op in
  the cluster participates" coverage check that `plan_wide` does, and
  the pad/narrow hoisted out of the per-op split rather than repeated.
- *Backend option:* ISLE rules for 64-bit float ops that reuse the
  128-bit encodings — `addps`/`vfmadd` compute 4 lanes; the top 2 are
  don't-care because `F32X2` has only 2 semantic lanes. ~one rule arm
  per opcode family (`fadd/fsub/fmul/fdiv/fmin/fmax/fma/fneg/fsqrt`,
  `fcmp`+`select`, `fcvt`). More rules, zero IR machinery.

Either way this is a patch series, not a gate flip.

**f16v4** — the existing bf16 machinery is the template: `native_vec`
already returns `I16X8` for `<8 x bf16>` as a bit-pattern container and
lower.rs lowers `fpext`/`fptrunc` on it via `fvpromote_bf16`/
`fvdemote_bf16`. The f16x4 analog is:

- `native_vec`: `<4 x f16>` → `I16X4` under `WIDE_SIMD && F16C`
- scalarize: extend the `bfv`/`hfv` container accounting
  (`native_leafwise`, ~69-87) so `fpext`/`fptrunc` between `I16X4` and
  `F32X4` stay leafwise
- vendored ISLE: add `$F32X4`/`$F16X4` arms → `vcvtph2ps xmm` /
  `vcvtps2ph xmm` (F16C; the assembler decls exist for ymm/zmm — add
  the xmm forms)

That turns each cast into 1 packed convert instead of 4 converts + 8
lane moves. Estimated value: recovers most of the 4.6x f16v4 gap.

**f16v2** — hardest of the three: the `<2 x f16>` container would be
`I16X2` (32-bit, below any useful x86 vector width), and the `f32x2`
compute twin is itself scalarized by gate 1. Falls out if the f32x2
fix lands first (cast to a padded `f32x4`, compute there).

## Recommendation

Order by effort-vs-gap: (1) f16v4 packed-cast path — mirrors the bf16
path closely, clear 4-6x win; (2) f32v2 via whichever of the two
options fits the vendoring policy (the ISLE-rule route also unblocks
future 64-bit vector users like aarch64's `cmeq.8b` idiom on x86);
(3) f16v2 falls out of both. None are trivially removable gates, so
this stops at diagnosis per the task.

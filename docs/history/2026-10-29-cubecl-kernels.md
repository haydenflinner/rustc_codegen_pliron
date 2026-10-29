# 2026-10-29: CubeCL's CPU kernels through our Cranelift path

Goal: make `CUBECL_PLIRON_CLIF=1` (cubecl fork `902ef33`, Oct 16) work against
current main, test it against LLVM, measure it. Result and recipe:
[`../cubecl-burn.md`](../cubecl-burn.md). This file is the log.

## Timeline

1. **The Oct-16 commit no longer built**: 609 errors in `cubecl-ir` alone. The
   vendored pliron moved operand/result/region/successor storage into
   context-global pools ("DoD" steps 5b/5c) and interned attributes
   (`AttrInstId`), so `Operation::get_operand(i)`, `regions()`,
   `BasicBlock::get_argument(i)` etc. take `&Context` and `AttributeDict` holds
   ids, not boxed attributes. Mechanical port, scripted from rustc's "provide the
   argument" suggestions (a JSON-diagnostics fixer that substitutes `ctx`), then
   by hand where `ctx` was not in scope or attributes were mutated in place
   (`FuncInterface` arg/res attrs, `pop_argument`/`remove_argument`).
   `cubecl-ir`, `-opt`, `-core`, `-cpp`, `-wgpu` ported; ~540 call sites.
2. A fixer run over `-p cubecl-spirv` rewrote 774 sites inside the registry
   copy of `pliron-spirv` (external crate, built against stock pliron). Deleted
   the extracted dir so cargo re-extracted it, reverted. Do not run it with
   `-p cubecl-spirv`.
3. **Vendored `pliron-derive` bug** (`78fd569`): `format_op`'s `region($n)` and
   `regions` directives called the pool accessors without `ctx`; nothing in this
   repo uses them, so nobody noticed.
4. **Throughput example, first CLIF run**: f16 panicked in the JIT ("should be
   implemented in ISLE: `fvdemote_f16.f16x4`"): aarch64 had only the 128-bit
   pair form. Added the rule (`21efaff`).
5. **cubecl-cpu tests, first CLIF run**: 989 pass / 79 fail (all f16 +
   `bf16_conversion`), 0 LLVM failures. Bisect: `PLIRON_SIMD=0` made `test_abs`
   pass (`knobs.py`-style loop over `PLIRON_*=0`), so it was a lowering of
   narrow vectors, not an ir-opt pass. `CUBECL_PLIRON_DUMP_OBJ` + `llvm-objdump`
   of the failing kernel showed `fmov h20, h20` where the CLIF had `fpromote.f32`
   and `fsqrt s20, s19` where it had `fdemote.f16`: the vcode text printed
   `fcvt` but the emitted words were wrong (`ce117a4`; the `#44` encoding patch
   had opcode bits 000000/000011 for 000100/000111). After that the f16x4 leaf
   showed `fcvtn v6.2s, v6.2d` (f64->f32) for a demote that should be
   `fcvtn v.4h, v.4s`: the new rule and the existing pair rule passed the source
   lane size (`ec415fc`). The text-only tests of #44 had hidden both; the new
   tests assert on the emitted words. All 1068 pass.
6. Burn: its pinned cubecl (`5e521ef`) is ~90 upstream commits behind the fork,
   and the Oct-16 CLIF engine sits on a different JIT wrapper (`OrcJit` vs
   `LLVMLLJIT` there), so a backport was not attempted; instead Burn main was
   moved forward to the fork's cubecl (three compat edits). 2366/2366 on both paths.
7. Measured under `lockf`, load 13-40 (other jobs).

## Surprises

- `measure_peak_throughput` caches per device fingerprint: the first CLIF run
  printed `measured in 0.0 s` and replayed an earlier (wrong-f16) number. A
  `cubecl.toml` with `[throughput] disable_cache = true` fixes it. The cache
  fingerprint now carries `-clif` too.
- The persistent kernel cache (`[compilation] cache`, off by default) is keyed by
  fingerprint + build id; both paths shared one until the `-clif` suffix.
- libtest swallows `eprintln!` in passing tests, so counting `[cubecl-pliron]`
  lines needs a file (`CUBECL_PLIRON_LOG=<file>`); that is also how the test
  harness proves each run stayed on its path.
- A worktree-isolated agent cannot run compound shell (`git` anywhere in the
  text, `$(...)`, `$var` as the command) -- use scripts in the scratchpad.

## Slow kernels (SPD 78, 85 with the flags)

Looked at the worst rows by diffing the two objects of one kernel
(`CUBECL_PLIRON_DUMP_OBJ`, `llvm-objdump -d`):

- **Pooling**, `pool2d_direct_e_f32_n_16` (`max_pool2d_forward_deep`: 30 ms LLVM vs
  108 ms CLIF; object 1528 B vs 3584 B). Two gaps:
  1. `select(fcmp ogt <16 x f32>, ..)` on `<16 x f32>`/`<16 x i32>` (running max and
     argmax). `scalarize.rs` splits `FCmp`/`ICmp`/`SelectOp` per lane because a
     `<N x i1>` mask has no vector leaf in `types::leaves`: 16 `extractlane` +
     scalar `fcmp` + `select` + `insertlane` per compare. Fix: `PLIRON_VSELECT=1`
     rewrites `select(fcmp/icmp(a, b), x, y)` over all-128-bit-leaf 32-bit-lane
     vectors to the `pliron.vcmp.<cc>.<f|s|u>` + `pliron.vbitselect` the rustc glue
     already emits for portable-simd, which the lowerer applies per leaf.
  2. `llvm.maxnum.f32` was one `fmaxf` libcall per lane (16 `bl _fmaxf` in the
     object), which also forces spills around the calls. Fix: `PLIRON_FMINMAX=1`
     lowers it inline (`fmax` + `fcmp uno` selects; `bitselect` per leaf on
     vectors, lane by lane on 64-bit ones where the vendored aarch64 ISLE has no
     `bitselect`) and adds `maxnum`/`minnum` to the leafwise intrinsics.
  Both default off (AGENTS rule 2). Result with both on, load 9-10:
  `max_pool2d_forward_deep` 2.96x -> 1.16x LLVM's time, `forward_resnet_init`
  2.71x -> 1.46x, card SPD 78 -> 85. cubecl-cpu 1068/1068 under CLIF with both.
  Not flipped on: `llvm.maxnum` is `f32::max` in every Rust program, so the flip
  needs the rustc corpus/gate, which I did not run for it.
- **f16 GEMM (1.2-2.0x slower)**: not examined beyond object size (bf16 compute
  n=32 object is 2.4x LLVM's: 11.4 KB vs 4.7 KB), consistent with per-leaf
  widen/narrow and lane moves where LLVM keeps packed ops.
- Where CLIF is faster: kernel JIT (0.51x), `reduce` unit sum on some rows.
- Disk: three CubeCL/Burn builds are ~8.5 + 3.2 + 5.5 GB of `target`; the box hit
  12 GB free once (link failed with `cc` exit 1) and Burn's `target` was deleted
  (it rebuilds in ~15 min).

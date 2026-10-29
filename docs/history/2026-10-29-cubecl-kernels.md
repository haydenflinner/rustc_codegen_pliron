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
3. **Vendored `pliron-derive` bug** (`85f8431`): `format_op`'s `region($n)` and
   `regions` directives called the pool accessors without `ctx`; nothing in this
   repo uses them, so nobody noticed.
4. **Throughput example, first CLIF run**: f16 panicked in the JIT ("should be
   implemented in ISLE: `fvdemote_f16.f16x4`"): aarch64 had only the 128-bit
   pair form. Added the rule (`c0c38b3`).
5. **cubecl-cpu tests, first CLIF run**: 989 pass / 79 fail (all f16 +
   `bf16_conversion`), 0 LLVM failures. Bisect: `PLIRON_SIMD=0` made `test_abs`
   pass (`knobs.py`-style loop over `PLIRON_*=0`), so it was a lowering of
   narrow vectors, not an ir-opt pass. `CUBECL_PLIRON_DUMP_OBJ` + `llvm-objdump`
   of the failing kernel showed `fmov h20, h20` where the CLIF had `fpromote.f32`
   and `fsqrt s20, s19` where it had `fdemote.f16`: the vcode text printed
   `fcvt` but the emitted words were wrong (`1c14663`; the `#44` encoding patch
   had opcode bits 000000/000011 for 000100/000111). After that the f16x4 leaf
   showed `fcvtn v6.2s, v6.2d` (f64->f32) for a demote that should be
   `fcvtn v.4h, v.4s`: the new rule and the existing pair rule passed the source
   lane size (`72a6ebc`). The text-only tests of #44 had hidden both; the new
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

## Slow kernels (SPD 76)

See the table in `cubecl-burn.md`. Notes from looking at the worst rows
(filled in below as they are understood).

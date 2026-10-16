# Overnight debug log — windows-msvc native bench crash + MSVC-free toolchain work

## What was broken (root cause, FIXED)

Criterion `--baseline` crashed with `0xc0000005` inside LLVM-compiled
`criterion::analysis::compare::estimates::stats` — iterating a `&[f64]` whose
data pointer was actually a stack address.

Root cause chain:
- `stats(a: &Sample, b: &Sample) -> (f64, f64)` — Rust ABI returns the pair in
  xmm0/xmm1 (verified against stock LLVM output).
- Our dialect/CLIF for the call was correct: `(i64,i64,i64,i64) -> f64,f64`.
- BUT vendored cranelift's `get_fltreg_for_retval(WindowsFastcall)` listed only
  `xmm0` — the second f64 return couldn't fit → `compute_arg_locs` saw
  `sized_stack_ret_space > 0` → `add_ret_area_ptr` silently injected a hidden
  return-area pointer as arg0 (rcx). Emitted call: `(sret_buf, a.ptr, a.len,
  b.ptr, b.len)`. LLVM callee read rcx=a.ptr → wild pointer → crash.
- Same path explains the "thunk" (s139 shim) oddities: every `{f64,f64}`
  multi-ret call/def on windows-msvc got the phantom first arg.

Fix (in `vendor/cranelift-codegen/src/isa/x64/abi.rs`): added
`1 => Some(regs::xmm1())` to the WindowsFastcall float-retval list — the same
way the i128 fix added `rdx` to the int-retval list ("The Rust ABI for i128s
needs this"). Only affects sigs with >=2 float rets, which only come from
rustc ScalarPair returns; extern-"C" large-struct returns are Indirect at the
dialect level anyway (sret param), so MSVC C ABI is unaffected.

Minimal repro (worked before fix):
    // callee.rs (stock rustc rlib): pub fn stats(a:&Sample,b:&Sample)->(f64,f64)
    // main.rs (pliron): let (x,y) = callee::stats(mk(&a), mk(&b));
    → segfault pre-fix, correct output post-fix.

Full bench now runs `--baseline` comparisons to completion. Perf deltas vs
LLVM baseline are noisy (a few +/-10-30%); worth a proper look later.

## Diagnostics added

- `pliron::lower` tracing: `callsig=` (param/ret count per lowered call),
  `lowerfn=` (per-function marker). `PLIRON_TRACE='pliron::lower=debug'`.
- bench/benches/kernels.rs: VEH crash-probe printing rip/aaddr (dbghelp
  symbolization attempt was unreliable; .map file + manual RVA math worked).

## Known remaining issues

- `pliron-clif-opt/src/loadfwd.rs:1084` ICE at `-Ccodegen-units=1`:
  `loadpre: location not available at end of block` — latent, separate.
- Vec<f64> in-memory order is {cap@0, ptr@8, len@16} on this toolchain —
  verified empirically (relevant to any hand-rolled fat-ptr handling).
- Criterion perf vs LLVM: strwork -30% (we're faster), sortw +20%, tokenize
  +12%, parse_only +10% (slower) — mostly small-N noise, but sortw is worth
  profiling.

## Questions for review

- OK to keep the temporary repro bin + serde deps out of the repo? (dropped
  from the commit; they were only crash repro scaffolding.)
- Next: MSVC-free Windows link path — see plan below.

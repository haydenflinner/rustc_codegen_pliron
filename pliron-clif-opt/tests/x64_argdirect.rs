//! x64 call-argument detachment (`PLIRON_ARGCOPY`, default-on) copies a
//! call arg into a fresh vreg when the value has other uses, so the
//! ABI's fixed-register use does not pin the value's whole bundle to a
//! call-clobbered register. `PLIRON_ARGDIRECT=1` (opt-in) narrows "has
//! other uses" from the transitive `value_ir_uses == Multiple` mark to
//! `value_direct_uses > 1`: a value whose only use is this call dies at
//! the call, so pinning it to the arg register is free and the copy is
//! pure waste. The transitive mark fires through the call *result* — a
//! multiply-used consumer back-propagates `Multiple` to the args of the
//! call that produced it, which is what happens in fib's recursive
//! loop (the call result feeds the multiply-used accumulator).

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str, argdirect: &str) -> String {
    unsafe { std::env::set_var("PLIRON_ARGDIRECT", argdirect) };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("x86_64")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

/// fib's recursive loop shape: the call arg `v4 = isub cur, 1` has a
/// single direct use (the call) but is transitively `Multiple` — the
/// call result `v5` feeds `v7 = iadd`, which is used by both back-edge
/// and exit-edge block args, so `Multiple` propagates `v7 -> v5 ->
/// call args -> v4`.
const FIB_LOOP: &str = "
function %fib(i32) -> i64 system_v {
    sig0 = (i32) -> i64 system_v
    fn0 = %fib sig0
block0(v0: i32):
    v1 = iconst.i32 2
    v2 = icmp uge v0, v1
    v20 = uextend.i64 v0
    v21 = iconst.i64 0
    brif v2, block2(v0, v21), block1(v20)
block1(v22: i64):
    return v22
block2(v27: i32, v28: i64):
    v3 = iconst.i32 1
    v4 = isub v27, v3
    v5 = call fn0(v4)
    v6 = isub v27, v1
    v7 = iadd.i64 v5, v28
    v8 = icmp uge v6, v1
    brif v8, block2(v6, v7), block1(v7)
}";

#[test]
fn single_use_arg_not_detached() {
    let off = asm(FIB_LOOP, "0");
    let on = asm(FIB_LOOP, "1");
    // With the transitive mark the arg is detached: the producer writes
    // a scratch reg and a `movl` copies it into `%rdi` at the call.
    assert!(off.contains("movl %eax, %edi"), "off:\n{off}");
    // With direct-use counting the producer's destination is the arg
    // register itself — no copy.
    assert!(!on.contains("movl %eax, %edi"), "on:\n{on}");
    assert!(on.contains("subl $0x1, %edi"), "on:\n{on}");
}

/// The genuinely multi-use case must keep detaching: `v4` is both the
/// call arg and the loop's carried counter, so the fixed `%rdi` pin
/// would otherwise hold a long-lived value in a call-clobbered reg.
const MULTI_USE_ARG: &str = "
function %g(i32) -> i32 system_v {
    sig0 = (i32) -> i32 system_v
    fn0 = %h sig0
block0(v0: i32):
    v1 = iconst.i32 0
    jump block1(v0, v1)
block1(v10: i32, v11: i32):
    v12 = call fn0(v10)
    v13 = iadd v12, v11
    v14 = iconst.i32 1
    v15 = isub v10, v14
    v16 = iconst.i32 2
    v17 = icmp uge v15, v16
    brif v17, block1(v15, v13), block2(v13)
block2(v20: i32):
    return v20
}";

#[test]
fn multi_use_arg_still_detached() {
    let on = asm(MULTI_USE_ARG, "1");
    // The arg copy survives — `v10` is used by the call *and* by
    // `v15 = isub`, so `value_direct_uses[v10] == 2`.
    assert!(on.contains("movl %r"), "on:\n{on}");
}

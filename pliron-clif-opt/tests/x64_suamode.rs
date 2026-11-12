//! x64 `to_amode_add`: an `iadd x, K` operand folds `K` into the
//! lea/load displacement. Under `PLIRON_X64_SUAMODE` (default-on) the
//! fold is restricted to sums with <=1 direct use — a multi-use sum
//! is materialized anyway for its other consumers, and folding then
//! keeps `x` live alongside it (parse's digit adds: folded
//! `lea -0x30(char, acc*2)` kept `char` live across the range check;
//! `lea (digit, acc*2)` frees it). Env off/unset = unconditional
//! fold, identical to stock.
//!
//! `su_amode_enabled` is a `OnceLock`, so this binary pins the env to
//! "1"; `x64_suamode_off.rs` pins "0".

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_X64_SUAMODE", "1") };
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

/// `v0 - 48` has one use (the `iadd` that becomes `lea`): the
/// displacement fold still fires.
#[test]
fn single_use_folds() {
    let a = asm(
        "
function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v9 = iconst.i64 -48
    v2 = iadd v0, v9
    v3 = iadd v2, v1
    return v3
}",
    );
    assert!(a.contains("-0x30(%rdi, %rsi)"), "{a}");
}

/// `v0 - 48` feeds both the lea'd `iadd` and a second consumer: no
/// displacement fold — the sum materializes once and the indexed lea
/// reuses its register.
#[test]
fn multi_use_materializes() {
    let a = asm(
        "
function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v9 = iconst.i64 -48
    v2 = iadd v0, v9
    v3 = iadd v2, v1
    v4 = iadd v3, v2
    return v4
}",
    );
    // `v3` must not re-derive `v0 - 48` inside its address expression.
    assert!(!a.contains("-0x30(%rdi, %rsi)"), "{a}");
    // The sum is materialized for the second use.
    assert!(a.contains("-0x30(%rdi)"), "{a}");
}

/// Same with the constant on the `iadd` lhs and the multi-use sum on
/// the to_amode_add rhs: the fold still must not fire.
#[test]
fn multi_use_swapped() {
    let a = asm(
        "
function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v9 = iconst.i64 -48
    v2 = iadd v9, v0
    v3 = iadd v1, v2
    v4 = iadd v3, v2
    return v4
}",
    );
    assert!(!a.contains("-0x30(%rsi, %rdi)") && !a.contains("-0x30(%rdi, %rsi)"), "{a}");
}

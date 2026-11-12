//! `PLIRON_X64_SUAMODE=0`: `to_amode_add` folds `iadd x, K` into the
//! displacement unconditionally (stock Cranelift), even when the sum
//! has multiple uses — duplicated `K` per consumer.

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_X64_SUAMODE", "0") };
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

/// `v0 - 48` has two uses; the fold fires for both, duplicating the
/// `-0x30` displacement and keeping `v0` live across both leas.
#[test]
fn multi_use_folds_stock() {
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
    assert!(a.contains("-0x30(%rdi, %rsi)"), "{a}");
    assert_eq!(a.matches("-0x30(").count(), 2, "{a}");
}

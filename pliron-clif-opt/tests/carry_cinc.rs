//! aarch64: the unsigned-carry idiom `hi + (lo + c <u lo ? 1 : 0)` lowers
//! to `cmn lo, c; cinc hi, hs` (LLVM's `adds`/`cinc`) instead of
//! `add; cmp; cset; add` -- the loop-carried carry of a multi-word add
//! (num-bigint-dig's `add_mul_vvw`).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

fn count(asm: &str, mnem: &str) -> usize {
    asm.lines().filter(|l| l.split_whitespace().next() == Some(mnem)).count()
}

/// The `select c, 1, 0` form our pipeline leaves.
#[test]
fn select_carry_is_cinc() {
    let a = asm("
function %f(i64, i64, i64) -> i64, i64 apple_aarch64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iadd v0, v1
    v4 = icmp ult v3, v0
    v5 = iconst.i64 1
    v6 = iconst.i64 0
    v7 = select v4, v5, v6
    v8 = iadd v2, v7
    return v3, v8
}");
    assert_eq!(count(&a, "csinc"), 1, "{a}");
    assert_eq!(count(&a, "cset"), 0, "{a}");
    assert_eq!(count(&a, "subs"), 0, "{a}");
}

/// The `uextend` form, compared against the other addend.
#[test]
fn uextend_carry_is_cinc() {
    let a = asm("
function %f(i64, i64, i64) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iadd v0, v1
    v4 = icmp ult v3, v1
    v5 = uextend.i64 v4
    v6 = iadd v5, v2
    return v6
}");
    assert_eq!(count(&a, "csinc"), 1, "{a}");
    assert_eq!(count(&a, "cset"), 0, "{a}");
}

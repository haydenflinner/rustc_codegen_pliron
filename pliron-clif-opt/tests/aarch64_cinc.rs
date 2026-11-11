//! aarch64 `x + (a cc b ? 1 : 0)` fusion (`PLIRON_CINC`, opt-in):
//! `iadd x, uextend(icmp cc a b)` lowers to `cmp a, b; csinc x, x, !cc`
//! (LLVM's `cinc`) instead of `cmp; cset; add`. The compare must be
//! single-use: when the bool materializes for another consumer anyway
//! a fused compare would just be a second `cmp`.
//!
//! The knob is read once per process (`OnceLock`), so every test here
//! sets it before its first compile; all tests run with it on.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_CINC", "1") };
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

#[test]
fn cinc_right() {
    let a = asm("
function %f(i64, i64) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64):
    v2 = icmp eq v0, v1
    v3 = uextend.i64 v2
    v4 = iadd v0, v3
    return v4
}");
    assert_eq!(count(&a, "csinc"), 1, "{a}");
    assert_eq!(count(&a, "cset"), 0, "{a}");
    // `eq ? x+1 : x` — the `csinc` increments on the false arm, so the
    // flag condition is inverted to `ne`.
    assert!(a.contains("csinc"), "{a}");
    assert!(a.lines().any(|l| l.contains("csinc") && l.contains("ne")), "{a}");
}

#[test]
fn cinc_left_commuted() {
    let a = asm("
function %f(i32, i32) -> i32 apple_aarch64 {
block0(v0: i32, v1: i32):
    v2 = icmp ult v0, v1
    v3 = uextend.i32 v2
    v4 = iadd v3, v0
    return v4
}");
    assert_eq!(count(&a, "csinc"), 1, "{a}");
    assert_eq!(count(&a, "cset"), 0, "{a}");
    // `ult ? x+1 : x` — `hs` is inverted `lo`/`ult`.
    assert!(a.lines().any(|l| l.contains("csinc") && l.contains("hs")), "{a}");
}

#[test]
fn cinc_declines_multiuse() {
    let a = asm("
function %f(i64, i64, i64) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = icmp eq v0, v1
    v4 = uextend.i64 v3
    v5 = iadd v2, v4
    v6 = iadd v4, v4
    v7 = iadd v5, v6
    return v7
}");
    // The bool is needed as a value by the second add, so its `cset`
    // materializes regardless; fusing `v5`'s compare would just add a
    // second `cmp`. Expect the plain `cmp; cset` chain.
    assert_eq!(count(&a, "csinc"), 0, "{a}");
    assert_eq!(count(&a, "cset"), 1, "{a}");
}

#[test]
fn cinc_narrow_compare() {
    let a = asm("
function %f(i8, i8, i64) -> i64 apple_aarch64 {
block0(v0: i8, v1: i8, v5: i64):
    v2 = icmp eq v0, v1
    v3 = uextend.i64 v2
    v4 = iadd v5, v3
    return v4
}");
    assert_eq!(count(&a, "csinc"), 1, "{a}");
    assert_eq!(count(&a, "cset"), 0, "{a}");
}

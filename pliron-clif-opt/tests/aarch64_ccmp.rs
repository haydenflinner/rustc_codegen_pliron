//! aarch64 conditional-compare fusion (`PLIRON_CCMP`, default off):
//! `band`/`bor` of two integer compares and `select` of a compare
//! against a constant arm lower to a `cmp; ccmp` flag chain — `brif`,
//! `cset` and `csel` consumers read the fused condition directly
//! instead of materializing both compares with `cset` and combining
//! them with `and`/`orr`. The knob is read once per process
//! (`OnceLock`), so this binary sets it before any compile — the same
//! constraint as `narrowext_lower.rs`.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_CCMP", "1") };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    b.set("is_pic", "true").unwrap();
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

/// `v0 < v1 && v2 > v3` consumed by `brif` — `cmp; ccmp …, lo; b.hi`.
const BRIF_BAND: &str = "function %f(i64, i64, i64, i64) {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = band v4, v5
    brif v6, block1, block2
block1:
    return
block2:
    return
}";

/// `v0 < v1 || v2 > v3` consumed by `brif`.
const BRIF_BOR: &str = "function %f(i64, i64, i64, i64) {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = bor v4, v5
    brif v6, block1, block2
block1:
    return
block2:
    return
}";

/// Same `band` produced as an i64 value — `cmp; ccmp; cset`.
const VAL_BAND: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = uextend.i64 v4
    v7 = uextend.i64 v5
    v8 = band v6, v7
    return v8
}";

/// Same `bor` produced as an i32 value.
const VAL_BOR: &str = "function %f(i64, i64, i64, i64) -> i32 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = uextend.i32 v4
    v7 = uextend.i32 v5
    v8 = bor v6, v7
    return v8
}";

/// `select v4, (v2 > v3), 0` — `A && B`.
const SEL_AND: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = uextend.i64 v5
    v7 = iconst.i64 0
    v8 = select v4, v6, v7
    return v8
}";

/// `select v4, 0, (v2 > v3)` — `!A && B`.
const SEL_AND_NOT: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = iconst.i64 0
    v7 = uextend.i64 v5
    v8 = select v4, v6, v7
    return v8
}";

/// `select v4, (v2 > v3), 1` — `!A || B`.
const SEL_OR_NOT: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = uextend.i64 v5
    v7 = iconst.i64 1
    v8 = select v4, v6, v7
    return v8
}";

/// `select v4, 1, (v2 > v3)` — `A || B`.
const SEL_OR: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp slt v0, v1
    v5 = icmp sgt v2, v3
    v6 = iconst.i64 1
    v7 = uextend.i64 v5
    v8 = select v4, v6, v7
    return v8
}";

/// Narrow compare as the band's second operand: only the swapped order
/// fuses (the i8 compare becomes the primary — `emit_icmp` handles
/// narrow; the i64 compare becomes the `ccmp` secondary).
const SWAP_ORDER: &str = "function %f(i64, i64, i8, i8) -> i64 {
block0(v0: i64, v1: i64, v2: i8, v3: i8):
    v4 = icmp slt v0, v1
    v5 = icmp slt v2, v3
    v6 = uextend.i64 v4
    v7 = uextend.i64 v5
    v8 = band v6, v7
    return v8
}";

/// Both compares narrow — no 32/64-bit `ccmp` secondary exists in
/// either order, so the pair must decline to plain `and`.
const DECLINE_NARROW: &str = "function %f(i8, i8, i8, i8) -> i64 {
block0(v0: i8, v1: i8, v2: i8, v3: i8):
    v4 = icmp slt v0, v1
    v5 = icmp slt v2, v3
    v6 = uextend.i64 v4
    v7 = uextend.i64 v5
    v8 = band v6, v7
    return v8
}";

fn has_ccmp(a: &str) -> bool {
    a.lines()
        .any(|l| l.trim_start().starts_with("ccmp "))
}

#[test]
fn band_of_compares_fuses_for_brif() {
    let a = asm(BRIF_BAND);
    assert!(has_ccmp(&a), "want `ccmp` in:\n{a}");
    assert!(
        !a.lines().any(|l| l.trim_start().starts_with("and ")),
        "no materialized `and` left:\n{a}"
    );
}

#[test]
fn bor_of_compares_fuses_for_brif() {
    let a = asm(BRIF_BOR);
    assert!(has_ccmp(&a), "want `ccmp` in:\n{a}");
}

#[test]
fn band_of_compares_fuses_as_value() {
    let a = asm(VAL_BAND);
    assert!(has_ccmp(&a), "want `ccmp` in:\n{a}");
    assert!(
        a.lines().any(|l| l.trim_start().starts_with("cset ")),
        "fused chain ends in `cset`:\n{a}"
    );
}

#[test]
fn bor_of_compares_fuses_as_value() {
    let a = asm(VAL_BOR);
    assert!(has_ccmp(&a), "want `ccmp` in:\n{a}");
    assert!(
        a.lines().any(|l| l.trim_start().starts_with("cset ")),
        "fused chain ends in `cset`:\n{a}"
    );
}

#[test]
fn select_forms_fuse_all_modes() {
    for (name, clif) in [
        ("A && B", SEL_AND),
        ("!A && B", SEL_AND_NOT),
        ("!A || B", SEL_OR_NOT),
        ("A || B", SEL_OR),
    ] {
        let a = asm(clif);
        assert!(has_ccmp(&a), "want `ccmp` for `{name}`:\n{a}");
    }
}

#[test]
fn swapped_operand_order_still_fuses() {
    let a = asm(SWAP_ORDER);
    assert!(has_ccmp(&a), "swapped order should fuse:\n{a}");
}

#[test]
fn all_narrow_compares_decline() {
    let a = asm(DECLINE_NARROW);
    assert!(!has_ccmp(&a), "no `ccmp` when no 32/64 secondary:\n{a}");
}

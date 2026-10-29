//! aarch64 `PLIRON_TBNZ_IREDUCE=1`: `brif (band (ireduce x), 2^k)` tests bit k of
//! `x`'s own register. The `ireduce` rule materializes `uxtb`/`uxth`, which only
//! clears bits `tbnz`/`tbz` never read. The knob is read once per process, so
//! this binary sets it before its first compile.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};

fn isa() -> std::sync::Arc<dyn TargetIsa> {
    // SAFETY: every test sets the same value before any compile reads it.
    unsafe {
        std::env::set_var("PLIRON_TBNZ_IREDUCE", "1");
    }
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap()
}

fn asm(clif: &str) -> String {
    let isa = isa();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

fn count(asm: &str, mnem: &str) -> usize {
    asm.lines()
        .filter(|l| l.split_whitespace().next() == Some(mnem))
        .count()
}

#[test]
fn band_ireduce_brif_is_one_tbnz() {
    let a = asm("
function %f(i64) -> i64 apple_aarch64 {
block0(v0: i64):
    v1 = ireduce.i8 v0
    v2 = iconst.i8 1
    v3 = band v1, v2
    brif v3, block1, block2
block1:
    v4 = iconst.i64 11
    return v4
block2:
    v5 = iconst.i64 22
    return v5
}");
    assert_eq!(count(&a, "uxtb"), 0, "{a}");
    assert_eq!(count(&a, "tbnz"), 1, "{a}");
}

#[test]
fn band_ireduce_icmp_eq_is_one_tbz() {
    let a = asm("
function %f(i64) -> i64 apple_aarch64 {
block0(v0: i64):
    v1 = ireduce.i16 v0
    v2 = iconst.i16 0x80
    v3 = band v1, v2
    v6 = iconst.i16 0
    v7 = icmp eq v3, v6
    brif v7, block1, block2
block1:
    v4 = iconst.i64 11
    return v4
block2:
    v5 = iconst.i64 22
    return v5
}");
    assert_eq!(count(&a, "uxth"), 0, "{a}");
    assert_eq!(count(&a, "tbz"), 1, "{a}");
}

/// A mask with several bits is not a single-bit test: the generic path
/// (and its extension) stays.
#[test]
fn multi_bit_mask_keeps_generic_path() {
    let a = asm("
function %f(i64) -> i64 apple_aarch64 {
block0(v0: i64):
    v1 = ireduce.i8 v0
    v2 = iconst.i8 3
    v3 = band v1, v2
    brif v3, block1, block2
block1:
    v4 = iconst.i64 11
    return v4
block2:
    v5 = iconst.i64 22
    return v5
}");
    assert_eq!(count(&a, "tbnz"), 0, "{a}");
}

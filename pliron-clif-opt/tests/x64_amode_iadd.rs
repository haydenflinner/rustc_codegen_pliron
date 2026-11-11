//! x64 amode: `base + ((idx + K) << s)` folds `K<<s` into the load/store
//! displacement (`amode_imm_reg_reg_shift_iadd_shl_*`, default-on,
//! `PLIRON_X64_AMODEIADD=0` disables). Without the fold each access
//! materializes `idx+K` with a separate `lea`.

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str, gate: &str) -> String {
    unsafe { std::env::set_var("PLIRON_X64_AMODEIADD", gate) };
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

fn count(asm: &str, mnem: &str) -> usize {
    asm.lines().filter(|l| l.split_whitespace().next() == Some(mnem)).count()
}

// `p[i + K]` for a fixed K: the whole `iadd` chain is single-use, so the
// load should carry `K*4` in its displacement.
#[test]
fn fold_disp() {
    let a = asm(
        "
function %f(i64, i64) -> i32 {
block0(v0: i64, v1: i64):
    v9 = iconst.i64 2
    v2 = iadd v1, v9
    v3 = ishl v2, v9
    v4 = iadd v0, v3
    v5 = load.i32 v4
    return v5
}",
        "1",
    );
    // disp = 2<<2 = 8: one indexed load, no `lea` for `i+2`.
    assert_eq!(count(&a, "leaq"), 0, "{a}");
    assert!(a.contains("8(") || a.contains("0x8("), "{a}");
}

// The constant can sit on the `iadd` lhs and the `ishl` on the `iadd`
// lhs — same fold.
#[test]
fn fold_swapped() {
    let a = asm(
        "
function %f(i64, i64) -> i32 {
block0(v0: i64, v1: i64):
    v9 = iconst.i64 3
    v8 = iconst.i64 2
    v2 = iadd v9, v1
    v3 = ishl v2, v8
    v4 = iadd v3, v0
    v5 = load.i32 v4
    return v5
}",
        "1",
    );
    assert_eq!(count(&a, "leaq"), 0, "{a}");
    assert!(a.contains("12(") || a.contains("0xc("), "{a}");
}

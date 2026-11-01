//! aarch64 flag twins (`PLIRON_FLAGTWIN`, patch 91): a sum stored as a
//! value and tested for its carry (`mac_with_carry` in num-bigint-dig's
//! montgomery loop) computes once, as `adds d, a, b`, instead of `add`
//! plus a later `cmn a, b` (`adds xzr, a, b`).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
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

/// `*p += c; return hi + carry`.
const MAC: &str = "function %f(i64, i64, i64) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = load.i64 notrap v0
    v4 = iadd v3, v1
    store notrap v4, v0
    v5 = icmp ult v4, v3
    v6 = uextend.i64 v5
    v7 = iadd v2, v6
    return v7
}";

#[test]
fn stored_sum_and_its_carry_share_one_adds() {
    let a = asm(MAC);
    let adds: Vec<&str> = a.lines().filter(|l| l.trim_start().starts_with("adds ")).collect();
    assert!(
        adds.len() == 1 && !adds[0].contains("xzr"),
        "want one `adds d, a, b` and no flag-only recompute:\n{a}"
    );
    assert!(!a.lines().any(|l| l.trim_start().starts_with("add ")), "separate `add` left:\n{a}");
}

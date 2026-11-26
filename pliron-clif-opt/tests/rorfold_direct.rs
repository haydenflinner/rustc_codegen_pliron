//! aarch64: `PLIRON_RORFOLD_DIRECT=1` — a constant rotate folds into a
//! logic op's shifted operand (`eor x, y, ror #n`) when the rotate has at
//! most one *direct* use, even if `ValueUseState` marks it `Multiple`
//! transitively because that one consumer's result fans out (a loop's
//! `bxor` feeding both the backedge and the exit args). The fold never
//! materializes the rotate, so it is strictly fewer instructions — unlike
//! the genuinely multi-use case (rorfold_multi_use.rs), where the `ror`
//! is emitted anyway and the shifted-operand op costs a cycle on Apple
//! cores. SipHash-1-3's chunk loop (`RandomState::hash_one` on regex's
//! DFA-state cache) is exactly this shape.

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

fn shifted_ror(asm: &str) -> usize {
    asm.lines()
        .filter(|l| l.contains("ROR") || l.contains("ror #"))
        .filter(|l| matches!(l.split_whitespace().next(), Some("eor" | "and")))
        .count()
}

/// The rotate's only use is a `bxor` whose result feeds the backedge
/// argument, the exit test and the return — transitively `Multiple`,
/// but the `rotl` dies when folded: one `eor .., ror` and no `extr`.
#[test]
fn loop_carried_single_direct_use_folds() {
    unsafe { std::env::set_var("PLIRON_RORFOLD_DIRECT", "1") };
    let a = asm("
function %f(i64, i64) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64):
    v13 = iconst.i64 13
    jump block1(v0)

block1(v6: i64):
    v3 = rotl v6, v13
    v4 = bxor v3, v1
    v5 = icmp ne v4, v0
    brif v5, block1(v4), block2()

block2:
    return v4
}");
    assert_eq!(shifted_ror(&a), 1, "{a}");
    assert!(!a.lines().any(|l| l.trim_start().starts_with("extr")), "{a}");
}

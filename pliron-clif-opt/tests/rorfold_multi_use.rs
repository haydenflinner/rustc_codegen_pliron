//! aarch64: a constant rotate folds into a logic op's shifted operand
//! (`eor x, y, ror #n`) only when the rotate has one use. With other uses
//! the rotate is materialized anyway, and the shifted-operand op adds a
//! cycle of latency on Apple cores (blake3's mixing rounds, 1.39x).

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
    asm.lines().filter(|l| l.contains("ROR") || l.contains("ror #")).filter(|l| {
        matches!(l.split_whitespace().next(), Some("eor" | "and"))
    }).count()
}

/// One use: the rotate folds into the `eor`, no separate `ror`.
#[test]
fn single_use_rotate_folds() {
    let a = asm("
function %f(i32, i32) -> i32 apple_aarch64 {
block0(v0: i32, v1: i32):
    v9 = iconst.i32 8
    v2 = rotr v1, v9
    v3 = bxor v0, v2
    return v3
}");
    assert_eq!(shifted_ror(&a), 1, "{a}");
}

/// Two uses (blake3's `d = (d ^ a).rotate_right(8)`, then `c += d` and the
/// next round's `d ^ a'`): the `ror` is emitted for the `iadd`, so the
/// `eor` must use the rotated register, not re-rotate the input.
#[test]
fn multi_use_rotate_stays_separate() {
    let a = asm("
function %f(i32, i32, i32) -> i32 apple_aarch64 {
block0(v0: i32, v1: i32, v2: i32):
    v9 = iconst.i32 8
    v3 = rotr v1, v9
    v4 = iadd v2, v3
    v5 = bxor v0, v3
    v6 = iadd v4, v5
    return v6
}");
    assert_eq!(shifted_ror(&a), 0, "{a}");
}

/// One direct use but transitively `Multiple` (the `bxor` feeds the
/// backedge, the exit test and the return): `PLIRON_RORFOLD_DIRECT`
/// (default on since 2026-11-26) folds it — the standalone ror dies.
#[test]
fn transitive_multi_use_folds_by_default() {
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
}

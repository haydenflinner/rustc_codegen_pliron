//! `PLIRON_COMMONRET` (opt-in): `brif c, A, B` whose arms are each a lone
//! `return` with equal operands collapses to `jump` — the emulated-EH
//! `brif flag, return, return` on drop-free/void exits, which frees the
//! TLS chain + flag load feeding the condition.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::clifpeep;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// The `eh_check` shape from `__rust_dealloc`: flag check between a call
/// and two identical bare returns.
const EHCHECK: &str = "function %f(i64, i64, i64) {
    sig0 = (i64, i64, i64) windows_fastcall
    fn0 = %g sig0

block0(v0: i64, v1: i64, v2: i64):
    call fn0(v0, v1, v2)
    v4 = iconst.i32 1
    v5 = load.i32 notrap aligned v0
    v9 = icmp ne v5, v4
    brif v9, block2, block1

block1:
    return

block2 cold:
    return
}";

#[test]
fn eh_check_same_return_folds() {
    let mut f = parse(EHCHECK);
    let brifs: Vec<_> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Brif)
        .collect();
    let n = brifs
        .into_iter()
        .filter(|&i| clifpeep::brif_commondest(&mut f, i))
        .count();
    assert_eq!(n, 1);
    let s = f.to_string();
    assert!(s.contains("jump block"), "{s}");
    assert!(!s.contains("brif"), "{s}");
}

/// Arms returning different values stay brifs.
const DIFFRET: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 7
    brif v0, block1(v1), block2(v0)

block1(v10: i64):
    return v10

block2(v20: i64):
    return v20
}";

#[test]
fn differing_returns_untouched() {
    let mut f = parse(DIFFRET);
    let brifs: Vec<_> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Brif)
        .collect();
    let n = brifs
        .into_iter()
        .filter(|&i| clifpeep::brif_commondest(&mut f, i))
        .count();
    assert_eq!(n, 0);
    assert!(f.to_string().contains("brif v0, block1"));
}

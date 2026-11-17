//! `PLIRON_VRP_SIGNEDEQ=1`: `vrp` decides an equality from disjoint signed
//! intervals, not only unsigned ones. The shape is dragon's bignum compare
//! (`core::num::imp::bignum` `Ord::cmp` through `Iterator::cmp_by`): on the
//! not-equal exit the `Ordering` is built as `(a > b) - (a < b)` (signed
//! [-1, 1], unsigned everything: it wraps to 0xff) and tested against
//! `ControlFlow::Continue`'s niche -2, a test that never holds.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::pipeline::Analysis;

fn src(k: i64) -> String {
    format!(
        "function u0:0(i32, i32) -> i8 system_v {{
block0(v0: i32, v1: i32):
    v2 = icmp eq v0, v1
    brif v2, block1, block2

block1:
    v10 = iconst.i8 0
    return v10

block2:
    v3 = icmp ugt v0, v1
    v4 = icmp ult v0, v1
    v5 = isub v3, v4
    v6 = iconst.i8 {k}
    v8 = icmp eq v5, v6
    brif v8, block1, block3(v5)

block3(v9: i8):
    return v9
}}"
    )
}

fn brifs(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Brif)
        .count()
}

fn vrp(src: &str) -> (Function, usize) {
    unsafe { std::env::set_var("PLIRON_VRP_SIGNEDEQ", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let n = pliron_clif_opt::vrp::run(&mut f, &mut Analysis::default());
    (f, n)
}

/// The `brif` on `v8` once the test is decided: its condition resolves to
/// an `iconst`.
fn decided(f: &Function) -> bool {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).any(|i| {
        f.dfg.insts[i].opcode() == Opcode::Brif && {
            let c = f.dfg.resolve_aliases(f.dfg.inst_args(i)[0]);
            f.dfg.value_def(c).inst().is_some_and(|d| f.dfg.insts[d].opcode() == Opcode::Iconst)
        }
    })
}

/// -2 and 2 lie outside [-1, 1]: the test folds to a constant.
#[test]
fn outside_the_signed_range_folds() {
    for k in [-2, 2, -128, 127] {
        let (f, n) = vrp(&src(k));
        assert!(n >= 1 && decided(&f), "k={k}: {}", f.display());
        assert_eq!(brifs(&f), 2, "k={k}: {}", f.display());
    }
}

/// -1, 0 and 1 are possible values: nothing folds.
#[test]
fn inside_the_signed_range_stays() {
    for k in [-1, 0, 1] {
        let (f, _) = vrp(&src(k));
        assert!(!decided(&f), "k={k}: {}", f.display());
    }
}

#[test]
fn decide_uses_both_intervals() {
    use cranelift_codegen::ir::condcodes::IntCC;
    use pliron_clif_opt::ranges::{decide, Range};
    unsafe { std::env::set_var("PLIRON_VRP_SIGNEDEQ", "1") };
    let ord = Range::signed(8, -1, 1);
    let k = Range::signed(8, -2, -2);
    assert_eq!(decide(IntCC::Equal, &ord, &k), Some(false));
    assert_eq!(decide(IntCC::NotEqual, &ord, &k), Some(true));
    let m1 = Range::signed(8, -1, -1);
    assert_eq!(decide(IntCC::Equal, &ord, &m1), None);
}

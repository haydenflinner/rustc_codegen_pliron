//! `PLIRON_UREMCMP=1`: `icmp ucc (urem a, y), y` and its mirror fold — a
//! remainder is below its divisor (rustc's `h % len` bounds-checking the
//! index it just derived, regex's suffix cache). The fold is exact only
//! where deleting the compare cannot delete the `urem`'s int_divz trap:
//! the divisor proved nonzero, or the `urem` read by another use.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::pipeline::Analysis;

fn vrp(src: &str) -> (Function, usize) {
    unsafe { std::env::set_var("PLIRON_UREMCMP", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let n = pliron_clif_opt::vrp::run(&mut f, &mut Analysis::default());
    (f, n)
}

fn icmps(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Icmp)
        .count()
}

/// Divisor guarded nonzero on the edge: `urem` cannot trap, so the
/// compare folds even when it is the `urem`'s only use.
#[test]
fn uremcmp_folds_when_divisor_known_nonzero() {
    let (f, n) = vrp(
        "function u0:0(i64, i64) -> i8 system_v {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = icmp ne v1, v2
    brif v3, block1, block2

block1:
    v4 = urem v0, v1
    v5 = icmp ult v4, v1
    return v5

block2:
    v6 = iconst.i8 0
    return v6
}",
    );
    assert!(n > 0, "fold expected:\n{}", f.display());
    assert_eq!(icmps(&f), 1, "the `ne` stays, the `ult` folds:\n{}", f.display());
}

/// Divisor unconstrained, but the `urem` value has a second use — its
/// int_divz check lives on, so the compare still folds.
#[test]
fn uremcmp_folds_when_urem_has_another_use() {
    let (f, n) = vrp(
        "function u0:0(i64, i64) -> i64 system_v {
block0(v0: i64, v1: i64):
    v4 = urem v0, v1
    v5 = icmp ult v4, v1
    v6 = uextend.i64 v5
    v7 = iadd v4, v6
    return v7
}",
    );
    assert!(n > 0, "fold expected:\n{}", f.display());
    assert_eq!(icmps(&f), 0, "the `ult` folds:\n{}", f.display());
    assert!(
        f.layout
            .blocks()
            .flat_map(|b| f.layout.block_insts(b))
            .any(|i| f.dfg.insts[i].opcode() == Opcode::Urem),
        "the `urem` (and its trap) stays:\n{}",
        f.display()
    );
}

/// Divisor unconstrained and the compare is the `urem`'s only use:
/// folding would dead-code the int_divz trap — must not happen.
#[test]
fn uremcmp_keeps_a_lone_trap_check() {
    let (f, n) = vrp(
        "function u0:0(i64, i64) -> i8 system_v {
block0(v0: i64, v1: i64):
    v4 = urem v0, v1
    v5 = icmp ult v4, v1
    return v5
}",
    );
    assert_eq!(icmps(&f), 1, "the `ult` must stay:\n{}", f.display());
    assert!(
        f.layout
            .blocks()
            .flat_map(|b| f.layout.block_insts(b))
            .any(|i| f.dfg.insts[i].opcode() == Opcode::Urem),
        "the `urem` stays:\n{}",
        f.display()
    );
    let _ = n;
}

/// The mirror: `icmp ugt y, (urem a, y)` is always true.
#[test]
fn uremcmp_mirror_folds() {
    let (f, n) = vrp(
        "function u0:0(i64, i64) -> i64 system_v {
block0(v0: i64, v1: i64):
    v4 = urem v0, v1
    v5 = icmp ugt v1, v4
    v6 = uextend.i64 v5
    v7 = iadd v4, v6
    return v7
}",
    );
    assert!(n > 0, "fold expected:\n{}", f.display());
    assert_eq!(icmps(&f), 0, "the `ugt` folds:\n{}", f.display());
}

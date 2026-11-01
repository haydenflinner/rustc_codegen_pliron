//! punroll must not copy a loop past its constant trip bound: `for i in
//! 1..4` (regex-syntax's `Utf8Sequences::next`, `1..MAX_UTF8_BYTES`) got
//! an x8 region unroll that could never run, and the peel path made eight
//! copies of the simple form. Both now stop at the bound (`trip_max` sees
//! constant start and bound; peeling up to it is a full unroll). The same
//! body over a runtime bound still gets the full x8.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::pipeline::Analysis;

fn src(bound: &str) -> String {
    format!(
        "function u0:0(i64, i64) fast {{
block0(v0: i64, v1: i64):
    v2 = iconst.i64 1
    v3 = iconst.i64 4
    jump block1(v2)

block1(v5: i64):
    v10 = iconst.i64 1
    v11 = iconst.i64 3
    v12 = ishl v5, v11
    v6 = iadd v0, v12
    store.i64 notrap v5, v6
    v7 = iadd v5, v10
    v8 = icmp ult v7, {bound}
    brif v8, block1(v7), block2

block2:
    return
}}"
    )
}

fn stores(f: &Function) -> usize {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).filter(|&i| f.dfg.insts[i].opcode() == Opcode::Store).count()
}

fn unroll(src: &str) -> Function {
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::punroll::run(&mut f, &mut Analysis::default(), "t", &Default::default());
    f
}

#[test]
fn constant_three_trip_loop_copies_at_most_its_bound() {
    let src3 = src("v3");
    let before = cranelift_reader::parse_functions(&src3).unwrap().remove(0);
    let f = unroll(&src3);
    // trip_max's bound is 4 (3 trips, one spare): 4 peeled copies plus the
    // residual loop, never 8.
    assert!(stores(&f) <= 5, "{}", f.display());
    let s = pliron_clif_opt::diffcheck::check(&before, &f, &Default::default(), &pliron_clif_opt::diffcheck::Config { trials: 64, ..Default::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", f.display()));
    assert!(s.compared > 0);
}

#[test]
fn runtime_bound_still_unrolls() {
    let f = unroll(&src("v1"));
    assert!(stores(&f) >= 8, "{}", f.display());
}

//! ivcmp: a loop's `icmp eq (iadd p, d), e` with `d`, `e` invariant (or
//! constant) tests `p` against `e - d` computed once before the loop --
//! differentially checked against the interpreter.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Function, InstructionData, Opcode, ValueDef};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn run(f: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(f);
    let dt = DominatorTree::with_function(f, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(f, &cfg, &dt);
    pliron_clif_opt::ivcmp::run(f, &dt, &la)
}

/// Does an `icmp eq|ne` in `block` read an `iadd` result?
fn eq_reads_iadd(f: &Function) -> bool {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).any(|i| {
        let InstructionData::IntCompare { args, .. } = f.dfg.insts[i] else {
            return false;
        };
        args.iter().any(|&a| {
            matches!(f.dfg.value_def(a), ValueDef::Result(d, _)
                if matches!(f.dfg.insts[d], InstructionData::Binary { opcode: Opcode::Iadd, .. }))
        })
    })
}

fn same(before: &Function, after: &Function) {
    if let Err(m) = check(before, after, &Facts::default(), &Config::default()) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

/// num-bigint-dig's zip: `p < x_end` and `p + d == z_end` per step.
#[test]
fn offset_end_test_moves_out() {
    let before = parse(
        "
function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = iconst.i64 0
    jump block1(v0, v4)

block1(v5: i64, v6: i64):
    v7 = icmp ult v5, v1
    brif v7, block2, block3(v6)

block2:
    v8 = iadd v5, v2
    v9 = icmp eq v8, v3
    v10 = iconst.i64 1
    v11 = iadd v6, v10
    v12 = iconst.i64 8
    v13 = iadd v5, v12
    brif v9, block3(v6), block1(v13, v11)

block3(v14: i64):
    return v14
}",
    );
    let mut after = before.clone();
    assert_eq!(run(&mut after), 1, "{}", after.display());
    assert!(!eq_reads_iadd(&after), "{}", after.display());
    same(&before, &after);
}

/// The other side is a constant materialized inside the loop.
#[test]
fn constant_inside_loop_counts() {
    let before = parse(
        "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v4 = iconst.i64 0
    jump block1(v0, v4)

block1(v5: i64, v6: i64):
    v7 = icmp ult v5, v1
    brif v7, block2, block3(v6)

block2:
    v8 = iadd v5, v2
    v3 = iconst.i64 0
    v9 = icmp eq v8, v3
    v10 = iconst.i64 1
    v11 = iadd v6, v10
    v12 = iconst.i64 8
    v13 = iadd v5, v12
    brif v9, block3(v6), block1(v13, v11)

block3(v14: i64):
    return v14
}",
    );
    let mut after = before.clone();
    assert_eq!(run(&mut after), 1, "{}", after.display());
    same(&before, &after);
}

/// Ordered compares are not offset-invariant and stay.
#[test]
fn ordered_compare_stays() {
    let mut f = parse(
        "
function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    jump block1(v0)

block1(v5: i64):
    v8 = iadd v5, v2
    v9 = icmp ult v8, v3
    v12 = iconst.i64 8
    v13 = iadd v5, v12
    brif v9, block1(v13), block2(v5)

block2(v14: i64):
    return v14
}",
    );
    assert_eq!(run(&mut f), 0, "{}", f.display());
}

/// The sum is also the loop's pointer (parse_only's post-incremented load
/// address): the add stays, so moving the test would only add work.
#[test]
fn multi_use_sum_stays() {
    let mut f = parse(
        "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    jump block1(v0)

block1(v5: i64):
    v8 = iadd v5, v2
    v9 = icmp eq v8, v1
    v10 = uload8.i64 notrap v8
    brif v9, block2(v10), block1(v8)

block2(v14: i64):
    return v14
}",
    );
    assert_eq!(run(&mut f), 0, "{}", f.display());
}

/// The moved-to operand must be an induction variable: `(-p) + d == e`
/// would put the test on a derived value (parse_only's digit loop, where
/// later passes form a plain `p == end` compare instead).
#[test]
fn derived_operand_stays() {
    let mut f = parse(
        "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    jump block1(v0)

block1(v5: i64):
    v6 = iconst.i64 -1
    v7 = imul v5, v6
    v8 = iadd v7, v2
    v9 = icmp ne v8, v1
    v12 = iconst.i64 1
    v13 = iadd v5, v12
    brif v9, block1(v13), block2(v5)

block2(v14: i64):
    return v14
}",
    );
    assert_eq!(run(&mut f), 0, "{}", f.display());
}

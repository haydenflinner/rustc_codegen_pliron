//! joincmp: a compare on a join's parameter that every incoming edge
//! decides becomes a constant (hashbrown's `new_uninitialized`: the
//! allocation's length against the requested size). Differentially checked.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn run(src: &str) -> (Function, Function, usize) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let mut an = Analysis::default();
    let (cfg, dt) = an.cfg_dt(&after);
    let (cfg, dt) = (cfg.clone(), dt.clone());
    let k = pliron_clif_opt::joincmp::run(&mut after, &cfg, &dt);
    (before, after, k)
}

fn checked(before: &Function, after: &Function) {
    let cfg = Config { trials: 256, ..Config::default() };
    if let Err(m) = check(before, after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

fn has(f: &Function, s: &str) -> bool {
    f.display().to_string().contains(s)
}

/// `size == 0` passes `(8, 0)`, the allocation passes `(ptr, size)`: the
/// join's `len != size` is false on both edges (the first by the dominating
/// `size == 0`, the second because the argument is `size` itself).
const ALLOC: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = icmp eq v0, v2
    v4 = iconst.i64 8
    brif v3, block3(v4, v2), block1
block1:
    v5 = iadd v1, v0
    v6 = icmp eq v5, v2
    brif v6, block2, block3(v5, v0)
block2:
    v7 = iconst.i64 -1
    return v7
block3(v10: i64, v11: i64):
    v12 = icmp ne v11, v0
    brif v12, block4, block5
block4:
    v13 = udiv v11, v4
    return v13
block5:
    return v10
}";

#[test]
fn alloc_length_test_folds() {
    let (before, after, k) = run(ALLOC);
    assert_eq!(k, 1, "{}", after.display());
    assert!(!has(&after, "udiv"), "the recompute arm is gone:\n{}", after.display());
    checked(&before, &after);
}

/// One edge passes a value the join's compare cannot decide: it stays.
#[test]
fn undecided_edge_keeps_compare() {
    let (before, after, k) = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = icmp eq v0, v2
    v4 = iconst.i64 8
    brif v3, block3(v4, v2), block1
block1:
    brif v1, block2, block3(v4, v1)
block2:
    v7 = iconst.i64 -1
    return v7
block3(v10: i64, v11: i64):
    v12 = icmp ne v11, v0
    brif v12, block4, block5
block4:
    v13 = udiv v11, v4
    return v13
block5:
    return v10
}");
    assert_eq!(k, 0, "{}", after.display());
    checked(&before, &after);
}

/// Edges that decide the compare differently keep it.
#[test]
fn disagreeing_edges_keep_compare() {
    let (before, after, k) = run("function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 3
    v2 = iconst.i64 5
    brif v0, block1(v1), block1(v2)
block1(v10: i64):
    v11 = iconst.i64 4
    v12 = icmp ult v10, v11
    brif v12, block2, block3
block2:
    return v10
block3:
    v13 = iconst.i64 0
    return v13
}");
    assert_eq!(k, 0, "{}", after.display());
    checked(&before, &after);
}

/// Constants on every edge, signed compare: decided without facts.
#[test]
fn constant_edges_fold() {
    let (before, after, k) = run("function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 -3
    v2 = iconst.i64 -5
    brif v0, block1(v1), block1(v2)
block1(v10: i64):
    v11 = iconst.i64 0
    v12 = icmp slt v10, v11
    brif v12, block2, block3
block2:
    return v10
block3:
    v13 = iconst.i64 7
    return v13
}");
    assert_eq!(k, 1, "{}", after.display());
    checked(&before, &after);
}

/// A loop header: the back edge passes `i + 1` (undecided), so a bound
/// test on the header parameter stays.
#[test]
fn loop_header_bound_stays() {
    let (before, after, k) = run("function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 0
    jump block1(v1)
block1(v10: i64):
    v11 = icmp ult v10, v0
    brif v11, block2, block3
block2:
    v13 = iconst.i64 1
    v12 = iadd v10, v13
    jump block1(v12)
block3:
    return v10
}");
    assert_eq!(k, 0, "{}", after.display());
    checked(&before, &after);
}

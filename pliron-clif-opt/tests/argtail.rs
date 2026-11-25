//! argtail: a back-edge argument computed inside the loop is cloned to
//! the edge tail — the bottom of a `jump` predecessor, or a fresh edge
//! block on a `brif` edge — so its live range abuts the loop parameter's
//! instead of overlapping it and regalloc2 can merge the two bundles.
//! Only fires when a slot-bundle member's range actually reaches into
//! the argument's (otherwise the merge already succeeds and the copy —
//! plus a `brif` edge block's `b` — is pure waste). Differentially
//! checked against the interpreter.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn run(src: &str) -> (Function, Function, usize) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::argtail::run(&mut after, &mut Analysis::default());
    (before, after, k)
}

fn checked(before: &Function, after: &Function) {
    let mut cfg = Config::default();
    cfg.trials = 64;
    if let Err(m) = check(before, after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

/// `jump` predecessor: `v9`'s range spans `v11`'s use of the parameter
/// `v4` — sink the def under the load so the two abut. `v12`'s slot has
/// no conflict (`v5` is only read at its own def), so it stays.
const JUMP: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0, v1)
block1(v4: i64, v5: i64):
    v10 = icmp ne v4, v1
    brif v10, block2, block3(v5)
block2:
    v9 = iadd v5, v4
    v11 = uload8.i64 notrap v4
    v12 = iadd v5, v5
    jump block1(v9, v12)
block3(v13: i64):
    return v13
}";

#[test]
fn jump_pred_sinks_the_arg() {
    let (before, after, k) = run(JUMP);
    assert_eq!(k, 1, "{}", after.display());
    let s = after.display().to_string();
    // The recomputed arg sits at block2's bottom, right above the jump.
    let tail = s.split("block2:").nth(1).unwrap();
    assert!(tail.contains("iadd.i64 v5, v4") && tail.contains("jump block1"), "{}", s);
    checked(&before, &after);
}

/// `brif` back edge: the other call keeps the parameter `v4` live at
/// block2's tail — a fresh edge block carries the recomputed arg.
#[test]
fn brif_edge_gets_a_tail_block() {
    let (before, after, k) = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0, v1)
block1(v4: i64, v5: i64):
    v6 = uload8.i64 notrap v4
    v10 = icmp ne v6, v1
    brif v10, block3(v5), block2
block2:
    v9 = iadd v5, v4
    v11 = icmp ne v4, v1
    brif v11, block1(v9, v5), block3(v4)
block3(v13: i64):
    return v13
}");
    assert_eq!(k, 1, "{}", after.display());
    let s = after.display().to_string();
    // A new block holds the recomputed arg and jumps to the header.
    let tail = s.split("block4:").nth(1).unwrap();
    assert!(tail.contains("iadd.i64 v5, v4") && tail.contains("jump block1"), "{}", s);
    checked(&before, &after);
}

/// The LLVM shape already: the parameter's only use is the arg's own
/// def — the ranges abut, the merge already works, leave it.
#[test]
fn abutting_arg_is_left_alone() {
    let (_, after, k) = run("function %f(i64) -> i64 {
block0(v0: i64):
    jump block1(v0)
block1(v4: i64):
    v10 = icmp ne v4, v0
    brif v10, block2, block3(v4)
block2:
    v9 = iadd v4, v0
    jump block1(v9)
block3(v13: i64):
    return v13
}");
    assert_eq!(k, 0, "{}", after.display());
}

/// The parameter rides the same call (`v4` is passed for slot 1): its
/// range reaches the tail wherever the arg's def sits — the sink cannot
/// remove the conflict, leave the arg.
#[test]
fn param_riding_the_call_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0, v1)
block1(v4: i64, v5: i64):
    v10 = icmp ne v4, v1
    brif v10, block2, block3(v5)
block2:
    v9 = iadd v5, v4
    v11 = uload8.i64 notrap v4
    jump block1(v9, v4)
block3(v13: i64):
    return v13
}");
    assert_eq!(k, 0, "{}", after.display());
}

/// An arg that also has an ordinary instruction use keeps its shared
/// def.
#[test]
fn arg_with_other_uses_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0, v1)
block1(v4: i64, v5: i64):
    v10 = icmp ne v4, v1
    brif v10, block2, block3(v5)
block2:
    v9 = iadd v5, v4
    v11 = icmp ne v9, v1
    v12 = uload8.i64 notrap v4
    jump block1(v9, v5)
block3(v13: i64):
    return v13
}");
    assert_eq!(k, 0, "{}", after.display());
}

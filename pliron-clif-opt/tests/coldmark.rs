//! `coldmark::doomed`: the blocks from which no path reaches a `return` —
//! a loop on the way to a trap is doomed, a cold-looking normal exit is not,
//! and a function with no exit at all marks nothing.

use cranelift_codegen::ir::{Block, Function};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn doomed(src: &str) -> Vec<u32> {
    let mut v: Vec<u32> = pliron_clif_opt::coldmark::doomed(&parse(src)).iter().map(|b: &Block| b.as_u32()).collect();
    v.sort();
    v
}

/// `if x == 0 { for .. { fmt } panic }`: the loop and the trap are doomed,
/// the normal path is not.
#[test]
fn loop_before_trap() {
    let d = doomed(
        "function %f(i64) -> i64 {
block0(v0: i64):
    brif v0, block4, block1
block1:
    v1 = iconst.i64 0
    jump block2(v1)
block2(v2: i64):
    v5 = iconst.i64 1
    v6 = iconst.i64 10
    v3 = iadd v2, v5
    v4 = icmp ult v3, v6
    brif v4, block2(v3), block3
block3:
    trap user1
block4:
    return v0
}",
    );
    assert_eq!(d, vec![1, 2, 3]);
}

/// A search loop's not-found edge returns: nothing on it is doomed, even
/// though the loop has a trapping side exit.
#[test]
fn live_exit_is_not_doomed() {
    let d = doomed(
        "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)
block1(v2: i64):
    v3 = icmp eq v2, v1
    brif v3, block3, block2
block2:
    v4 = load.i8 notrap v2
    brif v4, block4, block5
block4:
    trap user1
block5:
    v6 = iconst.i64 1
    v5 = iadd v2, v6
    jump block1(v5)
block3:
    return v2
}",
    );
    assert_eq!(d, vec![4]);
}

/// No `return` anywhere (`-> !`, a server loop): nothing is marked.
#[test]
fn no_exit_marks_nothing() {
    let d = doomed(
        "function %f(i64) {
block0(v0: i64):
    jump block1
block1:
    brif v0, block2, block1
block2:
    trap user1
}",
    );
    assert!(d.is_empty(), "{d:?}");
}

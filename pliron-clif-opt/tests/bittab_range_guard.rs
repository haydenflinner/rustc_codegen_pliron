//! `bittab` folded a `switchInt`-lowered match tree whose sign-crossing
//! `sle` interval list arrived unsorted from `signed_to_unsigned`
//! (`[(neg,max), (0,k)]` instead of `[(0,k), (neg,max)]`). `meet`
//! merge-walks both lists assuming ascending order, so the `1..=3` leaf
//! edge intersected to empty, was dropped as dead, and the tree folded to
//! a 2-leaf mask that routed `x = 1` to the wildcard arm (3, not 2).
//!
//! Found via rustc's `tests/ui/match/overeager-sub-match-pruning-13027.rs`
//! at -O0: `match x+1 { 100 => 0, 0..=2 if false => 1, 1..=3 => 2, _ => 3 }`
//! returned 3 for every signed-int `x + 1 == 1`.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::bittab;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const TREE: &str = "function %f(i32) -> i32 {
    block0(v9: i32):
        v11 = iconst.i32 100
        v12 = icmp.i32 eq v9, v11
        brif v12, block3, block4

    block4:
        v13 = iconst.i32 0
        v14 = icmp.i32 sle v13, v9
        brif v14, block6, block6

    block6:
        v15 = iconst.i32 1
        v16 = icmp.i32 sle v15, v9
        brif v16, block9, block8

    block9:
        v17 = iconst.i32 3
        v18 = icmp.i32 sle v9, v17
        brif v18, block10, block8

    block8:
        v19 = iconst.i32 3
        return v19

    block10:
        v20 = iconst.i32 2
        return v20

    block3:
        v21 = iconst.i32 0
        return v21
    }";

#[test]
fn sign_crossing_range_keeps_sorted_isets() {
    let before = parse(TREE);
    let mut after = before.clone();
    let n = bittab::run(&mut after);
    assert!(n > 0, "the tree should still fold");
    check(
        &before,
        &after,
        &Facts::default(),
        &Config {
            trials: 256,
            ..Config::default()
        },
    )
    .unwrap();
}

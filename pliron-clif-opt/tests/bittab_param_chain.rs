//! `bittab` follows the tested value through single-predecessor block
//! params: jump threading leaves serde_json's whitespace skip as
//! `brif (d == 0), hit, next(d)` with `next(d')` testing `d' in {1, 4, 23}`
//! (`d = x - 9`). Rooting the tree at `next` left the `d == 0` test in
//! front of the bitmask on every byte; the whole tree must fold into one
//! mask including bit 0. Differentially checked.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Function, InstructionData};
use pliron_clif_opt::bittab;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i8) -> i32 {
    block0(v0: i8):
        v1 = iconst.i8 -9
        v2 = iadd v0, v1
        v3 = uextend.i32 v2
        v4 = iconst.i32 0
        v5 = icmp eq v3, v4
        brif v5, block9, block1(v3)

    block1(v10: i32):
        v11 = iconst.i32 1
        v12 = icmp eq v10, v11
        brif v12, block9, block2

    block2:
        v13 = iconst.i32 4
        v14 = icmp eq v10, v13
        brif v14, block9, block3

    block3:
        v15 = iconst.i32 23
        v16 = icmp eq v10, v15
        brif v16, block9, block8

    block8:
        v20 = iconst.i32 0
        return v20

    block9:
        v21 = iconst.i32 1
        return v21
    }";

#[test]
fn param_chain_folds_whole_tree() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after: Function = before.clone();
    assert!(bittab::run(&mut after) > 0, "the tree should fold");
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags).unwrap();
    // No member test left: `d == 0` joined the mask (bit 0) instead of
    // staying in front of it.
    let eqs = after
        .layout
        .blocks()
        .flat_map(|b| after.layout.block_insts(b))
        .filter(|&i| matches!(after.dfg.insts[i], InstructionData::IntCompare { cond: IntCC::Equal, .. }))
        .count();
    assert_eq!(eqs, 0, "{}", after.display());
    let s = check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() }).unwrap();
    assert!(s.compared > 0);
}

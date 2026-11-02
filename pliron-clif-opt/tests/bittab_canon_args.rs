//! `bittab` compares leaf-edge args through single-predecessor block
//! params (`PLIRON_BITTAB_CANON`). Rebinding leaves serde_json's
//! whitespace skip passing the root's `i + 1` on the first hit edge and a
//! test block's param (fed that same value) on the others; keyed by raw
//! args those were three different outcomes and the tree never folded.
//! Differentially checked.

use cranelift_codegen::ir::{Function, InstructionData};
use pliron_clif_opt::bittab;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i8, i64) -> i64 {
    block0(v0: i8, v1: i64):
        v2 = iconst.i8 -9
        v3 = iadd v0, v2
        v4 = iconst.i8 2
        v5 = icmp ult v3, v4
        v6 = iconst.i64 1
        v7 = iadd v1, v6
        brif v5, block9(v7), block1(v7)

    block1(v10: i64):
        v11 = iconst.i8 13
        v12 = icmp eq v0, v11
        brif v12, block9(v10), block2

    block2:
        v13 = iconst.i8 32
        v14 = icmp eq v0, v13
        brif v14, block9(v10), block8

    block8:
        v20 = iconst.i64 -1
        return v20

    block9(v30: i64):
        return v30
}";

fn brifs(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::Brif { .. }))
        .count()
}

#[test]
fn hit_edges_with_forwarded_args_fold() {
    // Opt-in; this test binary holds only this test.
    unsafe { std::env::set_var("PLIRON_BITTAB_CANON", "1") };
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    let k = bittab::run(&mut after);
    assert_eq!(k, 1, "not folded:\n{}", after.display());
    assert!(brifs(&after) <= 2, "chain kept:\n{}", after.display());
    if let Err(m) = check(&before, &after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}

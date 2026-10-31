//! `bittab` explores only glue jumps and tests of the tree's own value.
//! It used to follow the miss path into tests of other values and give up
//! once eight candidates were collected: serde_json's whitespace skip
//! (`x - 9 in {0, 1, 4, 23}`) is followed by the parser's dispatch on the
//! raw byte, and stayed a compare chain. Differentially checked.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::bittab;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v2 = iconst.i32 0
        v3 = icmp eq v0, v2
        brif v3, block20, block1
    block1:
        v4 = iconst.i32 1
        v5 = icmp eq v0, v4
        brif v5, block20, block2
    block2:
        v6 = iconst.i32 4
        v7 = icmp eq v0, v6
        brif v7, block20, block3
    block3:
        v8 = iconst.i32 23
        v9 = icmp eq v0, v8
        brif v9, block20, block4
    block4:
        v10 = iconst.i32 100
        v11 = icmp eq v1, v10
        brif v11, block21, block5
    block5:
        v12 = iconst.i32 101
        v13 = icmp eq v1, v12
        brif v13, block21, block6
    block6:
        v14 = iconst.i32 102
        v15 = icmp eq v1, v14
        brif v15, block21, block7
    block7:
        v16 = iconst.i32 103
        v17 = icmp eq v1, v16
        brif v17, block21, block8
    block8:
        v18 = iconst.i32 104
        v19 = icmp eq v1, v18
        brif v19, block21, block9
    block9:
        v20 = iconst.i32 105
        v21 = icmp eq v1, v20
        brif v21, block21, block10
    block10:
        v22 = iconst.i32 106
        v23 = icmp eq v1, v22
        brif v23, block21, block11
    block11:
        v24 = iconst.i32 107
        v25 = icmp eq v1, v24
        brif v25, block21, block12
    block12:
        v26 = iconst.i32 108
        v27 = icmp eq v1, v26
        brif v27, block21, block22
    block20:
        v28 = iconst.i32 1
        return v28
    block21:
        v29 = iconst.i32 2
        return v29
    block22:
        v30 = iconst.i32 3
        return v30
}";

fn icmps_on_v0(f: &Function) -> usize {
    let v0 = f.dfg.block_params(f.layout.entry_block().unwrap())[0];
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Icmp && f.dfg.inst_args(i).contains(&v0))
        .count()
}

#[test]
fn membership_folds_despite_long_miss_path() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    bittab::run(&mut after);
    assert!(icmps_on_v0(&after) <= 1, "membership chain kept:\n{}", after.display());
    if let Err(m) = check(&before, &after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}

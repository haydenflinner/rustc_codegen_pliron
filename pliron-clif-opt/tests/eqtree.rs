//! `eqtree`: a chain of `icmp eq x, K` tests on one value becomes a
//! balanced search tree. Checked against the interpreter on every `i8`
//! input (the constants include negatives, so the unsigned order of the
//! tree differs from their signed order), with distinct targets per case
//! and a repeated constant that must keep its first hit.

use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i8) -> i32 {
block0(v0: i8):
    v1 = iconst.i8 3
    v2 = icmp eq v0, v1
    brif v2, block20, block1
block1:
    v3 = iconst.i8 -7
    v4 = icmp eq v0, v3
    brif v4, block21, block2
block2:
    v5 = iconst.i8 100
    v6 = icmp ne v0, v5
    brif v6, block3, block22
block3:
    v7 = iconst.i8 3
    v8 = icmp eq v0, v7
    brif v8, block23, block4
block4:
    v9 = iconst.i8 -128
    v10 = icmp eq v0, v9
    brif v10, block24, block5
block5:
    v11 = iconst.i8 42
    v12 = icmp eq v0, v11
    brif v12, block20, block6
block6:
    v13 = iconst.i8 0
    v14 = icmp eq v0, v13
    brif v14, block25, block7
block7:
    v15 = iconst.i8 127
    v16 = icmp eq v0, v15
    brif v16, block26, block8
block8:
    v17 = iconst.i8 9
    v18 = icmp eq v0, v17
    brif v18, block21, block30
block20:
    v40 = iconst.i32 20
    return v40
block21:
    v41 = iconst.i32 21
    return v41
block22:
    v42 = iconst.i32 22
    return v42
block23:
    v43 = iconst.i32 23
    return v43
block24:
    v44 = iconst.i32 24
    return v44
block25:
    v45 = iconst.i32 25
    return v45
block26:
    v46 = iconst.i32 26
    return v46
block30:
    v47 = iconst.i32 30
    return v47
}";

#[test]
fn chain_becomes_tree_and_agrees() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    let n = pliron_clif_opt::eqtree::run(&mut after);
    assert_eq!(n, 1, "{}", after.display());
    let ult = after
        .layout
        .blocks()
        .flat_map(|b| after.layout.block_insts(b))
        .filter(|&i| after.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Icmp && format!("{}", after.dfg.display_inst(i)).contains("ult"))
        .count();
    assert!(ult >= 1, "no search split\n{}", after.display());
    let cfg = Config { trials: 3000, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

/// Regression: a value defined in a chain block and used in a block the
/// rewrite leaves reachable (here `block21`, `block25` and the default
/// `block30` re-use their test block's compare and constant) must keep
/// the pass from deleting its def. The rewrite used to leave those uses
/// pointing at detached instructions ("defined by inst which has no
/// block" / "non-dominating inst" in the verifier, an ICE on
/// `codegen_intrinsic_call`).
const ESCAPE: &str = "function %f(i8) -> i32 {
block0(v0: i8):
    v1 = iconst.i8 3
    v2 = icmp eq v0, v1
    brif v2, block20, block1
block1:
    v3 = iconst.i8 -7
    v4 = icmp eq v0, v3
    brif v4, block21, block2
block2:
    v5 = iconst.i8 100
    v6 = icmp eq v0, v5
    brif v6, block22, block3
block3:
    v7 = iconst.i8 17
    v8 = icmp eq v0, v7
    brif v8, block23, block4
block4:
    v9 = iconst.i8 -128
    v10 = icmp eq v0, v9
    brif v10, block24, block5
block5:
    v11 = iconst.i8 42
    v12 = icmp eq v0, v11
    brif v12, block25, block6
block6:
    v13 = iconst.i8 0
    v14 = icmp eq v0, v13
    brif v14, block26, block30
block20:
    v40 = iconst.i32 20
    return v40
block21:
    v50 = select.i8 v4, v3, v0
    v51 = uextend.i32 v50
    return v51
block22:
    v42 = iconst.i32 22
    return v42
block23:
    v43 = iconst.i32 23
    return v43
block24:
    v44 = iconst.i32 24
    return v44
block25:
    v52 = select.i8 v12, v11, v0
    v53 = uextend.i32 v52
    return v53
block26:
    v46 = iconst.i32 26
    return v46
block30:
    v54 = select.i8 v14, v13, v0
    v55 = uextend.i32 v54
    return v55
}";

#[test]
fn chain_value_used_outside_its_block_stays_verified() {
    let before = cranelift_reader::parse_functions(ESCAPE).unwrap().remove(0);
    let mut after = before.clone();
    pliron_clif_opt::eqtree::run(&mut after);
    let flags =
        cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags)
        .unwrap_or_else(|e| panic!("{e}\n{}", after.display()));
    let cfg = Config { trials: 3000, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

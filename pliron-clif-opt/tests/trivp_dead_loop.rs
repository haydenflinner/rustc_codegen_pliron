//! `remove_trivial_params` on an unreachable loop: the header param's only
//! incoming value is the latch's `iadd param, 16`, and the dominator tree
//! (unreachable blocks keep default preorder numbers) said the latch
//! dominates the header — so the param became an alias of a value defined
//! from itself. That def cycle hung `clifpeep::lin_flat` (rustc UI
//! `custom_test_frameworks/test-case-on-use-item-129765.rs`, `--test`).
//! `DominatorTree::try_dominates` now requires both blocks be reachable.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Value;

#[test]
fn dead_loop_header_param_is_not_aliased_to_its_latch() {
    let src = "function %f() {
    block0:
        jump block5
    block3(v6: i64):
        v8 = iconst.i64 8
        v13 = icmp eq v6, v8
        brif v13, block5, block4
    block4:
        v14 = iconst.i64 16
        v15 = iadd v6, v14
        jump block3(v15)
    block5:
        return
    }";
    let mut func = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let cfg = ControlFlowGraph::with_function(&func);
    let dt = DominatorTree::with_function(&func, &cfg);
    let rm = pliron_clif_opt::jumpthread::remove_trivial_params(&mut func, &cfg, &dt);
    let v6 = Value::from_u32(6);
    assert!(rm.iter().all(|&(p, _)| p != v6), "aliased {rm:?}");
    assert_eq!(func.dfg.resolve_aliases(v6), v6);
}

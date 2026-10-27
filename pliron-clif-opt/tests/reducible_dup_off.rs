//! The hazard `PLIRON_REDUCIBLE` guards against, shown on the default
//! path: `dispatch_dup` clones the loop header into its `jump` pred and the
//! CFG becomes irreducible (see reducible_dup.rs for the same input with the
//! flag on). If this stops reproducing, the positive test proves nothing.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Function;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const SRC: &str = "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v9 = iconst.i32 0
        v8 = iconst.i32 1
        brif v0, block1, block3(v0)

    block1:
        jump block3(v1)

    block3(v2: i32):
        v3 = icmp ne v2, v9
        brif v3, block4(v2), block5(v2)

    block4(v4: i32):
        v5 = isub v4, v8
        brif v5, block3(v5), block5(v5)

    block5(v6: i32):
        return v6
    }";

#[test]
fn dispatch_dup_without_the_flag_makes_a_side_entrance() {
    unsafe { std::env::remove_var("PLIRON_REDUCIBLE") };
    assert!(!pliron_clif_opt::jumpthread::keep_reducible());
    let mut f = parse(SRC);
    let cfg = ControlFlowGraph::with_function(&f);
    let dom = DominatorTree::with_function(&f, &cfg);
    let n = pliron_clif_opt::jumpthread::dispatch_dup(&mut f, &cfg, &dom);
    assert!(n >= 1, "{}", f.display());
    assert!(pliron_clif_opt::taildup::irreducible(&f), "{}", f.display());
}

//! `PLIRON_REDUCIBLE=1`: `dispatch_dup` and `taildup` must not clone a loop
//! header into one of its outside `jump` predecessors while another outside
//! edge still enters at the header — the clone's direct edge into the loop
//! body is a side entrance and the CFG turns irreducible. waffle's wasm
//! backend then duplicates blocks (docs/history/2026-10-27-wasm-size.md).

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Function;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

// block3 is a loop header (latch block4) and a small pure dispatch block;
// block1 is a `jump` pred outside the loop, block0 enters it by `brif`.
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
fn dispatch_dup_keeps_the_cfg_reducible() {
    unsafe { std::env::set_var("PLIRON_REDUCIBLE", "1") };
    assert!(pliron_clif_opt::jumpthread::keep_reducible());
    let mut f = parse(SRC);
    assert!(!pliron_clif_opt::taildup::irreducible(&f));
    let cfg = ControlFlowGraph::with_function(&f);
    let dom = DominatorTree::with_function(&f, &cfg);
    // The latch (block4) is a brif pred, not a candidate; the outside `jump`
    // pred (block1) must not get the copy.
    let n = pliron_clif_opt::jumpthread::dispatch_dup(&mut f, &cfg, &dom);
    assert!(
        !pliron_clif_opt::taildup::irreducible(&f),
        "dispatch_dup ({n} clones) made it irreducible:\n{}",
        f.display()
    );
    let b1 = f.layout.blocks().nth(1).unwrap();
    let t = f.layout.last_inst(b1).unwrap();
    assert!(
        matches!(f.dfg.insts[t], cranelift_codegen::ir::InstructionData::Jump { .. }),
        "block1 got the clone:\n{}",
        f.display()
    );
    let n = pliron_clif_opt::taildup::run(&mut f);
    assert!(
        !pliron_clif_opt::taildup::irreducible(&f),
        "taildup ({n} clones) made it irreducible:\n{}",
        f.display()
    );
}

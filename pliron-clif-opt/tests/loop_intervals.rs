//! Vendored cranelift patch 45: `LoopAnalysis::is_in_loop` answers via
//! Euler-tour intervals over the loop forest instead of walking the
//! parent chain. Check it against the original algorithm on a function
//! with nested loops, sibling loops, and blocks outside every loop.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// The upstream algorithm: `b` is in `lp` iff `lp` is on
/// `innermost_loop(b)`'s parent chain.
fn slow_is_in_loop(la: &LoopAnalysis, b: Block, lp: Loop) -> bool {
    let mut finger = la.innermost_loop(b);
    while let Some(l) = finger {
        if l == lp {
            return true;
        }
        finger = la.loop_parent(l);
    }
    false
}

fn check_all(func: &Function) {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    for lp in la.loops() {
        for b in func.layout.blocks() {
            assert_eq!(
                la.is_in_loop(b, lp),
                slow_is_in_loop(&la, b, lp),
                "is_in_loop({b}, {lp})"
            );
        }
    }
}

#[test]
fn nested_and_sibling_loops() {
    // block1/block2/block4 are loop headers; L1 = {block1, block2,
    // block3, block4, block5, block6, block7} nests L2 = {block2} and
    // L3 = {block4}; block10 is a disjoint sibling loop L4; block0,
    // block8, block9 are loop-free.
    let func = parse(
        "function %f(i32) {
        block0(v0: i32):
            brif v0, block1, block9

        block1:
            brif v0, block2, block3

        block2:
            brif v0, block2, block3

        block3:
            brif v0, block4, block6

        block4:
            brif v0, block4, block5

        block5:
            jump block7

        block6:
            jump block7

        block7:
            brif v0, block1, block8

        block8:
            brif v0, block10, block9

        block9:
            return

        block10:
            brif v0, block10, block9
        }",
    );
    check_all(&func);
}

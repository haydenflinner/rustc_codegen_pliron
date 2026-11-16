//! `dispatch_dup` copies a small dispatch block's test into a `jump`
//! predecessor. When the dispatch heads a loop, the predecessor is the
//! loop's latch and both arms stay inside the loop (`loop { match x { .. } }`),
//! the latch then branches to both arms itself and the cycle it closes is
//! entered at each arm from the old header: irreducible. Cranelift does
//! not mind; waffle's wasm backend repairs it by threading every live
//! value through block parameters (the dsa program's `Compiler::compile`:
//! 305 KB of wasm instead of 48, 86% of it copies on branch edges).
//! `side_enters` refuses the copy under `PLIRON_REDUCIBLE` (default on);
//! the header whose other arm leaves the loop is still copied.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Function;
use pliron_clif_opt::{jumpthread, taildup};

/// `block1` is the loop header and the dispatch; `block4` the latch.
/// `ARM` is where the header's false edge goes.
fn src(arm: &str) -> String {
    format!(
        "function %f(i64, i64) -> i64 {{
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v10 = iconst.i64 1
    jump block1(v2)
block1(v3: i64):
    v4 = band v3, v10
    brif v4, block2(v3), {arm}(v3)
block2(v11: i64):
    v5 = iadd v11, v0
    v6 = icmp ult v5, v1
    brif v6, block4(v5), block5(v5)
block3(v12: i64):
    v7 = iadd v12, v10
    v20 = icmp ult v7, v1
    brif v20, block4(v7), block5(v7)
block4(v8: i64):
    jump block1(v8)
block5(v9: i64):
    return v9
}}"
    )
}

fn run(text: &str) -> (Function, usize) {
    let mut f = cranelift_reader::parse_functions(text).unwrap().remove(0);
    let cfg = ControlFlowGraph::with_function(&f);
    let dom = DominatorTree::with_function(&f, &cfg);
    let n = jumpthread::dispatch_dup(&mut f, &cfg, &dom);
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(&f, &flags) {
        panic!("dispatch_dup produced invalid IR: {e}\n{}", f.display());
    }
    (f, n)
}

#[test]
fn dispatch_header_with_both_arms_in_the_loop_is_not_copied_into_the_latch() {
    let (f, n) = run(&src("block3"));
    assert!(!taildup::irreducible(&f), "the loop has two entries now ({n} copies):\n{}", f.display());
}

/// The exit-test header (one arm leaves the loop) is the shape the
/// transform exists for: the copy rotates the loop and stays reducible.
#[test]
fn exit_test_header_is_still_copied() {
    let (f, n) = run(&src("block5"));
    let latch = cranelift_codegen::ir::Block::from_u32(4);
    let term = f.layout.last_inst(latch).unwrap();
    assert!(
        n >= 1 && f.dfg.insts[term].opcode() == cranelift_codegen::ir::Opcode::Brif,
        "the latch did not take the header's test:\n{}",
        f.display()
    );
    assert!(!taildup::irreducible(&f), "{}", f.display());
}

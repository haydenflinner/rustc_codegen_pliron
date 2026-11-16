//! `black_box` is not a value. Two executions give two opaque results,
//! so the CSE passes must not number it: `dgvn` once merged the barriers
//! of unrolled loop iterations, and with them everything computed from
//! them (`a += black_box(7) * black_box(6)` kept one multiply for the
//! whole loop).

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Opcode;

const SRC: &str = "function u0:0(i64) -> i64 fast {
block0(v0: i64):
    v1 = black_box v0
    v2 = black_box v0
    v3 = imul v1, v2
    brif v0, block1, block2

block1:
    v4 = black_box v0
    v5 = black_box v0
    v6 = imul v4, v5
    v7 = iadd v3, v6
    return v7

block2:
    return v3
}";

fn count(f: &cranelift_codegen::ir::Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

#[test]
fn dgvn_and_lcse_keep_every_barrier() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    assert_eq!(count(&f, Opcode::BlackBox), 4);
    let cfg = ControlFlowGraph::with_function(&f);
    let dt = DominatorTree::with_function(&f, &cfg);
    pliron_clif_opt::clifpeep::dgvn(&mut f, &dt);
    assert_eq!(count(&f, Opcode::BlackBox), 4, "dgvn merged barriers:\n{}", f.display());
    assert_eq!(count(&f, Opcode::Imul), 2, "dgvn merged what the barriers feed:\n{}", f.display());
    pliron_clif_opt::clifpeep::lcse(&mut f);
    assert_eq!(count(&f, Opcode::BlackBox), 4, "lcse merged barriers:\n{}", f.display());
    assert_eq!(count(&f, Opcode::Imul), 2, "lcse merged what the barriers feed:\n{}", f.display());
}

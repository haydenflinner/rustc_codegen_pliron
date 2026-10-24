//! Load PRE must not speculate a plain `notrap` load onto a critical edge.
//! `edge_pre` made a partially available load fully available by copying
//! it to the end of each predecessor that lacked it. When that
//! predecessor's terminator can also branch elsewhere, the copy runs on
//! that other path too, where the address may be invalid. btree's
//! `search_tree_for_bifurcation` loads a `Bound` key through a pointer
//! that is null for `Unbounded`, and the copy sat before a `br_table`
//! whose `Unbounded` arm never loaded: wasmtime's GC free list segfaulted
//! in `BTreeMap::range` (sightglass `pca_metrics`, 2026-10-24).
//!
//! Here block3's load of `[v0]` is available from block1 but not from
//! block2, whose `br_table` also leaves for block4 (taken when v0 is
//! null). A load copied into block2 would read `[v0]` on that path too.

use cranelift_codegen::ir::{Block, Function, Opcode};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn src(flags: &str) -> String {
    format!(
        "function %t(i64, i32, i32) -> i32 {{
block0(v0: i64, v1: i32, v2: i32):
    brif v2, block1, block2
block1:
    v3 = load.i32 {flags} v0
    v8 = iconst.i32 7
    v4 = icmp eq v3, v8
    brif v4, block5, block3
block2:
    br_table v1, block3, [block3, block4]
block3:
    v5 = load.i32 {flags} v0
    return v5
block4:
    v6 = iconst.i32 0
    return v6
block5:
    v7 = iconst.i32 1
    return v7
}}"
    )
}

/// Loads in block2 after running loadfwd on `src(flags)`.
fn loads_in_block2(flags: &str) -> usize {
    let mut f = parse(&src(flags));
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    );
    let b2 = Block::from_u32(2);
    f.layout
        .block_insts(b2)
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

#[test]
fn notrap_load_not_copied_onto_critical_edge() {
    assert_eq!(loads_in_block2("notrap"), 0, "a notrap load was speculated before block2's br_table");
}

/// A `can_move` load is dereferenceable wherever its operand is defined,
/// so PRE may still place it there (and does: the redundancy is real).
#[test]
fn can_move_load_still_pre() {
    assert_eq!(loads_in_block2("notrap can_move"), 1);
}

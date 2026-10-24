//! slotprom must not promote a load through `select(arg, &slot)`: when
//! the select picks the function argument, the load reads the caller's
//! memory, not the slot. webroot gave the argument (an entry-block param
//! no edge defines) an empty web — "provably not slot-derived" — instead
//! of `other` whenever the function had a null-check establisher, so the
//! select's web came out as exactly the slot. Stage-2 rustc's
//! `PlaceBuilder::try_to_place` (`resolved.as_ref().unwrap_or(self)`)
//! then read `resolved`'s stale bytes when it was `None` (as_place.rs:258
//! "could not resolve local", 2026-10-24).

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function, Opcode};
use rustc_hash::FxHashSet;

const SRC: &str = "function %t(i64, i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    store.i64 v1, v2
    v3 = iconst.i64 0
    v4 = icmp eq v1, v3
    brif v4, block1, block2
block1:
    v5 = select v4, v0, v2
    v6 = load.i64 notrap v5
    return v6
block2:
    return v1
}";

#[test]
fn select_with_argument_is_not_a_slot_address() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let cfg = ControlFlowGraph::with_function(&f);
    pliron_clif_opt::slotprom::run(&mut f, &cfg, &FxHashSet::default(), false, &Default::default());
    let b1 = Block::from_u32(1);
    let loads = f.layout.block_insts(b1).filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load).count();
    assert_eq!(loads, 1, "the load through select(arg, &ss0) was promoted:\n{}", f.display());
}

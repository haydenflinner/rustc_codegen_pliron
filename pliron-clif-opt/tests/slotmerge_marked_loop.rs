//! `PLIRON_SLOTMERGE_MARK=1`: a marker-bounded slot still needs the loop
//! span. Marker windows are measured in RPO positions like everything
//! else, and RPO can place a loop body past the loop's exit; the span
//! extension skipped marked slots, so a temp used only in that body was
//! folded onto a slot the header reads every trip (selmul `parse_only`
//! under marks + PLIRON_SM_POISON, 2026-10-24).
//!
//! RPO: block0, block1 (header), block2 (exit, end marker), block3 (body).
//! The header reads ss0 only through the escaped pointer (`call fn0`).

use cranelift_codegen::ir::{Block, Function, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

const SRC: &str = "function %t(i64, i64, i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    sig0 = (i64)
    fn0 = %sink sig0
block0(v0: i64, v1: i64, v2: i64):
    v3 = stack_addr.i64 ss0
    store.i64 v0, v3
    jump block1
block1:
    call fn0(v3)
    brif v1, block3, block2
block2:
    v7 = iadd v0, v0
    return v7
block3:
    v5 = stack_addr.i64 ss1
    store.i64 v2, v5
    v6 = load.i64 v5
    jump block1
}";

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

#[test]
fn marked_slot_keeps_loop_span() {
    // SAFETY: set before any config read in this test binary.
    unsafe { std::env::set_var("PLIRON_SLOTMERGE_MARK", "1") };
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let (b0, b2) = (Block::from_u32(0), Block::from_u32(2));
    let start = f.layout.first_inst(b0); // after `stack_addr ss0`
    let end = f.layout.first_inst(b2); // after `iadd` in the exit
    let ss0 = StackSlot::from_u32(0);
    let lifetimes = vec![(vec![ss0], start, b0, true), (vec![ss0], end, b2, false)];
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &lifetimes, &FxHashSet::default());
    assert_ne!(slot_of(&f, 3), slot_of(&f, 5), "the loop-body temp was folded onto the marked slot:\n{}", f.display());
}

//! Two ways slotmerge's loop handling called a slot dead while a later
//! trip still read it, both found with `PLIRON_SM_POISON` (2026-10-23):
//!
//! - **Partial per-trip writes.** A slot counted as fresh on every trip
//!   when any in-loop write dominated its in-loop reads. An array
//!   iterator rewrites only its 8-byte counter each trip and then reads
//!   the array bytes behind it, so those bytes looked dead across the
//!   back edge (tests/loopvec). Only a write covering the whole slot
//!   makes it fresh now.
//! - **Nested loop bodies past the outer latch.** A loop's span was
//!   (header start, back-edge source end) in RPO positions, but RPO can
//!   place a nested loop's body after the outer latch. A slot read in
//!   the outer header then looked dead in that body, and a temp used
//!   only there was merged onto it (cranelift-codegen's
//!   `compute_use_states`, sightglass-cli). Spans now close over any
//!   span whose header lies inside them.
//!
//! Each case asserts the temp is NOT folded onto the long-lived slot.

use cranelift_codegen::ir::{Function, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn slot_of(f: &Function, v: Value) -> StackSlot {
    let i = f.dfg.value_def(v).unwrap_inst();
    match f.dfg.insts[i] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

fn run(f: &mut Function) {
    pliron_clif_opt::slotmerge::run(f, &FxHashMap::default(), &[], &FxHashSet::default());
}

/// ss0 (16 bytes) is written in full before the loop; each trip stores
/// only its first 8 bytes, then reads bytes 8..16. ss1 is a temp used
/// later in the same trip.
const PARTIAL: &str = "function %t(i64, i64) -> i64 {
    ss0 = explicit_slot 16, align = 8
    ss1 = explicit_slot 8, align = 8
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    store.i64 v0, v2
    store.i64 v1, v2+8
    v3 = iconst.i64 0
    jump block1(v3, v3)
block1(v4: i64, v5: i64):
    store.i64 v4, v2
    v6 = load.i64 v2+8
    v7 = stack_addr.i64 ss1
    store.i64 v6, v7
    v8 = load.i64 v7
    v9 = iadd v5, v8
    v10 = iconst.i64 1
    v11 = iadd v4, v10
    v12 = icmp ult v11, v1
    brif v12, block1(v11, v9), block2(v9)
block2(v13: i64):
    return v13
}";

#[test]
fn partial_trip_write_keeps_slot_live() {
    let mut f = parse(PARTIAL);
    run(&mut f);
    let it = slot_of(&f, Value::from_u32(2));
    let tmp = slot_of(&f, Value::from_u32(7));
    assert_ne!(it, tmp, "the temp was folded onto the partially rewritten slot:\n{}", f.display());
}

/// RPO: block0, block1, block2, block4, block3, block5 — the inner body
/// block3 lies past the outer latch block4. ss0 is written once and read
/// in the outer header block1; ss1 is a temp used only in block3, and
/// block3 loops back (via block2, block4) to block1's read.
const NESTED: &str = "function %t(i64, i64, i64, i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = stack_addr.i64 ss0
    store.i64 v0, v4
    jump block1
block1:
    v5 = load.i64 v4
    jump block2
block2:
    brif v1, block4, block3
block4:
    brif v2, block1, block3
block3:
    v6 = stack_addr.i64 ss1
    store.i64 v3, v6
    v7 = load.i64 v6
    brif v7, block2, block5(v5)
block5(v8: i64):
    return v8
}";

#[test]
fn nested_body_past_outer_latch_keeps_slot_live() {
    let mut f = parse(NESTED);
    run(&mut f);
    let long = slot_of(&f, Value::from_u32(4));
    let tmp = slot_of(&f, Value::from_u32(6));
    assert_ne!(long, tmp, "the inner-body temp was folded onto the outer-header slot:\n{}", f.display());
}

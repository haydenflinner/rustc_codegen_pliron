//! `PLIRON_SM_FLOWIV=1`: a non-escaped slot's live range is its byte
//! liveness over the CFG, not the convex RPO span of its uses. The span
//! also covered blocks laid out between two uses that no path between
//! them runs (a `match` arm whose shared tail sits late), and inside a
//! loop it pinned every arm's temporaries for the whole loop even when
//! each trip rewrites them field by field before reading.

use cranelift_codegen::ir::{Function, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

fn run(src: &str) -> Function {
    // SAFETY: set before any config read in this test binary.
    unsafe { std::env::set_var("PLIRON_SM_FLOWIV", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &[], &FxHashSet::default());
    f
}

// ss0 is written in block1 and read in block3, a tail only block1
// reaches; ss1 lives entirely in block2. They must share in either arm
// order. (Here the RPO keeps block2 outside ss0's span both ways, so the
// convex interval passes too; the loop test below is the one it fails.)
const TAIL: &str = "function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
block0(v0: i64, v1: i8):
    brif v1, block1, block2
block1:
    v2 = stack_addr.i64 ss0
    store.i64 v0, v2
    jump block3
block2:
    v3 = stack_addr.i64 ss1
    store.i64 v0, v3
    v4 = load.i64 v3
    return v4
block3:
    v5 = load.i64 v2
    return v5
}";

#[test]
fn arm_slot_and_late_tail_slot_share() {
    let swapped = TAIL.replace("brif v1, block1, block2", "brif v1, block2, block1");
    for src in [TAIL.to_string(), swapped] {
        let f = run(&src);
        assert_eq!(slot_of(&f, 2), slot_of(&f, 3), "disjoint slots kept apart:\n{}", f.display());
    }
}

// A loop around a two-arm `match`: each arm writes all 16 bytes of its
// own temporary (two 8-byte stores, no single covering write) before
// reading it, so nothing survives a trip and the two can share.
const LOOP_FRESH: &str = "function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 16, align = 8
    ss1 = explicit_slot 16, align = 8
block0(v0: i64, v1: i8):
    jump block1(v0)
block1(v2: i64):
    brif v1, block2, block3
block2:
    v3 = stack_addr.i64 ss0
    store.i64 v2, v3
    store.i64 v2, v3+8
    v4 = load.i64 v3+8
    jump block4(v4)
block3:
    v5 = stack_addr.i64 ss1
    store.i64 v2, v5
    store.i64 v2, v5+8
    v6 = load.i64 v5
    jump block4(v6)
block4(v7: i64):
    v10 = iconst.i64 -1
    v8 = iadd v7, v10
    brif v8, block1(v8), block5
block5:
    return v7
}";

#[test]
fn per_trip_temporaries_in_a_loop_share() {
    let f = run(LOOP_FRESH);
    assert_eq!(slot_of(&f, 3), slot_of(&f, 5), "per-trip temporaries kept apart:\n{}", f.display());
}

// ss0 carries a value around the loop: the header reads what the last
// trip (or the entry) stored. It is live across the whole loop, so it
// may never share with ss1, used in the other arm.
const LOOP_CARRIED: &str = "function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
block0(v0: i64, v1: i8):
    v2 = stack_addr.i64 ss0
    store.i64 v0, v2
    jump block1
block1:
    v3 = load.i64 v2
    brif v1, block2, block3
block2:
    v10 = iconst.i64 1
    v9 = iadd v3, v10
    store.i64 v9, v2
    jump block4
block3:
    v4 = stack_addr.i64 ss1
    store.i64 v0, v4
    v5 = load.i64 v4
    jump block4
block4:
    v11 = iconst.i64 -1
    v6 = iadd v3, v11
    brif v6, block1, block5
block5:
    return v3
}";

#[test]
fn loop_carried_slot_never_shares() {
    let f = run(LOOP_CARRIED);
    assert_ne!(slot_of(&f, 2), slot_of(&f, 4), "loop-carried slot shared:\n{}", f.display());
}

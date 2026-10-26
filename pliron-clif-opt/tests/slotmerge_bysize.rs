//! `PLIRON_SM_BYSIZE=1`: slots are colored largest first. In `lo` order
//! the small slot of a `match`'s first arm opens a region, and the big
//! slot of a later, disjoint arm can't join it (regions only take members
//! no bigger than their rep), so every arm paid for its own storage —
//! stage-2 `lower_expr_mut` kept 36 regions, 2.6 KB, about twice LLVM's
//! whole frame.

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
    unsafe { std::env::set_var("PLIRON_SM_BYSIZE", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &[], &FxHashSet::default());
    f
}

// Two arms: an 8-byte temporary in the first (earlier in layout), a
// 64-byte one in the second. Their uses never overlap.
const ARMS: &str = "function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 64, align = 8
block0(v0: i64, v1: i8):
    brif v1, block1, block2
block1:
    v2 = stack_addr.i64 ss0
    store.i64 v0, v2
    v3 = load.i64 v2
    return v3
block2:
    v4 = stack_addr.i64 ss1
    store.i64 v0, v4
    store.i64 v0, v4+56
    v5 = load.i64 v4+56
    return v5
}";

// The same arms swapped, so whichever arm the layout puts first, one of
// the two functions has the small slot starting before the big one.
const ARMS_SWAPPED: &str = "function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 64, align = 8
block0(v0: i64, v1: i8):
    brif v1, block2, block1
block1:
    v2 = stack_addr.i64 ss0
    store.i64 v0, v2
    v3 = load.i64 v2
    return v3
block2:
    v4 = stack_addr.i64 ss1
    store.i64 v0, v4
    store.i64 v0, v4+56
    v5 = load.i64 v4+56
    return v5
}";

#[test]
fn small_first_arm_slot_joins_later_big_one() {
    for src in [ARMS, ARMS_SWAPPED] {
        let f = run(src);
        assert_eq!(slot_of(&f, 2), slot_of(&f, 4), "disjoint arm slots kept apart:\n{}", f.display());
        let total: u32 = f.sized_stack_slots.values().map(|d| d.size).sum();
        assert_eq!(total, 64, "{}", f.display());
    }
}

// The small slot wants 16-byte alignment: joining the 8-aligned big slot
// raises the representative's alignment rather than refusing the merge.
const ALIGN: &str = "function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 16, align = 16
    ss1 = explicit_slot 64, align = 8
block0(v0: i64, v1: i8):
    brif v1, block1, block2
block1:
    v2 = stack_addr.i64 ss0
    store.i64 v0, v2
    v3 = load.i64 v2
    return v3
block2:
    v4 = stack_addr.i64 ss1
    store.i64 v0, v4
    v5 = load.i64 v4
    return v5
}";

#[test]
fn stricter_alignment_raises_the_rep() {
    let f = run(ALIGN);
    assert_eq!(slot_of(&f, 2), slot_of(&f, 4), "{}", f.display());
    let d = &f.sized_stack_slots[slot_of(&f, 4)];
    assert_eq!((d.size, d.align_shift), (64, 4), "{}", f.display());
}

// Overlapping lifetimes still never share, whatever the order.
const OVERLAP: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 64, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = stack_addr.i64 ss1
    store.i64 v0, v1
    store.i64 v0, v2
    v3 = load.i64 v1
    v4 = load.i64 v2
    v5 = iadd v3, v4
    return v5
}";

#[test]
fn overlapping_slots_stay_apart() {
    let f = run(OVERLAP);
    assert_ne!(slot_of(&f, 1), slot_of(&f, 2), "{}", f.display());
}

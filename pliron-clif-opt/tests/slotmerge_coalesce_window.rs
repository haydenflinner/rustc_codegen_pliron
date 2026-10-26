//! A coalesced copy keeps its bytes in memory from the source's load to
//! the destination's store: once the pair goes, nothing holds them in a
//! register. The region must count that window as occupied. Under
//! `PLIRON_SM_BYSIZE` an 8-byte slot live between the copy's load and
//! store opened the region the 4-byte source joined, the destination then
//! coalesced into it, and the 8-byte slot's store overwrote the copied
//! bytes — stage 2's ICU `to_data_locale_language_priority` returned the
//! region code as the language, and every list-formatted warning panicked.

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
    unsafe {
        std::env::set_var("PLIRON_SM_COALESCE", "1");
        std::env::set_var("PLIRON_SM_BYSIZE", "1");
    }
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &[], &FxHashSet::default());
    f
}

// ss0 -> ss2 is a copy (v3 load, store v3 to ss2); ss1 lives between.
const WINDOW: &str = "function %t(i32, i64) -> i64 {
    ss0 = explicit_slot 4, align = 4
    ss1 = explicit_slot 8, align = 8
    ss2 = explicit_slot 4, align = 4
block0(v0: i32, v1: i64):
    v2 = stack_addr.i64 ss0
    store.i32 v0, v2
    v3 = load.i32 v2
    v4 = stack_addr.i64 ss1
    store.i64 v1, v4
    v5 = load.i64 v4
    v6 = stack_addr.i64 ss2
    store.i32 v3, v6
    v7 = load.i32 v6
    v8 = uextend.i64 v7
    v9 = iadd v5, v8
    return v9
}";

#[test]
fn slot_inside_copy_window_keeps_apart_from_the_copy() {
    let f = run(WINDOW);
    let copy_kept = f.dfg.value_def(Value::from_u32(3)).inst().is_some_and(|i| f.layout.inst_block(i).is_some());
    assert!(
        copy_kept || slot_of(&f, 4) != slot_of(&f, 6),
        "copy coalesced across a slot sharing its storage:\n{}",
        f.display()
    );
}

// Without the slot in between the copy still coalesces.
#[test]
fn copy_without_a_window_tenant_still_coalesces() {
    let src = WINDOW
        .replace("    v4 = stack_addr.i64 ss1\n    store.i64 v1, v4\n    v5 = load.i64 v4\n", "")
        .replace("iadd v5, v8", "iadd v1, v8");
    let f = run(&src);
    assert_eq!(slot_of(&f, 2), slot_of(&f, 6), "not coalesced:\n{}", f.display());
    assert!(f.layout.inst_block(f.dfg.value_def(Value::from_u32(3)).unwrap_inst()).is_none(), "copy stayed:\n{}", f.display());
}

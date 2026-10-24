//! `PLIRON_SM_PRECISE=1`: a pure op on a slot address is a use, not an
//! escape. A null-checked slot (`icmp ne p, 0`, the shape every `Option`
//! of a reference produces) then still shares storage with a slot whose
//! range is disjoint from it — before, the compare pinned it for the
//! whole function, which is how stage-2 rustc's frames grew to 2.5-12x
//! LLVM's.

use cranelift_codegen::ir::{Function, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

const SRC: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    v2 = load.i64 v1
    v3 = iconst.i64 0
    v4 = icmp ne v1, v3
    v5 = stack_addr.i64 ss1
    store.i64 v2, v5
    v6 = load.i64 v5
    brif v4, block1, block2
block1:
    return v6
block2:
    return v2
}";

#[test]
fn null_checked_slot_still_merges() {
    // SAFETY: set before any config read in this test binary.
    unsafe { std::env::set_var("PLIRON_SM_PRECISE", "1") };
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &[], &FxHashSet::default());
    assert_eq!(slot_of(&f, 1), slot_of(&f, 5), "the null-checked slot did not merge:\n{}", f.display());
}

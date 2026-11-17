//! `PLIRON_SM_COALLEN=1` (with `PLIRON_SM_COALESCE` and
//! `PLIRON_SM_COALCALL`): a whole-slot `memcpy(D, S, n)` coalesces when
//! `n` reaches the call as a block param every incoming edge passes the
//! same constant in. regex-syntax `pop_class_op` moves a 288-byte
//! `ClassState` that way (`block174(v1230)`, `v1230 = iconst 288`) and
//! kept both slots.

use cranelift_codegen::ir::{Function, InstructionData, StackSlot, Value};
use rustc_hash::FxHashMap;

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

/// ss0 is filled, then copied whole into ss1 through a block whose
/// param carries the length; ss0 is not touched again.
#[test]
fn length_through_block_param_coalesces() {
    // SAFETY: set before any config read in this test binary.
    unsafe {
        std::env::set_var("PLIRON_SM_COALESCE", "1");
        std::env::set_var("PLIRON_SM_COALCALL", "1");
        std::env::set_var("PLIRON_SM_COALLEN", "1");
    }
    let src = "function %t(i64) -> i64 {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64, i64, i64) -> i64
    fn0 = %Memcpy sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    store.i64 v0, v1+56
    v3 = iconst.i64 64
    jump block1(v3)
block1(v8: i64):
    v2 = stack_addr.i64 ss1
    v9 = stack_addr.i64 ss0
    v4 = call fn0(v2, v9, v8)
    v5 = load.i64 v2+56
    v6 = load.i64 v2
    v7 = iadd v5, v6
    return v7
}";
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::slotmerge::run_with(&mut f, &FxHashMap::default(), &[], &FxHashMap::default(), &[], &pliron_clif_opt::target::Target::host());
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "copy not coalesced:\n{}", f.display());
}

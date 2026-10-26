//! `PLIRON_SM_COALCALL=1` (with `PLIRON_SM_COALESCE=1`): a whole-slot
//! `memcpy(D, S, size)` call whose source dies and destination is born
//! at the call coalesces D into S's storage and the call goes away — a
//! `Drop` value built in a temporary and moved into its binding.

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
    // SAFETY: same values in every test of this binary, set before any
    // config read.
    unsafe {
        std::env::set_var("PLIRON_SM_COALESCE", "1");
        std::env::set_var("PLIRON_SM_COALCALL", "1");
    }
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::slotmerge::run_with(&mut f, &FxHashMap::default(), &[], &FxHashSet::default(), &[]);
    f
}

fn memcpys(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode().is_call())
        .filter(|&i| f.dfg.display_inst(i).to_string().contains("fn0("))
        .count()
}

/// ss0 is filled, copied whole into ss1 and never touched again; ss1 is
/// first written by the copy. They share storage and the copy is gone.
#[test]
fn whole_slot_memcpy_coalesces() {
    let src = "function %t(i64) -> i64 {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64, i64, i64) -> i64
    fn0 = %Memcpy sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    store.i64 v0, v1+56
    v2 = stack_addr.i64 ss1
    v3 = iconst.i64 64
    v4 = call fn0(v2, v1, v3)
    v5 = load.i64 v2+56
    v6 = load.i64 v2
    v7 = iadd v5, v6
    return v7
}";
    let f = run(src);
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "copy not coalesced:\n{}", f.display());
    assert_eq!(memcpys(&f), 0, "coalesced copy still runs:\n{}", f.display());
}

/// ss0 is read again after the copy: it doesn't die at the call, so the
/// two can't share storage and the copy stays.
#[test]
fn source_read_after_copy_stays() {
    let src = "function %t(i64) -> i64 {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64, i64, i64) -> i64
    fn0 = %Memcpy sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    v2 = stack_addr.i64 ss1
    v3 = iconst.i64 64
    v4 = call fn0(v2, v1, v3)
    store.i64 v0, v2
    v5 = load.i64 v1
    v6 = load.i64 v2
    v7 = iadd v5, v6
    return v7
}";
    let f = run(src);
    assert_ne!(slot_of(&f, 1), slot_of(&f, 2), "live source coalesced:\n{}", f.display());
    assert_eq!(memcpys(&f), 1);
}

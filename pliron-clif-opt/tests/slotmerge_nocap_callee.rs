//! `PLIRON_NOCAP_CALLEE=1`: a `nocap` fact names the call inst lowering
//! made; a pass that duplicates the call leaves copies without it. The
//! fact is a property of the callee, so slotmerge applies it to every
//! direct call of that callee at the same arg index — here the original
//! call was removed (as jumpthread does after threading every pred) and
//! only an un-annotated copy survives.

use cranelift_codegen::ir::{Block, Function, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

const SRC: &str = "function %t() {
    ss0 = explicit_slot 352, align = 8
    ss1 = explicit_slot 352, align = 8
    sig0 = (i64)
    fn0 = %by_value sig0
block0:
    v1 = stack_addr.i64 ss0
    call fn0(v1)
    v2 = stack_addr.i64 ss1
    call fn0(v2)
    return
block1:
    v3 = stack_addr.i64 ss0
    call fn0(v3)
    return
}";

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

#[test]
fn copied_call_keeps_callee_nocap() {
    // SAFETY: set before any config read in this test binary.
    unsafe { std::env::set_var("PLIRON_NOCAP_CALLEE", "1") };
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    // The annotated original: block1's call, then dropped from the layout.
    let b1 = Block::from_u32(1);
    let orig = f.layout.block_insts(b1).nth(1).unwrap();
    let nocap: FxHashSet<_> = [(orig, 0u32)].into_iter().collect();
    for i in f.layout.block_insts(b1).collect::<Vec<_>>() {
        f.layout.remove_inst(i);
    }
    f.layout.remove_block(b1);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &[], &nocap);
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "the copies' by-value args didn't share:\n{}", f.display());
}

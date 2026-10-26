//! `KILL_ALL` before an `_Unwind_Resume` call ends every slot's life: the
//! unwind leaves the frame there. Inlining breaks that when the resume
//! sat in a callee called through a `try_call`: the call becomes a
//! `try_call` whose handler is the caller's own landing pad, and the
//! unwind comes straight back into this frame. Stage 2's
//! `rustc_driver_impl::handle_options` read a 24-byte result slot in such
//! an outer pad; with the kill honored the slot counted as dead across
//! the inner pad's drop loop, and `rustc --print` (an option error, so an
//! early exit) freed a garbage pointer.

use cranelift_codegen::ir::{Block, Function, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

fn run(src: &str, resume_block: u32) -> Function {
    // SAFETY: set before any config read in this test binary.
    unsafe {
        std::env::set_var("PLIRON_SLOTMERGE_MARK", "1");
        std::env::set_var("PLIRON_SM_MARKFLOW", "1");
        std::env::set_var("PLIRON_SM_FLOWIV", "1");
    }
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let resume = f.layout.last_inst(Block::from_u32(resume_block)).unwrap();
    let kills = [(StackSlot::from_u32(u32::MAX - 1), resume, pliron_clif_opt::slotmerge::KILL_ALL)];
    pliron_clif_opt::slotmerge::run_with(&mut f, &FxHashMap::default(), &[], &FxHashSet::default(), &kills);
    f
}

// ss0 is written before a call that may unwind into block1, an inner
// pad using ss1 and resuming into block2, the outer pad, which reads ss0.
const NESTED: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    sig0 = (i64)
    fn0 = %may_throw sig0
    fn1 = %_Unwind_Resume sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    try_call fn0(v0), sig0, block3(), [ default: block1(exn0) ]
block1(v2: i64) cold:
    v3 = stack_addr.i64 ss1
    store.i64 v2, v3
    v4 = load.i64 v3
    try_call fn1(v4), sig0, block4(), [ default: block2(exn0) ]
block2(v5: i64) cold:
    v6 = load.i64 v1
    return v6
block3:
    v7 = load.i64 v1
    return v7
block4:
    trap user1
}";

#[test]
fn resume_into_own_handler_keeps_slots_alive() {
    let f = run(NESTED, 1);
    assert_ne!(slot_of(&f, 1), slot_of(&f, 3), "slot read in the outer pad shared with the inner pad's:\n{}", f.display());
}

// A plain `call` to the resume leaves the frame: ss0 is dead in the pad,
// and the slots share.
#[test]
fn resume_leaving_the_frame_still_kills() {
    let src = NESTED
        .replace("    try_call fn1(v4), sig0, block4(), [ default: block2(exn0) ]", "    call fn1(v4)\n    trap user1")
        .replace("block2(v5: i64) cold:\n    v6 = load.i64 v1\n    return v6\n", "");
    let f = run(&src, 1);
    assert_eq!(slot_of(&f, 1), slot_of(&f, 3), "{}", f.display());
}

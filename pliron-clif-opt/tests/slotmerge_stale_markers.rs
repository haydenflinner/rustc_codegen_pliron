//! Under `PLIRON_SM_MARKFLOW` a slot's end markers bound its live range —
//! unless the code contradicts them: a read of the slot reachable from a
//! kill with no write in between would observe bytes the markers call
//! dead (a pass forwarded or sank the access past the original
//! StorageDead). Such a slot's markers are dropped. Stage 2's
//! `PromoteTemps::run_pass` read a 56-byte slot far past its end-marker
//! kill; poisoning it there freed a garbage `Vec`, and with the slot
//! shared the ICU list formatter lost its data.

use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

type Lifetimes = Vec<(Vec<StackSlot>, Option<Inst>, Block, bool)>;

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

fn inst(f: &Function, b: u32, n: usize) -> Option<Inst> {
    f.layout.block_insts(Block::from_u32(b)).nth(n)
}

fn run(src: &str, lt: impl Fn(&Function) -> Lifetimes) -> Function {
    // SAFETY: set before any config read in this test binary.
    unsafe {
        std::env::set_var("PLIRON_SLOTMERGE_MARK", "1");
        std::env::set_var("PLIRON_SM_MARKFLOW", "1");
    }
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let lifetimes = lt(&f);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &lifetimes, &FxHashSet::default());
    f
}

// ss0's address escapes into fn0; its end marker sits right after the
// call, yet v4 reads it later. ss1 lives in between.
const STALE: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    sig0 = (i64)
    fn0 = %keep sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    call fn0(v1)
    v2 = stack_addr.i64 ss1
    store.i64 v0, v2
    v3 = load.i64 v2
    v4 = load.i64 v1
    v5 = iadd v3, v4
    return v5
}";

#[test]
fn read_past_end_marker_distrusts_the_markers() {
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    let f = run(STALE, |f| {
        vec![
            (vec![ss0], inst(f, 0, 2), Block::from_u32(0), false),
            (vec![ss1], inst(f, 0, 3), Block::from_u32(0), true),
            (vec![ss1], inst(f, 0, 5), Block::from_u32(0), false),
        ]
    });
    assert_ne!(slot_of(&f, 1), slot_of(&f, 2), "slot read past its end marker was shared:\n{}", f.display());
}

// The same without the late read: the end marker holds, and the slots
// share (the check only drops contradicted markers).
#[test]
fn consistent_markers_still_bound() {
    let src = STALE.replace("    v4 = load.i64 v1\n    v5 = iadd v3, v4\n    return v5", "    return v3");
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    let f = run(&src, |f| {
        vec![
            (vec![ss0], inst(f, 0, 2), Block::from_u32(0), false),
            (vec![ss1], inst(f, 0, 3), Block::from_u32(0), true),
            (vec![ss1], inst(f, 0, 5), Block::from_u32(0), false),
        ]
    });
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "{}", f.display());
}

//! `PLIRON_SM_FLOWIV=1` under `PLIRON_SM_MARKFLOW=1`: an exactly placed
//! start marker (MIR StorageLive) kills a non-escaped slot's liveness —
//! its old bytes are dead. A call the slot is passed to reads it (it may
//! also be an out-param or an `sret` that writes only part), so without
//! the marker the slot is live round the whole loop the call sits in:
//! regex-syntax `NestLimiter::check` kept its 128-byte `ast::Error` and
//! 56-byte `ErrorKind` across the entire inlined visitor loop.

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

/// The `n`th instruction of block `b`.
fn inst(f: &Function, b: u32, n: usize) -> Option<Inst> {
    f.layout.block_insts(Block::from_u32(b)).nth(n)
}

/// Run slotmerge with every call's arg 0 non-capturing.
fn run(src: &str, lt: impl Fn(&Function) -> Lifetimes) -> Function {
    // SAFETY: every test sets the same values before any config read.
    unsafe {
        std::env::set_var("PLIRON_SLOTMERGE_MARK", "1");
        std::env::set_var("PLIRON_SM_MARKFLOW", "1");
        std::env::set_var("PLIRON_SM_FLOWIV", "1");
    }
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let nocap: FxHashSet<(Inst, u32)> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode().is_call())
        .map(|i| (i, 0))
        .collect();
    let lifetimes = lt(&f);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &lifetimes, &nocap);
    f
}

// A loop around a two-arm `match`; each arm hands its own temporary to a
// call (which reads it) and then loads the result.
const LOOP: &str = "function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 32, align = 8
    ss1 = explicit_slot 32, align = 8
    sig0 = (i64)
    fn0 = %fill sig0
block0(v0: i64, v1: i8):
    jump block1(v0)
block1(v2: i64):
    brif v1, block2, block3
block2:
    v3 = stack_addr.i64 ss0
    call fn0(v3)
    v4 = load.i64 v3
    jump block4(v4)
block3:
    v5 = stack_addr.i64 ss1
    call fn0(v5)
    v6 = load.i64 v5
    jump block4(v6)
block4(v7: i64):
    v8 = iconst.i64 -1
    v9 = iadd v7, v8
    brif v9, block1(v9), block5
block5:
    return v7
}";

#[test]
fn start_markers_end_per_trip_lives() {
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    // StorageLive right after each arm's stack_addr.
    let f = run(LOOP, |f| {
        vec![
            (vec![ss0], inst(f, 2, 0), Block::from_u32(2), true),
            (vec![ss1], inst(f, 3, 0), Block::from_u32(3), true),
        ]
    });
    assert_eq!(slot_of(&f, 3), slot_of(&f, 5), "arm temporaries kept apart:\n{}", f.display());
}

#[test]
fn without_start_markers_calls_keep_slots_live_round_the_loop() {
    let f = run(LOOP, |_| Vec::new());
    assert_ne!(slot_of(&f, 3), slot_of(&f, 5), "slots shared without markers:\n{}", f.display());
}

// A start marker whose anchor moved to another block is not trusted.
#[test]
fn misplaced_start_marker_is_ignored() {
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    let f = run(LOOP, |f| {
        vec![
            (vec![ss0], inst(f, 3, 0), Block::from_u32(2), true),
            (vec![ss1], inst(f, 2, 0), Block::from_u32(3), true),
        ]
    });
    assert_ne!(slot_of(&f, 3), slot_of(&f, 5), "{}", f.display());
}

//! `PLIRON_SM_STARTS=1`: LLVM StackColoring's interference test — two
//! slots conflict only if one starts being in use (an access) while the
//! other is live. `PLIRON_SM_AFTERKILL=1`: an end marker that only quiet
//! insts and jumps separate from a preceding call also kills just after
//! that call, which survives jumpthread collapsing the jumps.
//! Stage-2 `lower_item_kind` had two 328-byte slots per `match` arm that
//! never merged: their end markers were lost to jumpthread, and every arm's
//! slots were live together in the shared panic-in-cleanup tail.

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

fn setup() {
    // SAFETY: same values in every test of this binary, set before any
    // config read.
    unsafe {
        std::env::set_var("PLIRON_SLOTMERGE_MARK", "1");
        std::env::set_var("PLIRON_SM_MARKFLOW", "1");
        std::env::set_var("PLIRON_SM_AFTERKILL", "1");
        std::env::set_var("PLIRON_SM_STARTS", "1");
    }
}

fn run(src: &str, lt: impl Fn(&Function) -> Lifetimes, extra_kill: Option<(u32, usize)>) -> Function {
    setup();
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let lifetimes = lt(&f);
    let mut kills = pliron_clif_opt::slotmerge::entry_kills(&f, &lifetimes);
    if let Some((b, n)) = extra_kill {
        let i = f.layout.block_insts(Block::from_u32(b)).nth(n).unwrap();
        kills.push((StackSlot::from_u32(u32::MAX - 1), i, pliron_clif_opt::slotmerge::KILL_ALL));
    }
    pliron_clif_opt::slotmerge::run_with(&mut f, &FxHashMap::default(), &lifetimes, &FxHashSet::default(), &kills);
    f
}

/// Two arms, each with an escaped slot ended on its normal path, unwind
/// into one abort tail (`call; trap`) with no kill: both slots are live
/// there, but neither starts there, so they never are in use together.
#[test]
fn shared_abort_tail_does_not_interfere() {
    let src = "function %t(i8) {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64)
    sig1 = ()
    fn0 = %sink sig0
    fn1 = %panic_in_cleanup sig1
block0(v0: i8):
    brif v0, block1, block2
block1:
    v1 = stack_addr.i64 ss0
    try_call fn0(v1), sig0, block5(), [ default: block4(exn0) ]
block2:
    v2 = stack_addr.i64 ss1
    try_call fn0(v2), sig0, block6(), [ default: block4(exn0) ]
block5:
    jump block3
block6:
    jump block3
block3:
    return
block4(v9: i64):
    call fn1()
    trap user1
}";
    let f = run(
        src,
        |_| {
            vec![
                (vec![StackSlot::from_u32(0)], None, Block::from_u32(5), false),
                (vec![StackSlot::from_u32(1)], None, Block::from_u32(6), false),
            ]
        },
        None,
    );
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "slots live together only in the abort tail stayed apart:\n{}", f.display());
}

/// ss1 starts (its call) while ss0 is still live (read again after): they
/// interfere and must stay apart.
#[test]
fn start_inside_live_interferes() {
    let src = "function %t() {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64)
    fn0 = %sink sig0
block0:
    v1 = stack_addr.i64 ss0
    call fn0(v1)
    v2 = stack_addr.i64 ss1
    call fn0(v2)
    call fn0(v1)
    jump block1
block1:
    return
}";
    let f = run(
        src,
        |_| {
            vec![
                (vec![StackSlot::from_u32(0)], None, Block::from_u32(1), false),
                (vec![StackSlot::from_u32(1)], None, Block::from_u32(1), false),
            ]
        },
        None,
    );
    assert_ne!(slot_of(&f, 1), slot_of(&f, 2), "overlapping lives merged:\n{}", f.display());
}

/// A store into ss1 while ss0 is live, never read back: its live set may
/// be just that position, but the store still writes the bytes — it is a
/// start inside ss0's live set.
#[test]
fn dead_store_inside_live_interferes() {
    let src = "function %t(i64) {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64)
    fn0 = %sink sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    call fn0(v1)
    v2 = stack_addr.i64 ss1
    store.i64 v0, v2
    call fn0(v1)
    jump block1
block1:
    return
}";
    let f = run(src, |_| vec![(vec![StackSlot::from_u32(0)], None, Block::from_u32(1), false)], None);
    assert_ne!(slot_of(&f, 1), slot_of(&f, 2), "a store landed in a live slot:\n{}", f.display());
}

/// The drop call, then a forwarding chain into the end marker's block;
/// jumpthread folds it all into one block, deleting every jump the edge
/// kills hung on. The kill just after the call survives.
#[test]
fn after_call_kill_survives_chain_collapse() {
    let src = "function %t() {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64)
    fn0 = %sink sig0
block0:
    v1 = stack_addr.i64 ss0
    call fn0(v1)
    jump block1
block1:
    jump block2
block2:
    jump block3
block3:
    v2 = stack_addr.i64 ss1
    call fn0(v2)
    return
}";
    setup();
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let (b0, b1, b2, b3) = (Block::from_u32(0), Block::from_u32(1), Block::from_u32(2), Block::from_u32(3));
    let lifetimes: Lifetimes = vec![
        (vec![StackSlot::from_u32(0)], None, b2, false),
        (vec![StackSlot::from_u32(1)], f.layout.block_insts(b3).nth(1), b3, false),
    ];
    let kills = pliron_clif_opt::slotmerge::entry_kills(&f, &lifetimes);
    // Fold block1..block3 into block0.
    let j0 = f.layout.last_inst(b0).unwrap();
    f.layout.remove_inst(j0);
    for b in [b1, b2] {
        let j = f.layout.first_inst(b).unwrap();
        f.layout.remove_inst(j);
        f.layout.remove_block(b);
    }
    let moved: Vec<Inst> = f.layout.block_insts(b3).collect();
    for i in moved {
        f.layout.remove_inst(i);
        f.layout.append_inst(i, b0);
    }
    f.layout.remove_block(b3);
    let mut g = f.clone();
    pliron_clif_opt::slotmerge::run_with(&mut f, &FxHashMap::default(), &lifetimes, &FxHashSet::default(), &kills);
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "ss0 stayed live past the collapsed chain:\n{}", f.display());
    // Only the after-call kill (`u32::MAX - 3`) still resolves.
    let without: Vec<_> = kills.iter().copied().filter(|k| k.2 != u32::MAX - 3).collect();
    pliron_clif_opt::slotmerge::run_with(&mut g, &FxHashMap::default(), &lifetimes, &FxHashSet::default(), &without);
    assert_ne!(slot_of(&g, 1), slot_of(&g, 2), "test lost its point: another kill survived");
}

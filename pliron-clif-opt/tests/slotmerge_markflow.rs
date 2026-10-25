//! `PLIRON_SM_MARKFLOW=1` (with `PLIRON_SLOTMERGE_MARK=1`): an escaped
//! slot is live from its uses until an end marker on every path. CLIF
//! passes delete and duplicate marker blocks (jumpthread alone dropped a
//! third of `main`'s markers in tests/std), so a lost marker must cost
//! precision, never soundness.

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

/// The `n`th instruction of `b`.
fn inst(f: &Function, b: u32, n: usize) -> Option<Inst> {
    f.layout.block_insts(Block::from_u32(b)).nth(n)
}

fn run(src: &str, lt: impl Fn(&Function) -> Lifetimes) -> Function {
    // SAFETY: every test sets the same values before any config read.
    unsafe {
        std::env::set_var("PLIRON_SLOTMERGE_MARK", "1");
        std::env::set_var("PLIRON_SM_MARKFLOW", "1");
    }
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let lifetimes = lt(&f);
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &lifetimes, &FxHashSet::default());
    f
}

/// Two escaped slots with disjoint marker windows share storage although
/// ss0's start marker was lost (its block left the layout) — the old
/// all-markers-resolve rule left ss0 unbounded.
#[test]
fn lost_start_marker_still_bounds() {
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
    v2 = stack_addr.i64 ss1
    call fn0(v2)
    return
}";
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    let f = run(src, |f| {
        vec![
            (vec![ss0], None, Block::from_u32(9), true),
            (vec![ss0], inst(f, 0, 1), Block::from_u32(0), false),
            (vec![ss1], inst(f, 1, 0), Block::from_u32(1), true),
            (vec![ss1], inst(f, 1, 1), Block::from_u32(1), false),
        ]
    });
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "disjoint windows didn't share:\n{}", f.display());
}

/// block2 is a copy of block1 whose end marker didn't survive: on that
/// path ss0 stays live into block3, where `fn1` may still read it through
/// the pointer `fn0` retained. A hull of the surviving marker positions
/// ends in block1 and let ss1 take ss0's storage.
#[test]
fn duplicated_block_without_kill_stays_live() {
    let src = "function %t(i64) {
    ss0 = explicit_slot 64, align = 8
    ss1 = explicit_slot 64, align = 8
    sig0 = (i64)
    sig1 = ()
    fn0 = %sink sig0
    fn1 = %peek sig1
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    call fn0(v1)
    brif v0, block1, block2
block1:
    call fn1()
    jump block3
block2:
    call fn1()
    jump block3
block3:
    call fn1()
    v2 = stack_addr.i64 ss1
    call fn0(v2)
    return
}";
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    let f = run(src, |f| {
        vec![
            (vec![ss0], inst(f, 0, 0), Block::from_u32(0), true),
            (vec![ss0], inst(f, 1, 0), Block::from_u32(1), false),
            (vec![ss1], inst(f, 3, 1), Block::from_u32(3), true),
            (vec![ss1], inst(f, 3, 2), Block::from_u32(3), false),
        ]
    });
    assert_ne!(slot_of(&f, 1), slot_of(&f, 2), "ss1 took storage ss0 still holds:\n{}", f.display());
}

/// ss0 lives twice (block0 and block2), killed in between: its live set
/// has a hole over block1 that ss1 can use, though the hull of ss0's
/// positions covers ss1 entirely.
#[test]
fn second_life_leaves_a_hole() {
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
    v2 = stack_addr.i64 ss1
    call fn0(v2)
    jump block2
block2:
    v3 = stack_addr.i64 ss0
    call fn0(v3)
    return
}";
    let (ss0, ss1) = (StackSlot::from_u32(0), StackSlot::from_u32(1));
    let f = run(src, |f| {
        vec![
            (vec![ss0], inst(f, 0, 1), Block::from_u32(0), false),
            (vec![ss1], inst(f, 1, 1), Block::from_u32(1), false),
            (vec![ss0], inst(f, 2, 1), Block::from_u32(2), false),
        ]
    });
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "ss1 didn't take ss0's hole:\n{}", f.display());
    assert_eq!(slot_of(&f, 1), slot_of(&f, 3));
}

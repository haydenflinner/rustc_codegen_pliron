//! `PLIRON_SM_COALESCE=1`: a slot copied pair-by-pair into another (`v =
//! load S+k; store v, D+k`) whose source dies and destination is born at
//! the copy shares one slot with it, and the copy disappears (LLVM
//! MemCpyOpt's stack move). rustc's lowering moves 300-byte values
//! between locals this way.

use cranelift_codegen::ir::{Function, Inst, InstructionData, Opcode, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

fn slot_of(f: &Function, v: u32) -> StackSlot {
    let v = f.dfg.resolve_aliases(Value::from_u32(v));
    match f.dfg.insts[f.dfg.value_def(v).unwrap_inst()] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

fn mem_ops(f: &Function) -> usize {
    let b = f.layout.entry_block().unwrap();
    f.layout
        .block_insts(b)
        .filter(|&i| matches!(f.dfg.insts[i].opcode(), Opcode::Load | Opcode::Store))
        .count()
}

/// Run slotmerge with every `call`'s arg 0 non-capturing.
fn run(src: &str) -> Function {
    // SAFETY: set before any config read in this test binary.
    unsafe { std::env::set_var("PLIRON_SM_COALESCE", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let nocap: FxHashSet<(Inst, u32)> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode().is_call())
        .map(|i| (i, 0))
        .collect();
    pliron_clif_opt::slotmerge::run(&mut f, &FxHashMap::default(), &[], &nocap);
    f
}

#[test]
fn copy_source_and_destination_share_a_slot() {
    let f = run("function %t(i64) {
    ss0 = explicit_slot 16, align = 8
    ss1 = explicit_slot 16, align = 8
    sig0 = (i64)
    fn0 = %use sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    store.i64 v0, v1+8
    call fn0(v1)
    v2 = stack_addr.i64 ss1
    v3 = load.i64 v1
    store.i64 v3, v2
    v4 = load.i64 v1+8
    store.i64 v4, v2+8
    call fn0(v2)
    return
}");
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "not coalesced:\n{}", f.display());
    assert_eq!(mem_ops(&f), 2, "the self-copy stayed:\n{}", f.display());
}

/// The source is read again after the copy: it doesn't die there, so the
/// destination can't take its bytes.
#[test]
fn source_read_after_copy_stays_apart() {
    let f = run("function %t(i64) -> i64 {
    ss0 = explicit_slot 16, align = 8
    ss1 = explicit_slot 16, align = 8
    sig0 = (i64)
    fn0 = %use sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    store.i64 v0, v1+8
    call fn0(v1)
    v2 = stack_addr.i64 ss1
    v3 = load.i64 v1
    store.i64 v3, v2
    v4 = load.i64 v1+8
    store.i64 v4, v2+8
    call fn0(v2)
    v5 = load.i64 v1+8
    return v5
}");
    assert_ne!(slot_of(&f, 1), slot_of(&f, 2), "coalesced a live source:\n{}", f.display());
    assert_eq!(mem_ops(&f), 7);
}

/// rustc's shape: the call filling S passes S's address on to its
/// normal-return block as a block param, and the copy reads S through it.
#[test]
fn copy_through_a_block_param() {
    let f = run("function %t(i64) {
    ss0 = explicit_slot 16, align = 8
    ss1 = explicit_slot 16, align = 8
    sig0 = (i64)
    fn0 = %use sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 v0, v1
    store.i64 v0, v1+8
    call fn0(v1)
    jump block1(v1)
block1(v9: i64):
    v2 = stack_addr.i64 ss1
    v3 = load.i64 v9
    store.i64 v3, v2
    v4 = load.i64 v9+8
    store.i64 v4, v2+8
    call fn0(v2)
    return
}");
    assert_eq!(slot_of(&f, 1), slot_of(&f, 2), "not coalesced:\n{}", f.display());
}

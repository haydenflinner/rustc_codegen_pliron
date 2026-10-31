//! slotprom container fields: a slot field written whole and read back by
//! narrower integer loads inside it is promoted, the narrow loads becoming
//! `ireduce(ushr(field, 8*delta))` (LLVM SROA's integer widening). regex's
//! `follow_epsilons` stored its six-bool flags argument (one i64) to a
//! slot and read each bool back as a byte; the six values then spilled.
//! Each case is differentially checked against the interpreter.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::FxHashSet;

fn run(src: &str) -> (Function, Function) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut f = before.clone();
    let cfg = ControlFlowGraph::with_function(&f);
    pliron_clif_opt::slotprom::run(&mut f, &cfg, &FxHashSet::default(), false, &Default::default());
    (before, f)
}

fn mem_ops(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i].opcode(), Opcode::Load | Opcode::Store))
        .count()
}

fn same(before: &Function, after: &Function) {
    if let Err(m) = check(before, after, &Facts::default(), &Config::default()) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

/// The flags shape: one i64 store, byte/half/word loads at several offsets.
const FLAGS: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    store.i64 notrap v0, v1
    v2 = load.i8 notrap v1
    v3 = load.i8 notrap v1+1
    v4 = load.i8 notrap v1+5
    v5 = load.i16 notrap v1+2
    v6 = load.i32 notrap v1+4
    v7 = uextend.i64 v2
    v8 = uextend.i64 v3
    v9 = uextend.i64 v4
    v10 = uextend.i64 v5
    v11 = uextend.i64 v6
    v20 = iconst.i64 8
    v21 = iconst.i64 16
    v22 = iconst.i64 24
    v23 = iconst.i64 40
    v12 = ishl v8, v20
    v13 = ishl v9, v21
    v14 = ishl v10, v22
    v15 = ishl v11, v23
    v16 = bor v7, v12
    v17 = bor v16, v13
    v18 = bor v17, v14
    v19 = bxor v18, v15
    return v19
}";

#[test]
fn narrow_loads_read_the_promoted_field() {
    let (before, after) = run(FLAGS);
    assert_eq!(mem_ops(&after), 0, "slot not promoted:\n{}", after.display());
    same(&before, &after);
}

/// The field is rewritten in a loop; narrow loads read the current value.
const LOOP: &str = "function %t(i64, i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    store.i64 notrap v0, v2
    v3 = iconst.i64 0
    jump block1(v3, v3)
block1(v4: i64, v5: i64):
    v6 = load.i8 notrap v2+3
    v7 = uextend.i64 v6
    v8 = iadd v5, v7
    v9 = load.i64 notrap v2
    v20 = iconst.i64 7
    v10 = rotl v9, v20
    store.i64 notrap v10, v2
    v21 = iconst.i64 1
    v11 = iadd v4, v21
    v12 = icmp ult v11, v1
    brif v12, block1(v11, v8), block2(v8)
block2(v13: i64):
    return v13
}";

#[test]
fn narrow_loads_follow_stores_in_a_loop() {
    let (before, after) = run(LOOP);
    assert_eq!(mem_ops(&after), 0, "slot not promoted:\n{}", after.display());
    same(&before, &after);
}

/// A narrow store inside the field writes part of it: stays in memory.
#[test]
fn narrow_store_inside_keeps_memory() {
    let (before, after) = run("function %t(i64, i8) -> i64 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64, v1: i8):
    v2 = stack_addr.i64 ss0
    store.i64 notrap v0, v2
    store.i8 notrap v1, v2+2
    v3 = load.i8 notrap v2+2
    v4 = load.i64 notrap v2
    v5 = uextend.i64 v3
    v6 = iadd v4, v5
    return v6
}");
    assert!(mem_ops(&after) > 0, "promoted across a narrow store:\n{}", after.display());
    same(&before, &after);
}

/// A load straddling the field's end reads bytes the field doesn't hold.
#[test]
fn straddling_load_keeps_memory() {
    let (before, after) = run("function %t(i32, i32) -> i64 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i32, v1: i32):
    v2 = stack_addr.i64 ss0
    store.i32 notrap v0, v2
    store.i32 notrap v1, v2+4
    v3 = load.i16 notrap v2+3
    v4 = uextend.i64 v3
    return v4
}");
    assert!(mem_ops(&after) > 0, "promoted a straddling load:\n{}", after.display());
    same(&before, &after);
}

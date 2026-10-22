//! A slot read only *through a container* — its address stored in a
//! closure env the callee dereferences — is still read on every loop
//! trip. The backedge freshness check counted only direct reads of the
//! slot, so the once-written `requirements` slot in prqlc's
//! `compute_positional_mappings` looked dead inside the loop and the
//! `log` level temps written later in the body were merged onto it
//! (`sql::test_append_select_multiple`: unfiltered columns, 2026-10-22).

use cranelift_codegen::ir::{Function, InstructionData, StackSlot, Value};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// ss0 = requirements (written once), ss1 = env holding &ss0, ss2 = a
/// temp written after the call each trip. `{LATCH}` is either a separate
/// latch block or the header itself (a self-loop).
fn src(self_loop: bool) -> String {
    let (latch, tail) = if self_loop {
        ("brif v12, block1(v11, v9), block3(v9)", "")
    } else {
        ("jump block2", "
block2:
    brif v12, block1(v11, v9), block3(v9)")
    };
    format!("function %t(i64, i64) -> i64 {{
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    ss2 = explicit_slot 8, align = 8
    sig0 = (i64)
    fn0 = %reader sig0
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    store.i64 v0, v2
    v3 = iconst.i64 0
    jump block1(v3, v3)
block1(v4: i64, v14: i64):
    v5 = stack_addr.i64 ss1
    store.i64 v2, v5
    call fn0(v5)
    v7 = stack_addr.i64 ss2
    store.i64 v4, v7
    v8 = load.i64 v7
    v9 = iadd v14, v8
    v10 = iconst.i64 1
    v11 = iadd v8, v10
    v12 = icmp ult v11, v1
    {latch}{tail}
block3(v13: i64):
    return v13
}}")
}

fn slot_of(f: &Function, v: Value) -> StackSlot {
    let i = f.dfg.value_def(v).unwrap_inst();
    match f.dfg.insts[i] {
        InstructionData::StackAddr { stack_slot, .. } => stack_slot,
        _ => panic!("not a stack_addr"),
    }
}

fn check(self_loop: bool) {
    let mut f = parse(&src(self_loop));
    // The reader writes nothing, so passing the env doesn't escape it.
    let fr = f.dfg.ext_funcs.keys().next().unwrap();
    let nowrite: FxHashMap<_, _> = [(fr, true)].into_iter().collect();
    pliron_clif_opt::slotmerge::run(&mut f, &nowrite, &[], &FxHashSet::default());
    let req = slot_of(&f, Value::from_u32(2));
    let tmp = slot_of(&f, Value::from_u32(7));
    assert_ne!(req, tmp, "the loop temp was folded onto the env-read slot:\n{}", f.display());
}

#[test]
fn container_read_keeps_slot_live_across_loop() {
    check(false);
}

#[test]
fn self_loop_is_a_backedge() {
    check(true);
}

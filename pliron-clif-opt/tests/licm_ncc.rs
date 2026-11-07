//! licm treats a `noalias` root whose only escapes are `nocapture` call
//! operands as isolated inside a loop holding none of those calls (the
//! `nocap` argument; `PLIRON_LICM_NCC` in the pipeline). The call can't
//! leave an alias behind, so stores through other pointers in the loop
//! can't reach the root's memory and its field loads hoist (regex's
//! `next_state` hands `qnext` to `follow_epsilons` outside the loop that
//! fills `qnext`). With the call inside the loop it may write the field on
//! any iteration: the load must stay.

use cranelift_codegen::ir::{Block, Function, Inst, Opcode, Value};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

/// `v0`: the `noalias` root (`q`), 16 dereferenceable bytes; `v1`: an
/// unrelated buffer. The loop reads `q.ptr` (`v0+8`) and stores through
/// it, the way `SparseSet::insert` does; `fn0(v0)` sits before the loop
/// or in its header.
fn src(call_in_loop: bool) -> String {
    let (pre, inl) = if call_in_loop { ("", "    call fn0(v0)\n") } else { ("    call fn0(v0)\n", "") };
    format!(
        "function %f(i64, i64, i64) {{
    sig0 = (i64)
    fn0 = %g sig0
block0(v0: i64, v1: i64, v2: i64):
{pre}    v3 = iconst.i64 0
    jump block1(v3)
block1(v4: i64):
{inl}    v5 = load.i64 notrap v0+8
    v10 = iconst.i64 3
    v6 = ishl v4, v10
    v7 = iadd v5, v6
    store.i64 notrap v4, v7
    v11 = iconst.i64 1
    v8 = iadd v4, v11
    v9 = icmp ult v8, v2
    brif v9, block1(v8), block2
block2:
    return
}}"
    )
}

fn call_inst(f: &Function) -> Inst {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| f.dfg.insts[i].opcode() == Opcode::Call)
        .unwrap()
}

fn load_in_loop(f: &Function) -> bool {
    // `block1` stays the header; a hoisted load lands in a preheader.
    let h = Block::from_u32(1);
    f.layout.block_insts(h).any(|i| f.dfg.insts[i].opcode() == Opcode::Load)
}

fn run(call_in_loop: bool, nocap_on: bool) -> Function {
    let mut f = cranelift_reader::parse_functions(&src(call_in_loop)).unwrap().remove(0);
    let v0 = Value::from_u32(0);
    let noalias: FxHashSet<Value> = [v0].into_iter().collect();
    let deref: FxHashMap<Value, (u64, bool)> = [(v0, (16, true))].into_iter().collect();
    let mut nocap: FxHashSet<(Inst, u32)> = FxHashSet::default();
    if nocap_on {
        nocap.insert((call_inst(&f), 0));
    }
    pliron_clif_opt::licm::run(
        &mut f,
        &mut Analysis::default(),
        &noalias,
        &FxHashSet::default(),
        &deref,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &nocap,
        false,
    );
    f
}

#[test]
fn nocapture_escape_before_loop_lets_field_load_hoist() {
    let f = run(false, true);
    assert!(!load_in_loop(&f), "q.ptr still loaded in the loop:\n{}", f.display());
    // Without the nocapture fact the call may have stashed `q`.
    let f = run(false, false);
    assert!(load_in_loop(&f), "hoisted past a capturing escape:\n{}", f.display());
}

#[test]
fn nocapture_call_inside_loop_keeps_the_load() {
    let f = run(true, true);
    assert!(load_in_loop(&f), "hoisted past an in-loop call that may write q:\n{}", f.display());
}

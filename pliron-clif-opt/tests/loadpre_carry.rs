//! `PLIRON_LOADPRE_CARRY=1`: loop-carried load PRE. A `noalias`,
//! dereferenceable param's field is loaded in one arm of a loop body; the
//! loop header gets a block param for it, the entry edge and the arm that
//! calls with the pointer (a clobber) get compensating loads, and the arm
//! that neither loads nor clobbers carries the param around unchanged.
//! regex's `next_state` reloaded its `&mut SparseSet`'s five fields per
//! decoded element for want of this.

use cranelift_codegen::ir::{Block, Function, Inst, Opcode};
use rustc_hash::{FxHashMap, FxHashSet};

const SRC: &str = "function %f(i64, i64, i64) -> i64 {
    sig0 = (i64)
    fn0 = %g sig0
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    jump block1(v3, v3)
block1(v4: i64, v5: i64):
    v21 = iconst.i64 3
    v6 = ishl v4, v21
    v7 = iadd v2, v6
    v8 = load.i64 notrap v7
    v22 = iconst.i64 1
    v9 = icmp eq v8, v22
    brif v9, block2, block3
block2:
    v10 = load.i64 notrap v1+8
    v11 = iadd v5, v10
    v14 = iadd v4, v22
    v15 = icmp ult v14, v0
    brif v15, block1(v14, v11), block6(v11)
block3:
    v23 = iconst.i64 2
    v12 = icmp eq v8, v23
    brif v12, block4, block5
block4:
    call fn0(v1)
    v16 = iadd v4, v22
    v17 = icmp ult v16, v0
    brif v17, block1(v16, v5), block6(v5)
block5:
    v18 = iadd v4, v22
    v19 = icmp ult v18, v0
    brif v19, block1(v18, v5), block6(v5)
block6(v20: i64):
    return v20
}";

fn field_loads(f: &Function) -> Vec<(Block, Inst)> {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b).map(move |i| (b, i)))
        .filter(|&(_, i)| {
            f.dfg.insts[i].opcode() == Opcode::Load && f.dfg.display_inst(i).to_string().contains("v1+8")
        })
        .collect()
}

#[test]
fn field_load_carried_around_the_loop() {
    // Opt-in; this test binary holds only this test.
    unsafe { std::env::set_var("PLIRON_LOADPRE_CARRY", "1") };
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let v1 = cranelift_codegen::ir::Value::from_u32(1);
    let call = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .find(|&i| f.dfg.insts[i].opcode().is_call())
        .unwrap();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    let noalias: FxHashSet<_> = [v1].into_iter().collect();
    let nocap: FxHashMap<(Inst, u32), u64> = [(call, 0)].into_iter().map(|k| (k, u64::MAX)).collect();
    let deref: FxHashMap<_, (u64, bool)> = [(v1, (16, true))].into_iter().collect();
    pliron_clif_opt::loadfwd::run_deref(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &noalias,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &nocap,
        &deref,
    );
    let loads = field_loads(&f);
    let blocks: Vec<Block> = loads.iter().map(|&(b, _)| b).collect();
    // The arm's load became the header's param; the entry edge and the
    // clobbering arm reload, the quiet arm carries.
    assert!(
        !blocks.contains(&Block::from_u32(2)) && !blocks.contains(&Block::from_u32(5)),
        "{blocks:?}\n{}",
        f.display()
    );
    assert!(
        blocks.contains(&Block::from_u32(0)) && blocks.contains(&Block::from_u32(4)) && loads.len() == 2,
        "{blocks:?}\n{}",
        f.display()
    );
    assert_eq!(f.dfg.num_block_params(Block::from_u32(1)), 3, "{}", f.display());
}

/// The load's own block is a merge the loop re-enters, and the call after
/// the load writes the location (lol_html's state machine: `self.state`
/// loaded, then called, and the callee stores the next state). The climb
/// to the dominator must not treat the block's tail as preserved.
const SELF_CLOBBER: &str = "function %f(i64, i64) -> i64 {
    sig0 = (i64) -> i64
    sig1 = (i64)
    fn1 = %g sig1
block0(v0: i64, v1: i64):
    brif v1, block1, block2
block1:
    store.i64 notrap v1, v0+16
    jump block3
block2:
    call fn1(v0)
    jump block3
block3:
    jump block4
block4:
    v3 = load.i64 notrap v0+16
    v4 = call_indirect sig0, v3(v0)
    brif v4, block5, block6
block5:
    jump block4
block6:
    return v4
}";

#[test]
fn load_before_clobbering_call_in_reentered_block_stays() {
    unsafe { std::env::set_var("PLIRON_LOADPRE_CARRY", "1") };
    let mut f = cranelift_reader::parse_functions(SELF_CLOBBER).unwrap().remove(0);
    let v0 = cranelift_codegen::ir::Value::from_u32(0);
    let calls: Vec<Inst> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode().is_call())
        .collect();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    let noalias: FxHashSet<_> = [v0].into_iter().collect();
    let nocap: FxHashMap<(Inst, u32), u64> = calls.iter().map(|&c| ((c, 0), u64::MAX)).collect();
    let deref: FxHashMap<_, (u64, bool)> = [(v0, (24, true))].into_iter().collect();
    pliron_clif_opt::loadfwd::run_deref(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &noalias,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &nocap,
        &deref,
    );
    let in_b4 = f
        .layout
        .block_insts(Block::from_u32(4))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count();
    assert_eq!(in_b4, 1, "the load must stay after the clobbering call's back edge\n{}", f.display());
}

//! A region-frozen operand (`*v0` immutable while an inlined `&self`
//! helper's clone runs) is only immune inside that clone's blocks. A CLIF
//! block merge can fuse two helper clones and the caller code between them
//! into one block that still carries the first clone's region tag; then a
//! `&mut self` call in the merged block looked frozen and loadfwd forwarded
//! the capacity reload across it (uiua `Shape::extend_from_slice`:
//! `triple(&self)`, `try_reserve`, `triple(&self)`; 2026-10-22).
//! `pipeline::live_regions` derives the block's region from its
//! instructions' original regions: their common ancestor, the outer region.

use cranelift_codegen::ir::{Function, Inst, Opcode, Value};
use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use rustc_hash::{FxHashMap, FxHashSet};

// v0 = &mut self. Insts: load (helper clone R1), call (caller, O),
// load (helper clone R2) — fused into block0.
const FN: &str = "function %f(i64) -> i64 {
    sig0 = (i64)
    fn0 = %try_reserve sig0

block0(v0: i64):
    v1 = load.i64 notrap v0+16
    call fn0(v0)
    v2 = load.i64 notrap v0+16
    v3 = iadd v1, v2
    return v3
}";

fn insts(f: &Function) -> Vec<Inst> {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).collect()
}

fn run(f: &mut Function, breg: &FxHashMap<cranelift_codegen::ir::Block, Ptr<BasicBlock>>,
       ofz: &FxHashMap<Value, Vec<(u64, Ptr<BasicBlock>)>>,
       rpar: &pliron::cols::ColMap<Ptr<BasicBlock>, Ptr<BasicBlock>>) -> usize {
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(f);
    let pf = pliron_clif_opt::proots::facts_cfg(f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        f, &cfg, &pf, &ps,
        &[Value::from_u32(0)].into_iter().collect(),
        &FxHashSet::default(), &FxHashSet::default(),
        ofz, breg, rpar,
        &FxHashMap::default(), &FxHashMap::default(), &FxHashSet::default(),
    )
}

#[test]
fn merged_block_takes_the_outer_region() {
    let mut f = cranelift_reader::parse_functions(FN).unwrap().remove(0);
    let mut ctx = Context::new();
    let outer = BasicBlock::new(&mut ctx, None, vec![]);
    let r1 = BasicBlock::new(&mut ctx, None, vec![]);
    let r2 = BasicBlock::new(&mut ctx, None, vec![]);
    let mut rpar = pliron::cols::ColMap::default();
    rpar.insert(r1, outer);
    rpar.insert(r2, outer);
    let is = insts(&f);
    // load(R1), call(O), load(R2); the rest (pure, return) from O.
    let mut ireg: FxHashMap<Inst, Ptr<BasicBlock>> = FxHashMap::default();
    for (k, &i) in is.iter().enumerate() {
        ireg.insert(i, match k { 0 => r1, 2 => r2, _ => outer });
    }
    let breg = pliron_clif_opt::pipeline::live_regions(&f, &ireg, &rpar);
    let b0 = f.layout.entry_block().unwrap();
    assert_eq!(breg.get(&b0), Some(&outer), "merged block keeps a helper region");
    let ofz: FxHashMap<Value, Vec<(u64, Ptr<BasicBlock>)>> =
        [(Value::from_u32(0), vec![(24, r1), (24, r2)])].into_iter().collect();
    run(&mut f, &breg, &ofz, &rpar);
    let loads = insts(&f).iter().filter(|&&i| f.dfg.insts[i].opcode() == Opcode::Load).count();
    assert_eq!(loads, 2, "capacity reload forwarded across try_reserve:\n{}", f.display());
}

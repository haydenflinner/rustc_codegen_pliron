//! A region-frozen operand's locs are immune to stores *through other
//! pointers* while its clone runs (scoped `!noalias`). A store through
//! the frozen root itself to an overlapping loc must still kill it: it
//! is either a real write (the region tagging is stale — CFG merging can
//! leave `set_len`'s store in a block still tagged with the inlined
//! `len(&self)` clone's region) or UB, and killing is sound either way.
//!
//! Real-world shape: smallvec's `into_iter` reads `self.capacity`
//! (`len()`, `&self` → frozen), stores 0 through `select(spilled,
//! &heap_len, &capacity)` (`set_len(0)`), then copies `self` into the
//! iterator. Forwarding the copy's `capacity` load to the pre-store value
//! kept the length at 1 and dropped the element twice
//! (smallvec tests::into_iter_drop, harness/crates.py).

use cranelift_codegen::ir::{Block, Function, Value};
use pliron::basic_block::BasicBlock;
use pliron::context::Context;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

const FN: &str = "function %f(i64, i8) -> i64 fast {
block0(v0: i64, v1: i8):
    v2 = load.i64 notrap v0+24
    v3 = iconst.i64 8
    v4 = iadd v0, v3
    v5 = iconst.i64 24
    v6 = iadd v0, v5
    v7 = select v1, v4, v6
    v8 = iconst.i64 0
    store notrap v8, v7
    v9 = load.i64 notrap v0+24
    v10 = iadd v2, v9
    return v10
}";

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Load)
        .count()
}

#[test]
fn same_root_store_kills_region_frozen_loc() {
    let before = cranelift_reader::parse_functions(FN).unwrap().remove(0);
    let mut after = before.clone();
    let mut ctx = Context::new();
    let region = BasicBlock::new(&mut ctx, None, vec![]);
    let v0 = Value::from_u32(0);
    let ofz: FxHashMap<Value, Vec<(u64, _)>> = [(v0, vec![(32, region)])].into_iter().collect();
    let breg: FxHashMap<Block, _> = [(Block::from_u32(0), region)].into_iter().collect();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&after);
    let pf = pliron_clif_opt::proots::facts_cfg(&after, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut after,
        &cfg,
        &pf,
        &ps,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &ofz,
        &breg,
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    );
    assert_eq!(loads(&after), 2, "the post-store reload must stay:\n{}", after.display());
    let cfgc = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfgc) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

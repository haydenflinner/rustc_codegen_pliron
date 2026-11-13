//! loadfwd's PRE does not thread one value through a loop nest as block
//! params (`PLIRON_LF_TRIVPHI`, default on).
//!
//! A field loaded ahead of a loop nest (LICM hoisted it) and read again
//! in the outer body is available everywhere in the nest. The forward
//! used to give every header and join on the way a new param fed by that
//! one load or by another such param, and left the web for `sameargs2`
//! much later; meanwhile the inner loop carried the extra param and the
//! shape-matching passes in between refused it (regex's
//! `Fsm::next_state` under `PLIRON_LICM_INNERPRESS=2`: its varint loop
//! kept the iterator's `select`s, +1.07M instructions). A param web with
//! a single value flowing in is that value. Differentially checked.

use cranelift_codegen::ir::{Block, Function, Opcode, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

// block1 outer header, block2 inner loop, block3 outer body re-reading
// `v0+8` (already in `v3`), block5 reads it once more after the nest.
const NEST: &str = "function %t(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = load.i64 notrap v0+8
    v4 = iconst.i64 0
    jump block1(v4, v3)
block1(v5: i64, v6: i64):
    jump block2(v4, v6)
block2(v7: i64, v8: i64):
    v9 = iconst.i64 1
    v10 = iadd v7, v9
    v11 = iadd v8, v7
    v12 = icmp ult v10, v2
    brif v12, block2(v10, v11), block3
block3:
    v13 = load.i64 notrap v0+8
    v14 = iadd v11, v13
    v15 = iadd v5, v9
    v16 = icmp ult v15, v1
    brif v16, block1(v15, v14), block5
block5:
    v17 = load.i64 notrap v0+8
    v18 = iadd v14, v17
    return v18
}";

// The same nest with a store to the field in the outer body: the value
// on the back edge differs from the one on entry, so the outer header's
// param is real and must stay.
const STORED: &str = "function %t(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = load.i64 notrap v0+8
    v4 = iconst.i64 0
    jump block1(v4, v3)
block1(v5: i64, v6: i64):
    jump block2(v4, v6)
block2(v7: i64, v8: i64):
    v9 = iconst.i64 1
    v10 = iadd v7, v9
    v11 = iadd v8, v7
    v12 = icmp ult v10, v2
    brif v12, block2(v10, v11), block3
block3:
    v13 = load.i64 notrap v0+8
    v14 = iadd v11, v13
    store.i64 notrap v14, v0+8
    v15 = iadd v5, v9
    v16 = icmp ult v15, v1
    brif v16, block1(v15, v14), block5
block5:
    v17 = load.i64 notrap v0+8
    v18 = iadd v14, v17
    return v18
}";

fn run(src: &str) -> (Function, Function) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut f = before.clone();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    let noalias: FxHashSet<Value> = [Value::from_u32(0)].into_iter().collect();
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
        &FxHashMap::default(),
        &FxHashMap::default(),
    );
    (before, f)
}

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

fn params(f: &Function, b: u32) -> usize {
    f.dfg.block_params(Block::from_u32(b)).len()
}

fn same(before: &Function, after: &Function) {
    cranelift_codegen::verify_function(
        after,
        &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", after.display()));
    if let Err(m) = check(before, after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}

#[test]
fn one_value_through_a_nest_adds_no_params() {
    let (before, f) = run(NEST);
    assert_eq!(loads(&f), 1, "both re-reads forward:\n{}", f.display());
    for b in [1, 2, 5] {
        assert_eq!(params(&f, b), params(&before, b), "block{b} gained a param:\n{}", f.display());
    }
    same(&before, &f);
}

#[test]
fn a_value_that_changes_keeps_its_param() {
    let (before, f) = run(STORED);
    assert_eq!(loads(&f), 1, "{}", f.display());
    // Entry value and stored value meet at the outer header.
    assert_eq!(params(&f, 1), params(&before, 1) + 1, "{}", f.display());
    same(&before, &f);
}

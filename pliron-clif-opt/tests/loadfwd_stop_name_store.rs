//! `loadfwd` keys a loc under the address's `stop` root, and an
//! operand-frozen value is a stop: `store 7 -> v2` with `v2 = v0 + 24`
//! frozen is kept as `(V(v2), 0)`. A later store to the same bytes
//! through the plain name, `store 3 -> v0+24`, roots at `(V(v0), 24)`.
//! `kill` saw two different roots, found `V(v0)` (a `noalias` param that
//! never escapes) in `iso` and kept the stop-named loc, so `load v2`
//! forwarded the stale 7. The store arms' extra `na0` kill only covers a
//! store made *through* a stop name, not a loc kept under one
//! (`kill_stops`).
//!
//! Fuzz seed 16782: the stale value decided a `br_table` dispatch on a
//! loop's state word (`v6 + 24`), and the loop never exited.

use cranelift_codegen::ir::{Function, Value};
use pliron::basic_block::BasicBlock;
use pliron::context::Context;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

const FN: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 24
    v2 = iadd v0, v1
    v3 = iconst.i64 7
    store.i64 notrap v3, v2
    v4 = iconst.i64 3
    store.i64 notrap v4, v0+24
    v5 = load.i64 notrap v2
    return v5
}";

fn run(f: &mut Function) -> usize {
    let mut ctx = Context::new();
    let region = BasicBlock::new(&mut ctx, None, vec![]);
    // v2 = v0 + 24 is an operand-frozen operand (a `stop` root).
    let ofz: FxHashMap<Value, Vec<(u64, pliron::context::Ptr<BasicBlock>)>> =
        [(Value::from_u32(2), vec![(8, region)])].into_iter().collect();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(f);
    let pf = pliron_clif_opt::proots::facts_cfg(f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        f,
        &cfg,
        &pf,
        &ps,
        &[Value::from_u32(0)].into_iter().collect(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &ofz,
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    )
}

#[test]
fn plain_name_store_kills_stop_named_loc() {
    let before = cranelift_reader::parse_functions(FN).unwrap().remove(0);
    let mut after = before.clone();
    run(&mut after);
    let noalias: FxHashSet<Value> = [Value::from_u32(0)].into_iter().collect();
    let facts = Facts {
        noalias: Some(&noalias),
        ..Facts::default()
    };
    let cfg = Config {
        trials: 16,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &facts, &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

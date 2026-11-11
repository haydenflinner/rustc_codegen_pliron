//! licm's region-frozen early return skipped the clobber scan entirely:
//! it gated on `own_write`, an exact same-root test a `store` through an
//! unresolvable address (a `select` over arms) always fails — while
//! `clobbers` keeps that store as a may-write. A frozen load could then
//! hoist over a real in-loop write and freeze on the entry value
//! (num_bigint_dig `div_rem` livelocked: a SmallVec `len` the loop kept
//! rewriting through a `select`-rooted store).

use cranelift_codegen::ir::Value;
use pliron::basic_block::BasicBlock;
use pliron::context::Context;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

#[test]
fn frozen_hoist_must_still_run_the_clobber_scan() {
    // Bump `*(v0 + 16)` until it reaches 4. The read's address is
    // invariant (`v2 = v0 + 16`); the write goes through `v13`, a
    // `select` that picks `v2` every iteration — a root `norm_root`
    // cannot resolve, so only the full `clobbers` scan sees the write.
    let before = cranelift_reader::parse_functions(
        "function %f(i64) -> i64 {
        block0(v0: i64):
            v1 = iconst.i64 16
            v2 = iadd v0, v1
            v3 = iconst.i64 24
            v4 = iadd v0, v3
            v5 = iconst.i64 0
            jump block1(v5)

        block1(v10: i64):
            v6 = load.i64 notrap v2
            v7 = iconst.i64 4
            v8 = icmp uge v6, v7
            brif v8, block2(v10), block3

        block3:
            v9 = iconst.i64 1
            v11 = iadd v6, v9
            v12 = icmp ult v10, v7
            v13 = select v12, v2, v4
            store notrap v11, v13
            v14 = iadd v10, v9
            jump block1(v14)

        block2(v15: i64):
            return v15
        }",
    )
    .unwrap()
    .remove(0);
    let mut ctx = Context::new();
    let region = BasicBlock::new(&mut ctx, None, vec![]);
    let v6 = Value::from_u32(6);
    let load = before.dfg.value_def(v6).inst().unwrap();
    let v0 = Value::from_u32(0);
    // `region_frozen` (`scoped`) tags the load; `breg` puts the
    // preheader in the same region so the tag's in-region check passes.
    let scoped: FxHashMap<_, _> = [(load, region)].into_iter().collect();
    let block0 = cranelift_codegen::ir::Block::from_u32(0);
    let breg: FxHashMap<_, _> = [(block0, region)].into_iter().collect();
    // `deref` makes the address safe to speculate; `v0` is deliberately
    // *not* `noalias` — in the real case the slot escapes through a
    // drop call, so neither root is isolated.
    let deref: FxHashMap<Value, (u64, bool)> = [(v0, (32, true))].into_iter().collect();
    let mut after = before.clone();
    pliron_clif_opt::licm::run(
        &mut after,
        &mut Analysis::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &deref,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
        &scoped,
        &breg,
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        false,
    );
    // The select-rooted store writes the load's bytes: the load must
    // stay inside the loop.
    assert_eq!(
        after.layout.inst_block(load),
        Some(cranelift_codegen::ir::Block::from_u32(1)),
        "load hoisted over a may-alias store:\n{}",
        after.display()
    );
    let facts = Facts {
        deref: Some(&deref),
        ..Facts::default()
    };
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &facts, &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

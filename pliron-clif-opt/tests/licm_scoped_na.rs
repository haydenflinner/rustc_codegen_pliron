//! licm's load-hoist clobber scan treated a region-scoped `noalias` root
//! as function-wide isolated: `p.noalias` mixes rustc-level `noalias`
//! params with clone args seeded from `region_frozen`/`region_noalias`,
//! whose disjointness only holds inside the clone's region. A store in
//! the caller's own code rooting at the underlying object was then
//! judged disjoint from a load keyed under the scope name, so the load
//! hoisted out of the loop and froze on the entry value
//! (rustc_resolve's `resolve_imports` freed `0x8` re-`reserve`ing a Vec
//! in stage2).

use cranelift_codegen::ir::Value;
use pliron::context::Context;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

#[test]
fn scoped_noalias_root_is_not_function_wide_isolated() {
    // Bump `*(v0 + 8)` until it reaches 4. The read goes through `v4`
    // (a scope root: `noalias`-seeded, valid only inside its clone's
    // region), the write through `v0` — same bytes, different roots.
    let before = cranelift_reader::parse_functions(
        "function %f(i64) -> i64 {
        block0(v0: i64):
            v1 = iconst.i64 0
            jump block1(v1)

        block1(v2: i64):
            v3 = iconst.i64 8
            v4 = iadd v0, v3
            v5 = load.i64 notrap v4
            v6 = iconst.i64 4
            v7 = icmp uge v5, v6
            brif v7, block2(v2), block3

        block3:
            v8 = iconst.i64 1
            v9 = iadd v5, v8
            store notrap v9, v0+8
            v10 = iadd v2, v8
            jump block1(v10)

        block2(v11: i64):
            return v11
        }",
    )
    .unwrap()
    .remove(0);
    let mut ctx = Context::new();
    let _ = &mut ctx;
    let v4 = Value::from_u32(4);
    // `noalias` carries the scope root so `root()` stops at `v4`;
    // `deref` proves the load's address safe to speculate. In the real
    // pipeline the seed is region-scoped (`scoped_na`), which the hoist
    // must honor rather than treating `v4` as globally unreachable.
    let noalias: FxHashSet<Value> = [v4].into_iter().collect();
    let scoped_na: FxHashSet<Value> = [v4].into_iter().collect();
    let v0 = Value::from_u32(0);
    let deref: FxHashMap<Value, (u64, bool)> = [
        (v0, (16, true)),  // the object the loop reads and writes
        (v4, (16, false)), // `region_frozen` seed: readable, not writable
    ]
    .into_iter()
    .collect();
    let mut after = before.clone();
    pliron_clif_opt::licm::run(
        &mut after,
        &mut Analysis::default(),
        &noalias,
        &scoped_na,
        &deref,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
    );
    let facts = Facts {
        noalias: Some(&noalias),
        scoped_na: Some(&scoped_na),
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

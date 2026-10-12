//! licm's scalar promotion ignored accesses whose root is an
//! operand-frozen value: that name is distinct only inside its region, and
//! a loop re-enters the region each iteration, so the read must see the
//! previous iteration's store. Promoting the store held it in a register
//! and the cursor never advanced (aho-corasick's `ByteClassElements::next`
//! looped forever). Found by the differential checker.

use cranelift_codegen::ir::{Function, Value};
use pliron::basic_block::BasicBlock;
use pliron::context::Context;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

#[test]
fn promotion_respects_operand_frozen_reads_of_the_same_location() {
    // Advance a cursor at `*(v0 + 8)` until it reaches 4; count the steps.
    // The read goes through `v4` (operand-frozen in its region), the write
    // through `v0` (noalias): same bytes, different roots.
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
    let region = BasicBlock::new(&mut ctx, None, vec![]);
    let v0 = Value::from_u32(0);
    let noalias: FxHashSet<Value> = [v0].into_iter().collect();
    let deref: FxHashMap<Value, (u64, bool)> = [(v0, (16, true))].into_iter().collect();
    let ofz: FxHashMap<Value, Vec<(u64, _)>> = [(Value::from_u32(4), vec![(8, region)])]
        .into_iter()
        .collect();
    let mut after = before.clone();
    pliron_clif_opt::licm::run(
        &mut after,
        &mut Analysis::default(),
        &noalias,
        &deref,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &ofz,
    );
    let facts = Facts {
        noalias: Some(&noalias),
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

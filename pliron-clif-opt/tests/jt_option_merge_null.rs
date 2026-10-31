//! jumpthread's null-test folds see through the second level of an
//! `Option<&T>` merge. After if-conversion a zip/enumerate loop tests
//! `p == 0` on a param `p` with a single incoming edge whose arg is
//! `select (q != 0), q, 0`, `q` itself the first-level merge of the `Some`
//! pointer (an inbounds offset of a non-null base) and the `None` zero.
//! `select (q != 0), q, 0` is `q`, and `q == 0` is the guarding branch's
//! condition. num-bigint-dig's zip (blind-sig) and brotli's match loop kept
//! that null test on every element. Differentially checked.

use cranelift_codegen::ir::{Function, Opcode, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::FxHashSet;

const SRC: &str = "function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = icmp ult v2, v1
    brif v3, block2, block1
block1:
    v4 = iconst.i64 0
    jump block3(v4)
block2:
    v5 = iadd v0, v2
    jump block3(v5)
block3(v6: i64):
    v7 = iconst.i64 0
    v8 = icmp eq v6, v7
    v9 = iconst.i64 0
    v10 = iconst.i64 1
    v11 = select v8, v9, v10
    v12 = ireduce.i8 v11
    v13 = iconst.i8 1
    v14 = band v12, v13
    v15 = iconst.i64 0
    v16 = select v14, v6, v15
    jump block4(v16)
block4(v17: i64):
    v18 = iconst.i64 0
    v19 = icmp eq v17, v18
    brif v19, block5, block6
block5:
    v20 = iconst.i64 7
    return v20
block6:
    v21 = uload8.i64 notrap v17
    return v21
}";

fn icmp_on(f: &Function, v: Value) -> bool {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .any(|i| f.dfg.insts[i].opcode() == Opcode::Icmp && f.dfg.inst_args(i).contains(&v))
}

#[test]
fn second_level_option_null_test_folds() {
    // The fold is opt-in; this test binary holds only this test.
    unsafe { std::env::set_var("PLIRON_NULLSRC", "1") };
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    let v0 = Value::from_u32(0);
    let v5 = Value::from_u32(5);
    let nonnull: FxHashSet<Value> = [v0].into_iter().collect();
    let mut derived = pliron_clif_opt::vmap::VMap::default();
    derived.insert(v5, v0);
    pliron_clif_opt::jumpthread::run(&mut after, &nonnull, &mut derived, u32::MAX);
    assert!(!icmp_on(&after, Value::from_u32(17)), "null test on the merge kept:\n{}", after.display());
    let mut facts = Facts::default();
    let nn = nonnull.clone();
    facts.nonnull = Some(&nn);
    if let Err(m) = check(&before, &after, &facts, &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}

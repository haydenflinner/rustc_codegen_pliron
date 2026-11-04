//! Site-qualified facts (docs/codegen-plan.md "Direction", level 2): a
//! non-null fact that the plain value-keyed table cannot hold (its
//! justifying site does not cover every use of the value) is kept with
//! its site, and the null folds answer by dominance at the use.

use cranelift_codegen::ir::{Function, Inst, ProgramPoint, Value};
use pliron_clif_opt::jumpthread::{SiteFacts, fold_nulls_late};
use pliron_clif_opt::vmap::VMap;
use rustc_hash::FxHashSet;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// `v3` has no plain fact; `v1` is a `!nonnull` load. The test on `v3`
/// sits in block1, dominated by `v1`'s site.
const SRC: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v9: i64):
    v1 = load.i64 notrap v0
    v3 = load.i64 notrap v0+8
    brif v9, block1, block3
block1:
    v5 = iconst.i64 0
    v4 = icmp eq v3, v5
    brif v4, block2, block3
block2:
    return v1
block3:
    return v3
}";

fn val(n: u32) -> Value {
    Value::from_u32(n)
}

fn site_of(f: &Function, v: u32) -> ProgramPoint {
    let i: Inst = f.dfg.value_def(val(v)).inst().unwrap();
    ProgramPoint::Inst(i)
}

#[test]
fn plain_tables_alone_do_not_fold() {
    let mut f = parse(SRC);
    let loads: FxHashSet<Value> = [val(1)].into_iter().collect();
    let derived = VMap::default();
    let k = fold_nulls_late(&mut f, &loads, &derived, &SiteFacts::default());
    let text = f.display().to_string();
    assert_eq!(k, 0, "{text}");
    assert!(text.contains("icmp"), "the test must still be there:\n{text}");
}

#[test]
fn a_dominating_site_fact_folds_the_test() {
    let mut f = parse(SRC);
    let loads: FxHashSet<Value> = [val(1)].into_iter().collect();
    let derived = VMap::default();
    let mut sites = SiteFacts::default();
    // "v3 is non-null at every point v1's definition dominates": the
    // shape a merge of v3's uses onto a survivor records.
    sites.add_nonnull(val(3), site_of(&f, 1));
    let k = fold_nulls_late(&mut f, &loads, &derived, &sites);
    let text = f.display().to_string();
    assert!(k > 0, "{text}");
    assert!(!text.contains("icmp"), "the test is gone:\n{text}");
}

#[test]
fn a_site_that_does_not_dominate_the_use_does_not_fold() {
    let mut f = parse(SRC);
    let loads: FxHashSet<Value> = [val(1)].into_iter().collect();
    let derived = VMap::default();
    let mut sites = SiteFacts::default();
    // block2's `return` does not dominate block1's test.
    let ret2 = {
        let b2 = f.layout.blocks().nth(2).unwrap();
        ProgramPoint::Inst(f.layout.last_inst(b2).unwrap())
    };
    sites.add_nonnull(val(3), ret2);
    let k = fold_nulls_late(&mut f, &loads, &derived, &sites);
    assert_eq!(k, 0, "{}", f.display());
}

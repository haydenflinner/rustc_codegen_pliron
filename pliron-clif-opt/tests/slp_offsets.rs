//! slp must keep a memory access's immediate offset when it merges the
//! access into a vector one. Found by the differential checker in
//! regex-automata's `backtrack::Cache::new`: a `Vec` header lost its
//! dangling pointer because a pair of stores landed 16 bytes too low.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn slp_is_sound(src: &str) {
    let before = parse(src);
    let mut after = before.clone();
    let n = pliron_clif_opt::slp::run(
        &mut after,
        &mut Analysis::default(),
        &FxHashSet::default(),
        true,
    );
    assert!(
        n > 0,
        "slp did nothing; the test no longer covers it:\n{}",
        after.display()
    );
    let cfg = Config {
        trials: 32,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

#[test]
fn store_pair_keeps_immediate_offset() {
    slp_is_sound(
        "function %f(i64) {
        block0(v0: i64):
            v1 = iconst.i64 0
            store notrap v1, v0+16
            v2 = iconst.i64 24
            v3 = iadd v0, v2
            v4 = iadd v3, v2
            store notrap v1, v3
            store notrap v1, v4
            return
        }",
    );
}

#[test]
fn load_pair_keeps_immediate_offset() {
    // The extra use of v4 keeps the store-run path off this pair, so the
    // pairwise (xpack) path packs it, loads included.
    slp_is_sound(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = load.i64 notrap v1+16
            v3 = load.i64 notrap v1+24
            v9 = iconst.i64 5
            v4 = imul v2, v9
            v5 = imul v3, v9
            store notrap v4, v0+8
            v6 = iconst.i64 16
            v7 = iadd v0, v6
            store notrap v5, v7
            return v4
        }",
    );
}

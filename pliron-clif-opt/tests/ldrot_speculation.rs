//! ldrot carries `a[i]` from the previous iteration's `a[i+1]` and seeds
//! the first iteration with a load on the loop's entry edge. That seed must
//! not run when the loop may exit before reaching the load: with an empty
//! slice its address is a dangling pointer. Found by the differential
//! checker in regex-syntax's `IntervalSet` canonicalization.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const SUM_PAIRS: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v20 = load.i64 notrap v0
    v21 = load.i64 notrap v0+8
    v2 = iconst.i64 0
    jump block1(v2, v2)

block1(v3: i64, v4: i64):
    v5 = icmp uge v3, v21
    brif v5, block3(v4), block2

block2:
    v30 = iconst.i64 8
    v6 = imul v3, v30
    v7 = iadd v20, v6
    v8 = load.i64 notrap v7
    v9 = load.i64 notrap v7+8
    v10 = iadd v4, v8
    v11 = iadd v10, v9
    v31 = iconst.i64 1
    v12 = iadd v3, v31
    jump block1(v12, v11)

block3(v13: i64):
    return v13
}";

#[test]
fn rotated_load_is_not_hoisted_above_the_exit_test() {
    let before = parse(SUM_PAIRS);
    let mut after = before.clone();
    pliron_clif_opt::indvars::rotld(&mut after, &mut Analysis::default());
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

#[test]
fn rotated_load_still_fires_when_the_load_always_runs() {
    // Do-while form: the loads run before the exit test, so the entry
    // edge's seed load reads an address the original reads too.
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = iconst.i64 0
            jump block1(v2, v2)

        block1(v3: i64, v4: i64):
            v30 = iconst.i64 8
            v6 = imul v3, v30
            v7 = iadd v0, v6
            v8 = load.i64 notrap v7
            v9 = load.i64 notrap v7+8
            v10 = iadd v4, v8
            v11 = iadd v10, v9
            v31 = iconst.i64 1
            v12 = iadd v3, v31
            v32 = iconst.i64 7
            v33 = band v1, v32
            v5 = icmp ult v12, v33
            brif v5, block1(v12, v11), block3(v11)

        block3(v13: i64):
            return v13
        }",
    );
    let mut after = before.clone();
    assert!(
        pliron_clif_opt::indvars::rotld(&mut after, &mut Analysis::default()) > 0,
        "{}",
        after.display()
    );
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
}

//! `elim_shadowed` rewrites `iv <u bound` to `np <u base + bound*e`. A
//! loop that exits early on data may carry a `bound` far past the
//! object's extent — the compare is then reached with any `bound`
//! value, and `base + bound*e` can wrap below `np`, cutting the loop
//! to zero iterations. Found by the differential checker in core's
//! `SlicePartialEq::equal_same_length` (trial args had `bound` =
//! `i64::MAX`: `pend` wrapped to `base - 16` and the rewritten test
//! reported "past the end" on the first compare). The entry edge now
//! clamps `pend` to `u::MAX` on `bound*e` or `base + bound*e` overflow.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

#[test]
fn pend_clamps_on_bound_overflow() {
    // Pairwise-equal scan over 16-byte chunks: exits early when the
    // elements differ, so `v2` need not bound the memory at `v0`/`v1`.
    let before = parse(
        "function %f(i64, i64, i64) -> i8 {
        block0(v0: i64, v1: i64, v2: i64):
            v5 = iconst.i64 0
            jump block1(v5)

        block1(v6: i64):
            v7 = icmp ult v6, v2
            brif v7, block3, block2

        block2:
            v8 = iconst.i8 1
            return v8

        block3:
            v13 = iconst.i64 16
            v14 = imul.i64 v6, v13
            v15 = iadd.i64 v0, v14
            v19 = load.i64 notrap v15
            v16 = iconst.i64 16
            v17 = imul.i64 v6, v16
            v18 = iadd.i64 v1, v17
            v23 = load.i64 notrap v18
            v27 = icmp eq v19, v23
            v10 = iconst.i64 1
            v11 = iadd.i64 v6, v10
            brif v27, block1(v11), block8

        block8:
            v37 = iconst.i8 0
            return v37
        }",
    );
    let mut after = before.clone();
    let k = pliron_clif_opt::indvars::run(&mut after, &mut Analysis::default(), false);
    assert!(k > 0, "elim_shadowed did not fire\n{}", after.display());
    let cfg = Config {
        trials: 128,
        ..Config::default()
    };
    match check(&before, &after, &Facts::default(), &cfg) {
        Ok(s) => assert!(s.compared > 0, "{s:?}"),
        Err(m) => panic!("{m}\n---- after\n{}", after.display()),
    }
}

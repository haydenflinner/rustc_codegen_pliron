//! latchspec: a loop latch holding only a few pure instructions and a jump
//! to the header is folded into its predecessor's `brif`, so the back
//! edge is one branch. Differentially checked against the interpreter.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn run(src: &str) -> (Function, Function, usize) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::latchspec::run(&mut after, &mut Analysis::default());
    (before, after, k)
}

/// pulldown-cmark's escape scan: the latch recomputes the cursor and the
/// index the exit path needs.
const SCAN: &str = "function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 1
    jump block1(v0, v0)
block1(v4: i64, v5: i64):
    v6 = uload8.i64 notrap v4
    v7 = iconst.i64 34
    v8 = icmp eq v6, v7
    brif v8, block4(v5), block2
block2:
    v9 = iadd v4, v3
    v10 = icmp ne v9, v1
    brif v10, block3, block4(v9)
block3:
    v11 = iadd v4, v3
    v12 = isub v11, v2
    jump block1(v11, v12)
block4(v13: i64):
    return v13
}";

#[test]
fn latch_folds_into_brif() {
    let (before, after, k) = run(SCAN);
    assert_eq!(k, 1, "{}", after.display());
    assert!(!after.display().to_string().contains("block3:"), "latch kept:\n{}", after.display());
    let mut cfg = Config::default();
    cfg.trials = 64;
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

/// A trapping instruction in the latch must not run on the exit path.
#[test]
fn trapping_latch_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)
block1(v4: i64):
    v10 = icmp ne v4, v1
    brif v10, block3, block4(v4)
block3:
    v11 = udiv v1, v4
    jump block1(v11)
block4(v13: i64):
    return v13
}");
    assert_eq!(k, 0, "{}", after.display());
}

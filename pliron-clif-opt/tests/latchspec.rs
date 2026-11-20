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

fn run_live(src: &str) -> (Function, Function, usize) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::latchspec::run_with(&mut after, &mut Analysis::default(), true);
    (before, after, k)
}

/// `PLIRON_LATCHSPEC_LIVE`: the escape scan's exit edge reads nothing the
/// fold redefines, so it still folds.
#[test]
fn live_rule_keeps_the_scan_fold() {
    let (before, after, k) = run_live(SCAN);
    assert_eq!(k, 1, "{}", after.display());
    let mut cfg = Config::default();
    cfg.trials = 64;
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

/// brotli's `is_mostly_utf8`: the other path out of the latch's
/// predecessor reads the old index, so folding would keep the old and the
/// new index live together and cost a move per parameter on the back edge
/// (plus the split edge's jump). Both latches (`block3`, `block4`) have
/// that shape: with the rule both stay, without it both fold.
const UTF8: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v4: i64, v5: i64):
    v10 = icmp ult v4, v1
    brif v10, block2, block5(v5)
block2:
    v6 = iadd v0, v4
    v7 = uload8.i64 notrap v6
    v8 = iconst.i64 127
    v9 = icmp ule v7, v8
    brif v9, block3, block4
block3:
    v3 = iconst.i64 1
    v11 = iadd v4, v3
    v12 = iadd v5, v3
    jump block1(v11, v12)
block4:
    v13 = isub v1, v4
    v14 = iadd v5, v13
    v15 = iconst.i64 2
    v16 = iadd v4, v15
    jump block1(v16, v14)
block5(v17: i64):
    return v17
}";

#[test]
fn live_rule_keeps_a_latch_whose_old_values_live_on() {
    let (_, after, k) = run_live(UTF8);
    assert_eq!(k, 0, "{}", after.display());
    let (before, after, k) = run(UTF8);
    assert_eq!(k, 2, "{}", after.display());
    let mut cfg = Config::default();
    cfg.trials = 64;
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}


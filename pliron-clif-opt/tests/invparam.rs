//! invparam: an inner-loop header param that every back edge passes back
//! unchanged moves onto a preheader, so the back edge carries only the
//! varying values -- differentially checked against the interpreter.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// serde_json's escape scan: the outer loop re-enters the inner scan with a
/// new base `v2`; the inner loop passes `v2` and `v3` back unchanged.
const SCAN: &str = "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v9: i64):
    v4 = iconst.i64 0
    jump block1(v4, v0, v1, v4)

block1(v5: i64, v2: i64, v3: i64, v20: i64):
    v6 = iadd v2, v5
    v7 = uload8.i64 notrap v6
    v101 = iconst.i64 34
    v8 = icmp eq v7, v101
    brif v8, block3(v5, v2, v3, v20), block2

block2:
    v102 = iconst.i64 1
    v10 = iadd v5, v102
    v11 = icmp ult v10, v3
    brif v11, block1(v10, v2, v3, v20), block4(v20)

block3(v12: i64, v13: i64, v14: i64, v21: i64):
    v103 = iconst.i64 1
    v15 = iadd v12, v103
    v16 = iadd v13, v15
    v17 = isub v14, v15
    v104 = iconst.i64 1
    v22 = iadd v21, v104
    v105 = iconst.i64 0
    v18 = icmp eq v17, v105
    v19 = iconst.i64 0
    brif v18, block4(v22), block1(v19, v16, v17, v22)

block4(v30: i64):
    return v30
}
";

#[test]
fn inner_invariants_move_to_preheader() {
    let before = parse(SCAN);
    let mut after = before.clone();
    let k = pliron_clif_opt::invparam::run(&mut after, &mut Analysis::default());
    assert!(k > 0, "nothing moved:\n{}", after.display());
    let text = after.display().to_string();
    // The inner back edge now passes only the induction variable.
    assert!(
        text.lines().any(|l| l.contains("brif v11") && l.matches(',').count() <= 3),
        "back edge still carries invariants:\n{text}"
    );
    let mut facts = Facts::default();
    let mut cfg = Config::default();
    let _ = (&mut facts, &mut cfg);
    if let Err(m) = check(&before, &after, &facts, &cfg) {
        panic!("{m}\n---- after\n{text}");
    }
}

/// A param the back edge changes stays on the header.
#[test]
fn varying_param_stays() {
    let mut f = parse(
        "
function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0, v1)

block1(v2: i64, v3: i64):
    v106 = iconst.i64 1
    v4 = iadd v2, v106
    v107 = iconst.i64 2
    v5 = iadd v3, v107
    v6 = icmp ult v4, v5
    brif v6, block1(v4, v5), block2(v4)

block2(v7: i64):
    return v7
}",
    );
    assert_eq!(pliron_clif_opt::invparam::run(&mut f, &mut Analysis::default()), 0, "{}", f.display());
}

//! The egraph step's boundary re-canonicalization (docs/codegen-plan.md
//! "Direction", json's mechanism): Cranelift's egraph folds `brif (icmp
//! ne x, 0)` to `brif x`, and pliron's branch-reasoning passes key on the
//! compare. After the step, a branch on an integer wider than i8 is
//! spelled back as a compare. Pure CLIF-level check of the step's
//! post-pass through the public pipeline entry: a function with the
//! folded shape goes in, the compare comes out.

use cranelift_codegen::ir::Function;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

#[test]
fn wide_branch_condition_becomes_a_compare() {
    let mut f = parse("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = isub v0, v1
    brif v2, block1, block2
block1:
    return v0
block2:
    return v1
}");
    let k = pliron_clif_opt::pipeline::egraph_recanon_for_test(&mut f);
    let text = f.display().to_string();
    assert_eq!(k, 1, "{text}");
    assert!(text.contains("icmp ne v2, "), "{text}");
    assert!(!text.contains("brif v2,"), "{text}");
}

#[test]
fn boolean_conditions_are_left_alone() {
    let mut f = parse("function %f(i8, i64) -> i64 {
block0(v0: i8, v1: i64):
    brif v0, block1, block2
block1:
    return v1
block2:
    v3 = iconst.i64 0
    return v3
}");
    let k = pliron_clif_opt::pipeline::egraph_recanon_for_test(&mut f);
    assert_eq!(k, 0, "{}", f.display());
}

//! `opts/pliron.isle`: clifpeep rewrites migrated into the egraph
//! (`PLIRON_EGRAPH_RULES=1`). Each test is one migrated family; the Rust
//! rule stays until its `rule:<name>` counter is zero on the corpus.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

/// Run Cranelift's mid-end (the egraph) alone and return the CLIF text.
fn opt(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_EGRAPH_RULES", "1") };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.optimize(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.func.display().to_string()
}

#[test]
fn addnorm_folds_a_constant_chain_once() {
    let t = opt("function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 8
    v2 = iadd v0, v1
    v3 = iconst.i64 3
    v4 = isub v2, v3
    v5 = iconst.i64 16
    v6 = iadd v4, v5
    return v6
}");
    assert!(t.contains("iconst.i64 21"), "8 - 3 + 16 folded to one constant:\n{t}");
    assert_eq!(t.lines().filter(|l| l.contains("= iadd") || l.contains("= isub")).count(), 1, "{t}");
}

#[test]
fn icmpk_moves_the_constant_across_the_equality() {
    let t = opt("function %f(i64) -> i8 {
block0(v0: i64):
    v1 = iconst.i64 5
    v2 = iadd v0, v1
    v3 = iconst.i64 12
    v4 = icmp ne v2, v3
    return v4
}");
    assert!(t.contains("iconst.i64 7"), "12 - 5:\n{t}");
    assert_eq!(t.lines().filter(|l| l.contains("= iadd")).count(), 0, "the add is gone:\n{t}");
}

#[test]
fn icmpk_drops_a_zero_compared_difference() {
    let t = opt("function %f(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = isub v0, v1
    v3 = iconst.i64 0
    v4 = icmp eq v2, v3
    return v4
}");
    assert!(t.contains("icmp eq v0, v1"), "{t}");
    assert_eq!(t.lines().filter(|l| l.contains("= isub")).count(), 0, "{t}");
}

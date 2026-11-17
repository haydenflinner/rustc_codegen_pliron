//! `PLIRON_FCMPFOLD=1` and `PLIRON_FMUL1=1` (both opt-in): the egraph
//! rules of `opts/pliron.isle` (`fcmpfold`, `fmul1`) and the constant
//! `fcmp` that `jumpthread::eval` (so `unroll`'s trip walk) now decides.
//! nalgebra's small matrix products need all of it: `gemv` tests
//! `!beta.is_zero()` on a constant `beta` and multiplies by `alpha = 1.0`.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::settings::{self, Configurable};
use pliron_clif_opt::pipeline::Analysis;

fn knobs() {
    unsafe {
        std::env::set_var("PLIRON_FCMPFOLD", "1");
        std::env::set_var("PLIRON_FMUL1", "1");
    }
}

/// Cranelift's mid-end (the egraph) alone; the CLIF text.
fn opt(clif: &str) -> String {
    knobs();
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

fn cmp_fn(ty: &str, cc: &str, a: &str, b: &str) -> String {
    format!(
        "function %f() -> i8 {{
block0:
    v0 = {ty}const {a}
    v1 = {ty}const {b}
    v2 = fcmp {cc} v0, v1
    return v2
}}"
    )
}

/// The returned constant, if the compare folded.
fn folded(t: &str) -> Option<i64> {
    if t.contains("fcmp") {
        return None;
    }
    let l = t.lines().find(|l| l.contains("iconst.i8"))?;
    l.split_whitespace().last()?.parse().ok()
}

#[test]
fn fcmp_of_two_constants_folds() {
    for (ty, cc, a, b, want) in [
        ("f32", "eq", "0.0", "0.0", 1),
        ("f32", "eq", "0x1.0p0", "0.0", 0),
        ("f32", "eq", "-0.0", "0.0", 1),
        ("f32", "ne", "0x1.0p0", "0.0", 1),
        ("f32", "lt", "-0x1.0p0", "0.0", 1),
        ("f32", "ge", "-0x1.0p0", "0.0", 0),
        ("f64", "gt", "0x1.8p1", "0x1.0p1", 1),
        ("f64", "le", "0x1.8p1", "0x1.0p1", 0),
        // An unordered pair: only the unordered codes and `ne` hold.
        ("f32", "eq", "+NaN", "+NaN", 0),
        ("f32", "ne", "+NaN", "0.0", 1),
        ("f32", "uno", "+NaN", "0.0", 1),
        ("f32", "ord", "+NaN", "0.0", 0),
        ("f64", "lt", "+NaN", "0.0", 0),
        ("f64", "ult", "+NaN", "0.0", 1),
        ("f64", "one", "+NaN", "0.0", 0),
        ("f64", "ueq", "+NaN", "0.0", 1),
        ("f32", "one", "0x1.0p0", "0.0", 1),
        ("f32", "ugt", "0.0", "0.0", 0),
    ] {
        let t = opt(&cmp_fn(ty, cc, a, b));
        assert_eq!(folded(&t), Some(want), "fcmp {cc} {ty} {a}, {b}:\n{t}");
    }
}

/// A compare with one unknown operand stays.
#[test]
fn fcmp_of_a_parameter_stays() {
    let t = opt("function %f(f32) -> i8 {
block0(v0: f32):
    v1 = f32const 0.0
    v2 = fcmp eq v0, v1
    return v2
}");
    assert!(t.contains("fcmp eq"), "{t}");
}

#[test]
fn fmul_by_one_is_the_operand() {
    for (ty, one) in [("f32", "0x1.0p0"), ("f64", "0x1.0p0")] {
        for order in ["v0, v1", "v1, v0"] {
            let t = opt(&format!(
                "function %f({ty}) -> {ty} {{
block0(v0: {ty}):
    v1 = {ty}const {one}
    v2 = fmul {order}
    return v2
}}"
            ));
            assert!(!t.contains("fmul"), "{ty} fmul {order}:\n{t}");
            assert!(t.contains("return v0"), "{t}");
        }
    }
}

/// Other constants keep their multiply: -1.0 (sign), 2.0, and 1.0 plus
/// one ulp.
#[test]
fn fmul_by_other_constants_stays() {
    for c in ["-0x1.0p0", "0x1.0p1", "0x1.000002p0"] {
        let t = opt(&format!(
            "function %f(f32) -> f32 {{
block0(v0: f32):
    v1 = f32const {c}
    v2 = fmul v0, v1
    return v2
}}"
        ));
        assert!(t.contains("fmul"), "x * {c}:\n{t}");
    }
}

#[test]
fn fcmp_semantics_match_floatcc() {
    use pliron_clif_opt::jumpthread::fcmp;
    let n = f64::NAN;
    assert!(fcmp(FloatCC::NotEqual, n, n));
    assert!(!fcmp(FloatCC::OrderedNotEqual, n, n));
    assert!(fcmp(FloatCC::UnorderedOrLessThanOrEqual, n, 1.0));
    assert!(fcmp(FloatCC::UnorderedOrLessThanOrEqual, 1.0, 1.0));
    assert!(!fcmp(FloatCC::UnorderedOrGreaterThan, 1.0, 1.0));
    assert!(fcmp(FloatCC::Equal, -0.0, 0.0));
}

fn count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

/// A two-trip loop whose body branches on `fcmp eq lhs, 0.0`, `lhs` the
/// constant `c` or (`loaded`) a value loaded in the loop: decided, the
/// trip walk unrolls it; loaded, it stays.
fn loop_src(c: &str, loaded: bool) -> String {
    format!(
        "function u0:0(i64) fast {{
block0(v0: i64):
    v1 = iconst.i64 0
    v2 = f32const {c}
    jump block1(v1)

block1(v10: i64):
    v11 = load.f32 notrap v0
    v12 = f32const 0.0
    v13 = fcmp eq {}, v12
    brif v13, block2, block3

block2:
    store notrap v12, v0
    jump block4

block3:
    v14 = fadd v11, v11
    store notrap v14, v0
    jump block4

block4:
    v15 = iconst.i64 1
    v16 = iadd v10, v15
    v17 = iconst.i64 2
    v18 = icmp ult v16, v17
    brif v18, block1(v16), block5

block5:
    return
}}",
        if loaded { "v11" } else { "v2" }
    )
}

fn unroll(src: &str) -> (Function, usize) {
    knobs();
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let n = pliron_clif_opt::unroll::run_with(
        &mut f,
        &mut Analysis::default(),
        &rustc_hash::FxHashSet::default(),
        &pliron_clif_opt::vmap::VMap::default(),
        &std::collections::HashMap::default(),
        false,
    );
    pliron_clif_opt::jumpthread::remove_unreachable_blocks(&mut f);
    (f, n)
}

#[test]
fn unroll_decides_a_constant_fcmp() {
    let (f, n) = unroll(&loop_src("0x1.0p0", false));
    assert_eq!(n, 1, "{}", f.display());
    assert_eq!(count(&f, Opcode::Brif), 0, "{}", f.display());
    // 1.0 != 0.0: both trips take the `fadd` arm.
    assert_eq!(count(&f, Opcode::Fadd), 2, "{}", f.display());
}

#[test]
fn unroll_leaves_a_loaded_fcmp() {
    let (f, n) = unroll(&loop_src("0x1.0p0", true));
    assert_eq!(n, 0, "{}", f.display());
    assert_eq!(count(&f, Opcode::Brif), 2, "{}", f.display());
}

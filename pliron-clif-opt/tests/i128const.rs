//! i128 constant materialization: `iconst` tops out at i64 — folds that
//! replace an inst with a constant result must emit `iconcat lo, hi`
//! for i128 (rustlantis seeds 65/89 hit `iconst.i128`, an invalid
//! controlling type).

use cranelift_codegen::ir::Function;
use pliron_clif_opt::clifpeep;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("invalid IR: {e}\n{}", f.display());
    }
}

/// lcse's `(x + c…) - x → Σc` fold rewrote the `isub` as
/// `UnaryImm::iconst` at the result's own type — `iconst.i128` for a
/// 128-bit subtrahend. Only the `x - x → 0` arm is reachable at i128
/// width (iconcat addends stop the chain walk).
#[test]
fn lcse_isub_i128_folds_to_iconcat() {
    let src = "function %f(i128) -> i128 {
block0(v0: i128):
    v1 = isub v0, v0
    return v1
}";
    let mut f = parse(src);
    assert!(clifpeep::lcse(&mut f) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(!text.contains("iconst.i128"), "{text}");
    assert!(text.contains("iconcat"), "{text}");
}

/// bandk's `band (band x, k1), k2 → 0` fold (`k1 & k2 == 0`) went
/// through the same `iconst(result_ty)` path — same invalid type on i128.
#[test]
fn bandk_i128_zero_folds_to_iconcat() {
    let src = "function %f(i128) -> i128 {
block0(v0: i128):
    v1 = iconst.i64 255
    v2 = iconst.i64 0
    v3 = iconcat v1, v2
    v4 = band v0, v3
    v5 = iconst.i64 65280
    v6 = iconcat v5, v2
    v7 = band v4, v6
    return v7
}";
    let mut f = parse(src);
    assert!(clifpeep::run(&mut f, false) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(!text.contains("iconst.i128"), "{text}");
    assert!(text.contains("iconcat"), "{text}");
}

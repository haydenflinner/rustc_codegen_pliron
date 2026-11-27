//! `lcse`'s `(x*C)>>k` / `(x*C)/C` → `x` fold: it fires under `mul nuw`
//! (`vfacts.nw`, as lowering records for `gep nuw` index scales and
//! rustc's `unchecked_*`) or a `vmax` bound on `x`, and must NOT fire on
//! an unbounded `x` — the `len*size >> log2(size)` round-trip is a real
//! clamp there (montgomery's `min(n, len)` inside `data()[i..n+i]`).

use cranelift_codegen::ir::{Function, Opcode, Value};
use cranelift_reader::parse_functions;
use pliron_clif_opt::vfacts::{NSW, NUW, Nw, VFacts};

fn parse(src: &str) -> Function {
    parse_functions(src).unwrap().remove(0)
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("invalid IR: {e}\n{}", f.display());
    }
}

/// `(x*8)>>3` on a `mul nuw`: folds to `x` — the product can't lose
/// bits, so the shift extracts nothing.
#[test]
fn ushr_imul_nuw_folds() {
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v8 = iconst.i64 8
    v3 = iconst.i64 3
    v10 = imul v0, v8
    v11 = ushr v10, v3
    return v11
}";
    let mut f = parse(src);
    let mut vf = VFacts::default();
    vf.nw.insert(
        Value::from_u32(10),
        Nw {
            bits: NUW,
            op: Opcode::Imul,
            args: [Value::from_u32(0), Value::from_u32(8)],
        },
    );
    assert!(pliron_clif_opt::clifpeep::lcse_facts(&mut f, &vf) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        !text.lines().any(|l| l.contains(" = ushr")),
        "ushr(x*8, 3) under mul nuw did not fold to x:\n{text}"
    );
    assert!(
        text.lines().any(|l| l.contains("v11 -> v0")),
        "the ushr result did not alias to x:\n{text}"
    );
}

/// Same pair without `nuw` and with an unbounded `x` (a function
/// parameter): the fold must NOT fire — `(x*8)>>3` is `x & (2^61-1)`,
/// not `x`.
#[test]
fn ushr_imul_unbounded_stays() {
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v8 = iconst.i64 8
    v3 = iconst.i64 3
    v10 = imul v0, v8
    v11 = ushr v10, v3
    return v11
}";
    let mut f = parse(src);
    pliron_clif_opt::clifpeep::lcse(&mut f);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        text.lines().any(|l| l.contains(" = ushr")),
        "ushr(x*8, 3) folded on an unbounded x:\n{text}"
    );
}

/// Same pair, `x` provably small (`band`/`uload8` bound it below
/// `2^61`): folds with no `nuw` at all.
#[test]
fn ushr_imul_bounded_folds() {
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v20 = uload8.i64 v0
    v8 = iconst.i64 8
    v3 = iconst.i64 3
    v10 = imul v20, v8
    v11 = ushr v10, v3
    return v11
}";
    let mut f = parse(src);
    assert!(pliron_clif_opt::clifpeep::lcse(&mut f) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        !text.lines().any(|l| l.contains(" = ushr")),
        "ushr(x*8, 3) on a bounded x did not fold:\n{text}"
    );
}

/// `nsw` alone is not enough: a signed-no-wrap product can still lose
/// unsigned high bits.
#[test]
fn ushr_imul_nsw_only_stays() {
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v8 = iconst.i64 8
    v3 = iconst.i64 3
    v10 = imul v0, v8
    v11 = ushr v10, v3
    return v11
}";
    let mut f = parse(src);
    let mut vf = VFacts::default();
    vf.nw.insert(
        Value::from_u32(10),
        Nw {
            bits: NSW,
            op: Opcode::Imul,
            args: [Value::from_u32(0), Value::from_u32(8)],
        },
    );
    pliron_clif_opt::clifpeep::lcse_facts(&mut f, &vf);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        text.lines().any(|l| l.contains(" = ushr")),
        "ushr(x*8, 3) folded on nsw-only mul:\n{text}"
    );
}

/// The shift amount must match the multiplier: `(x*8)>>4` is a clamp,
/// not `x`, even under `nuw`.
#[test]
fn ushr_imul_mismatched_shift_stays() {
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v8 = iconst.i64 8
    v4 = iconst.i64 4
    v10 = imul v0, v8
    v11 = ushr v10, v4
    return v11
}";
    let mut f = parse(src);
    let mut vf = VFacts::default();
    vf.nw.insert(
        Value::from_u32(10),
        Nw {
            bits: NUW,
            op: Opcode::Imul,
            args: [Value::from_u32(0), Value::from_u32(8)],
        },
    );
    pliron_clif_opt::clifpeep::lcse_facts(&mut f, &vf);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        text.lines().any(|l| l.contains(" = ushr")),
        "ushr(x*8, 4) folded under nuw:\n{text}"
    );
}

/// `(x*C)/C` under `nuw` is `x`; a `0` divisor keeps its trap.
#[test]
fn udiv_imul_nuw_folds() {
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v8 = iconst.i64 8
    v10 = imul v0, v8
    v11 = udiv v10, v8
    return v11
}";
    let mut f = parse(src);
    let mut vf = VFacts::default();
    vf.nw.insert(
        Value::from_u32(10),
        Nw {
            bits: NUW,
            op: Opcode::Imul,
            args: [Value::from_u32(0), Value::from_u32(8)],
        },
    );
    assert!(pliron_clif_opt::clifpeep::lcse_facts(&mut f, &vf) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        !text.lines().any(|l| l.contains(" = udiv")),
        "udiv(x*8, 8) under mul nuw did not fold to x:\n{text}"
    );

    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v9 = iconst.i64 0
    v11 = udiv v0, v9
    return v11
}";
    let mut f = parse(src);
    pliron_clif_opt::clifpeep::lcse(&mut f);
    let text = f.display().to_string();
    assert!(
        text.lines().any(|l| l.contains(" = udiv")),
        "udiv x, 0 folded away its trap:\n{text}"
    );
}

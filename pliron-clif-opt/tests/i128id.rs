//! `clifpeep::i128_identities` (`PLIRON_I128ID=1`): identities on 128-bit
//! integers with `iconcat` constants. Shapes: foldhash's sponge
//! (`0 | (x << n)`), `x ^ 0`, `x + 0`, `x - 0`, `x & 0`, a non-zero
//! constant (stays) and `0 - x` (stays). The checker runs every rewritten
//! function against its original.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::clifpeep;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn run(op: &str, zero_first: bool, hi: i64) -> usize {
    let (a, b) = if zero_first { ("v4", "v6") } else { ("v6", "v4") };
    let src = format!(
        "function %f(i64, i64, i64) -> i64, i64 {{
    block0(v0: i64, v1: i64, v2: i64):
        v3 = iconst.i64 0
        v10 = iconst.i64 {hi}
        v4 = iconcat v3, v10
        v5 = iconcat v0, v1
        v6 = ishl v5, v2
        v7 = {op} {a}, {b}
        v8, v9 = isplit v7
        return v8, v9
    }}"
    );
    let before = parse(&src);
    let mut after = before.clone();
    let n = clifpeep::i128_identities(&mut after);
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("invalid after i128id: {e}\\n{after}"));
    check(&before, &after, &Facts::default(), &Config { trials: 64, ..Config::default() })
        .unwrap_or_else(|m| panic!("i128id changed behaviour ({op}): {m:?}\\n{after}"));
    n
}

#[test]
fn zero_identities_fold() {
    for op in ["bor", "bxor", "iadd", "band"] {
        assert_eq!(run(op, true, 0), 1, "{op} 0, x");
        assert_eq!(run(op, false, 0), 1, "{op} x, 0");
    }
    assert_eq!(run("isub", false, 0), 1, "isub x, 0");
}

#[test]
fn non_identities_stay() {
    assert_eq!(run("isub", true, 0), 0, "isub 0, x");
    for op in ["bor", "bxor", "iadd", "band"] {
        assert_eq!(run(op, true, 1), 0, "{op} with a non-zero high half");
    }
}

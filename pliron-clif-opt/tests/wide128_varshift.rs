//! `wide128` with `PLIRON_WIDE128_VARSHIFT=1`: an `i128` shift by an
//! unknown amount on word halves (foldhash's `sponge |= x << sponge_len`).
//! Differentially checked on random inputs: amounts 0, 1..63, 64, 65..127
//! (the amount is masked to 7 bits) all take their own path.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn run(src: &str) -> (Function, Function, usize) {
    unsafe { std::env::set_var("PLIRON_WIDE128_VARSHIFT", "1") };
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::wide128::run(&mut after, true);
    (before, after, k)
}

fn checked(before: &Function, after: &Function) {
    let cfg = Config { trials: 2048, ..Config::default() };
    if let Err(m) = check(before, after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

#[test]
fn shl_of_a_word_by_any_amount() {
    let (before, after, k) = run("function %f(i64, i64, i64, i8) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i8):
    v4 = uextend.i128 v0
    v5 = iconcat v1, v2
    v6 = iconst.i8 127
    v7 = band v3, v6
    v8 = uextend.i128 v7
    v9 = ishl v4, v8
    v10 = bor v5, v9
    v11, v12 = isplit v10
    v13 = iconst.i64 3
    v14 = imul v12, v13
    v15 = bxor v11, v14
    return v15
}");
    assert!(k > 0, "{}", after.display());
    assert!(!after.display().to_string().contains("ishl.i128") && !after.display().to_string().contains("= ishl v4"), "{}", after.display());
    checked(&before, &after);
}

#[test]
fn ushr_of_a_pair_by_any_amount() {
    let (before, after, k) = run("function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconcat v0, v1
    v4 = uextend.i128 v2
    v5 = ushr v3, v4
    v6, v7 = isplit v5
    v8 = iconst.i64 5
    v9 = imul v7, v8
    v10 = bxor v6, v9
    return v10
}");
    assert!(k > 0, "{}", after.display());
    checked(&before, &after);
}

#[test]
fn shl_of_a_pair_by_any_amount() {
    let (before, after, k) = run("function %f(i64, i64, i32) -> i64 {
block0(v0: i64, v1: i64, v2: i32):
    v3 = iconcat v0, v1
    v4 = uextend.i128 v2
    v5 = ishl v3, v4
    v6, v7 = isplit v5
    v8 = iconst.i64 7
    v9 = imul v7, v8
    v10 = bxor v6, v9
    return v10
}");
    assert!(k > 0, "{}", after.display());
    checked(&before, &after);
}

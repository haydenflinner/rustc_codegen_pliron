//! A header `iv == limit` with `iv` starting at 0 and stepping by 1 is
//! the same trip as `iv < limit`. The reverse copy
//! `for i in (0..n) { a[(base - i)] }` uses that exit. A nonzero entry
//! can wrap past `limit`, so it stays uncounted.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Flags};
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn flags() -> Flags {
    settings::Flags::new(settings::builder())
}

/// `idx = end - i`, checked against 40, tripped by `i == n` from 0.
fn rev_copy(entry: &str, step: &str) -> String {
    format!(
        "function %f(i32, i32) -> i32 {{
    block0(v0: i32, v1: i32):
        {entry}
        jump block1(v2)

    block1(v3: i32):
        v4 = icmp eq v3, v0
        brif v4, block2, block3

    block3:
        v5 = isub v1, v3
        v6 = iconst.i32 40
        v7 = icmp uge v5, v6
        brif v7, block5, block4

    block5 cold:
        trap user1

    block4:
        {step}
        jump block1(v9)

    block2:
        return v3
}}"
    )
}

#[test]
fn zero_start_eq_is_a_counted_trip() {
    let mut f = parse(&rev_copy("v2 = iconst.i32 0", "v8 = iconst.i32 1\n        v9 = iadd v3, v8"));
    let n = pliron_clif_opt::bcheck::run(&mut f, &mut Analysis::default(), "t", true);
    let text = f.display().to_string();
    assert_eq!(n, 1, "{text}");
    cranelift_codegen::verify_function(&f, &flags()).unwrap_or_else(|e| panic!("{e}\n{text}"));
}

#[test]
fn nonzero_entry_stays_uncounted() {
    let mut f = parse(&rev_copy("v2 = iconst.i32 3", "v8 = iconst.i32 1\n        v9 = iadd v3, v8"));
    let n = pliron_clif_opt::bcheck::run(&mut f, &mut Analysis::default(), "t", true);
    assert_eq!(n, 0, "{}", f.display());
}

#[test]
fn step_two_stays_uncounted() {
    let mut f = parse(&rev_copy(
        "v2 = iconst.i32 0",
        "v8 = iconst.i32 2\n        v9 = iadd v3, v8",
    ));
    let n = pliron_clif_opt::bcheck::run(&mut f, &mut Analysis::default(), "t", true);
    assert_eq!(n, 0, "{}", f.display());
}

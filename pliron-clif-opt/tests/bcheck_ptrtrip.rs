//! A header `ptr == base + n*stride` with `ptr` stepping by `stride` runs
//! `n` times. An index stepping by 1 beside it is `entry .. entry+n`, so
//! `idx < 40` hoists. A shift that is not the stride stays uncounted.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Flags};
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap_or_else(|e| panic!("{e}\n{src}")).remove(0)
}

fn flags() -> Flags {
    settings::Flags::new(settings::builder())
}

fn walk(setup: &str, idx_step: &str) -> String {
    format!(
        "function %f(i32, i32) -> i32 {{
    block0(v0: i32, v1: i32):
        {setup}
        jump block1(v2, v5)

    block1(v6: i32, v7: i32):
        v8 = icmp eq v6, v4
        brif v8, block2, block3

    block3:
        v9 = iconst.i32 40
        v10 = icmp uge v7, v9
        brif v10, block5, block4

    block5 cold:
        trap user1

    block4:
        v11 = iconst.i32 4
        v12 = iadd v6, v11
        {idx_step}
        jump block1(v12, v14)

    block2:
        return v7
}}"
    )
}

fn run(src: &str) -> (usize, String) {
    let mut f = parse(src);
    let n = pliron_clif_opt::bcheck::run(&mut f, &mut Analysis::default(), "t", true);
    let text = f.display().to_string();
    if n > 0 {
        cranelift_codegen::verify_function(&f, &flags()).unwrap_or_else(|e| panic!("{e}\n{text}"));
    }
    (n, text)
}

const STEP1: &str = "v13 = iconst.i32 1\n        v14 = iadd v7, v13";

#[test]
fn zero_base_shift_is_a_counted_trip() {
    let (n, text) = run(&walk(
        "v2 = iconst.i32 0\n        v3 = iconst.i32 2\n        v4 = ishl v0, v3\n        v5 = iconst.i32 0",
        STEP1,
    ));
    assert_eq!(n, 1, "{text}");
}

#[test]
fn index_entry_is_added_into_the_guard() {
    let (n, text) = run(&walk(
        "v2 = iconst.i32 100\n        v3 = iconst.i32 2\n        v15 = ishl v0, v3\n        v4 = iadd v2, v15\n        v16 = iconst.i32 0\n        v5 = iadd v1, v16",
        STEP1,
    ));
    assert_eq!(n, 1, "{text}");
    assert!(text.contains("uadd_overflow"), "{text}");
}

#[test]
fn equal_constants_are_the_same_base() {
    let (n, text) = run(&walk(
        "v2 = iconst.i32 0\n        v17 = iconst.i32 0\n        v3 = iconst.i32 2\n        v15 = ishl v0, v3\n        v4 = iadd v17, v15\n        v5 = iconst.i32 0",
        STEP1,
    ));
    assert_eq!(n, 1, "{text}");
}

#[test]
fn different_constants_stay_uncounted() {
    let (n, text) = run(&walk(
        "v2 = iconst.i32 0\n        v17 = iconst.i32 8\n        v3 = iconst.i32 2\n        v15 = ishl v0, v3\n        v4 = iadd v17, v15\n        v5 = iconst.i32 0",
        STEP1,
    ));
    assert_eq!(n, 0, "{text}");
}

#[test]
fn shift_wider_than_the_stride_stays_uncounted() {
    let (n, text) = run(&walk(
        "v2 = iconst.i32 0\n        v3 = iconst.i32 3\n        v4 = ishl v0, v3\n        v5 = iconst.i32 0",
        STEP1,
    ));
    assert_eq!(n, 0, "{text}");
}

#[test]
fn index_step_two_stays_uncounted() {
    let (n, text) = run(&walk(
        "v2 = iconst.i32 0\n        v3 = iconst.i32 2\n        v4 = ishl v0, v3\n        v5 = iconst.i32 0",
        "v13 = iconst.i32 2\n        v14 = iadd v7, v13",
    ));
    assert_eq!(n, 0, "{text}");
}

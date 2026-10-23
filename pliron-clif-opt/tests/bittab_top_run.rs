//! `bittab` splits its accepted-value mask into runs with
//! `m >>= (!m).trailing_zeros()`. A run that reaches bit 63 makes that
//! shift 64: debug builds panic on the overflow, and release wraps the
//! amount to 0, leaving `m` unchanged, so the loop pushed runs until the
//! process was OOM-killed (11 GB in 2 s). Selfhost stage 2 died on it
//! compiling `rustc_hir_typeck`'s `FnCtxt::suggest_floating_point_literal`.
//! That needs the whole window accepted (`!m` has no zero below bit 64
//! only when `m` is all ones): the tree below accepts exactly `0..=63`.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::bittab;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const TREE: &str = "function %f(i32) -> i32 {
    block0(v0: i32):
        v1 = iconst.i32 31
        v2 = icmp ule v0, v1
        brif v2, block9, block1

    block1:
        v3 = iconst.i32 47
        v4 = icmp ule v0, v3
        brif v4, block9, block2

    block2:
        v5 = iconst.i32 63
        v6 = icmp ule v0, v5
        brif v6, block9, block8

    block8:
        v11 = iconst.i32 0
        return v11

    block9:
        v12 = iconst.i32 1
        return v12
    }";

#[test]
fn mask_run_ending_at_bit_63_terminates() {
    let before = parse(TREE);
    let mut after = before.clone();
    let n = bittab::run(&mut after);
    assert!(n > 0, "the tree should fold");
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags).unwrap();
    check(
        &before,
        &after,
        &Facts::default(),
        &Config {
            trials: 256,
            ..Config::default()
        },
    )
    .unwrap();
}

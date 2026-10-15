//! `brif_argspec` substituted `iconst 1` for the `brif` condition when the
//! condition value itself was threaded as an edge arg on the then-edge.
//! `brif arg` only proves `arg != 0` — for a non-boolean `arg` (a call
//! result, a peeled `select` condition) the substitution narrowed a real
//! value to 1. Found by the differential checker in pulldown_cmark's
//! `LineStart::scan_case_insensitive` (call result `v82` threaded through
//! `block12` to `return`: the run returned `1` instead of `35`).
//!
//! The falsy arm *does* prove `arg == 0`, so substituting `0` there stays.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::clifpeep;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

#[test]
fn then_edge_keeps_wide_condition() {
    let before = parse(
        "function %f(i8) -> i8 {
        block0(v0: i8):
            brif v0, block1(v0), block2

        block1(v1: i8):
            return v1

        block2:
            v3 = iconst.i8 0
            return v3
        }",
    );
    let mut after = before.clone();
    clifpeep::brif_fold(&mut after);
    // The then-edge must still thread `v0` itself — pre-fix it became
    // `iconst.i8 1`.
    let brif = after
        .layout
        .block_insts(after.layout.entry_block().unwrap())
        .next()
        .unwrap();
    let cranelift_codegen::ir::InstructionData::Brif { blocks, .. } = after.dfg.insts[brif] else {
        panic!("block0's terminator is not a brif")
    };
    let arg = blocks[0].args(&after.dfg.value_lists).next().unwrap();
    let cranelift_codegen::ir::BlockArg::Value(a) = arg else {
        panic!("edge arg is not a value")
    };
    assert!(
        matches!(
            after.dfg.value_def(after.dfg.resolve_aliases(a)),
            cranelift_codegen::ir::ValueDef::Param(..)
        ),
        "then-edge arg is no longer the param: {a}"
    );
    check(
        &before,
        &after,
        &Facts::default(),
        &Config {
            trials: 64,
            ..Config::default()
        },
    );
}

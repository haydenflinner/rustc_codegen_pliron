//! `plan_dead` follows at most `MAX_FWD` (8) lone-`jump` forwarders on a
//! dead loop's exit path. When the budget ran out while it was still on a
//! forwarder, `dest` had already stepped to the next block but the
//! resolved exit args still described the previous one: `apply_dead`
//! then indexed the new dest's params with the old args (index out of
//! bounds, or a fast edge binding the wrong values when the counts
//! happened to agree). Chains that long are what an exit looks like with
//! `PLIRON_JUMPTHREAD=0`, which leaves the forwarders jumpthread would
//! have threaded: the backend crashed on dsa's `mimas-compile` and std's
//! `from_iter_in_place` (wasm32), and `Path::normalize_lexically` failed
//! the verifier. A walk that runs out of budget now gives that exit up.

use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;
use target_lexicon::PointerWidth;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn run_idiom(f: &mut Function) -> usize {
    let tcfg = TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    };
    pliron_clif_opt::loopidiom::run(f, &mut Analysis::default(), &FxHashSet::default(), tcfg, "f")
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("{e}\n{}", f.display());
    }
}

/// A dead counting loop `for i in 0..v0 {}` whose exit passes the
/// counter through eight one-param forwarders `block10..block17`; the
/// last jumps to `block18`, whose signature is `last` (its own params and
/// the args `block17` passes).
fn src(last_params: &str, last_args: &str, last_body: &str) -> String {
    let mut s = String::from(
        "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v2 = iconst.i32 0
        jump block1(v2)

    block1(v3: i32):
        v5 = icmp uge v3, v0
        brif v5, block10(v3), block3

    block3:
        v9 = iconst.i32 1
        v6 = iadd v3, v9
        jump block1(v6)
",
    );
    for k in 10..17 {
        s += &format!("\n    block{k}(v{a}: i32):\n        jump block{n}(v{a})\n", a = 100 + k, n = k + 1);
    }
    s += &format!("\n    block17(v117: i32):\n        jump block18{last_args}\n");
    s += &format!("\n    block18{last_params}:\n{last_body}\n    }}");
    s
}

/// The chain ends in a block with no params: the stale exit arg (the
/// counter's closed form, resolved for `block17`) has no param to bind.
/// Before the fix: index out of bounds in `apply_dead`.
#[test]
fn dead_loop_long_forwarder_chain_to_paramless_block() {
    let before = parse(&src("", "", "        return v1"));
    let mut after = before.clone();
    run_idiom(&mut after);
    verify(&after);
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
}

/// The chain ends in a one-param block, but `block17` passes `v1`, not
/// the counter. The counts agree, so the stale exit arg (the counter's
/// closed form) would bind `block18`'s param on the fast edge: wrong
/// value, no crash.
#[test]
fn dead_loop_long_forwarder_chain_binds_the_right_value() {
    let before = parse(&src("(v118: i32)", "(v1)", "        return v118"));
    let mut after = before.clone();
    run_idiom(&mut after);
    verify(&after);
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
}

/// Seven forwarders (inside the budget) still get the fast edge.
#[test]
fn dead_loop_short_forwarder_chain_still_folds() {
    let mut s = String::from(
        "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v2 = iconst.i32 0
        jump block1(v2)

    block1(v3: i32):
        v5 = icmp uge v3, v0
        brif v5, block10(v3), block3

    block3:
        v9 = iconst.i32 1
        v6 = iadd v3, v9
        jump block1(v6)
",
    );
    for k in 10..16 {
        s += &format!("\n    block{k}(v{a}: i32):\n        jump block{n}(v{a})\n", a = 100 + k, n = k + 1);
    }
    s += "\n    block16(v116: i32):\n        return v116\n    }";
    let before = parse(&s);
    let mut after = before.clone();
    assert!(run_idiom(&mut after) > 0, "{}", after.display());
    verify(&after);
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
}

/// The exit forwarders are merges, and the scan from `dest` comes back
/// round to the first one through an outer loop (the wasm
/// `literate_eval::isolate::top_stmts` shape under
/// `PLIRON_JUMPTHREAD=0`). `block10` (preds: the loop exit and
/// `block11`) reads its own param `v10` in its `jump`; the old scan made
/// that an escape, so `block13` gained a param that the old edge from
/// `block12` bound to `v10`, which `block10` does not dominate
/// (`block12` is also entered from `block22`): a verifier error.
#[test]
fn dead_loop_merge_forwarders_reached_from_an_outer_loop() {
    let before = parse(
        "function %f(i32, i32) -> i32 {
    block0(v0: i32, v1: i32):
        v2 = iconst.i32 0
        jump block20(v1)

    block20(v20: i32):
        brif v20, block21, block22

    block21:
        jump block1(v2)

    block1(v3: i32):
        v5 = icmp uge v3, v0
        brif v5, block10(v3), block3

    block3:
        v9 = iconst.i32 1
        v6 = iadd v3, v9
        jump block1(v6)

    block22:
        brif v1, block11, block12(v1)

    block11:
        jump block10(v1)

    block10(v10: i32):
        jump block12(v10)

    block12(v12: i32):
        jump block13

    block13:
        v30 = iconst.i32 -1
        v31 = iadd v20, v30
        brif v31, block20(v31), block14

    block14:
        return v1
    }",
    );
    let mut after = before.clone();
    run_idiom(&mut after);
    verify(&after);
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
}

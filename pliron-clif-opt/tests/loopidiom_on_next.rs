//! `on_next` means the exit test reads the value a latch feeds back —
//! `iv + step`. A test on `iv + K` with `K != step` miscounts the trips
//! (`iv + 5 < n` stepping 1 writes ~5 fewer stores than the `iv + step`
//! model predicts), and the model's memset then runs past the region the
//! loop actually filled.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;
use target_lexicon::PointerWidth;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn tcfg() -> TargetFrontendConfig {
    TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    }
}

fn run_idiom(f: &mut Function) -> usize {
    pliron_clif_opt::loopidiom::run(
        f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        tcfg(),
        "f",
    )
}

fn has_call(f: &Function) -> bool {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .any(|i| f.dfg.insts[i].opcode() == Opcode::Call)
}

/// `iv += 1`; exits when `iv + K >= n` (K written next to the loop).
fn loop_src(k: i64) -> String {
    format!(
        "function %f(i64, i64) {{
        block0(v0: i64, v1: i64):
            v2 = iconst.i64 0
            jump block1(v2)

        block1(v4: i64):
            v5 = iadd v0, v4
            v6 = iconst.i8 0xAB
            store.i8 notrap v6, v5
            v7 = iconst.i64 1
            v8 = iadd v4, v7
            v9 = iconst.i64 {k}
            v10 = iadd v4, v9
            v11 = icmp ult v10, v1
            brif v11, block1(v8), block2

        block2:
            return
        }}"
    )
}

#[test]
fn shifted_exit_test_is_not_miscounted() {
    // `iv + 5 < n` stepping 1: the test reads `iv + K`, `K != step`.
    // Old behavior treated `on_next` as `iv + step` and emitted a memset
    // whose length overshot the loop's stores by ~K bytes.
    let mut f = parse(&loop_src(5));
    assert_eq!(run_idiom(&mut f), 0, "{}", f.display());
}

#[test]
fn post_step_exit_test_still_idioms() {
    // `iv + 1 < n` stepping 1: genuinely tests the fed-back value.
    let before = parse(&loop_src(1));
    let mut after = before.clone();
    assert!(run_idiom(&mut after) > 0, "{}", after.display());
    assert!(has_call(&after), "{}", after.display());
    check(&before, &after, &Facts::default(), &Config::default()).unwrap();
}

/// `dead` loops delete the body and jump straight to the exit dest. A
/// value defined inside the loop may still be used there — it dominated
/// the dest — so it has to become a closed-form edge arg. Internal block
/// params were invisible to the escape scan (`b == h` only), and a fast
/// edge jumping past them left a dangling use.
#[test]
fn dead_loop_refuses_internal_param_escape() {
    let mut f = parse(
        "function %f(i64) -> i64 {
        block0(v0: i64):
            v1 = iconst.i64 0
            jump block1(v1)

        block1(v2: i64):
            v3 = iconst.i64 1
            v4 = iadd v2, v3
            v5 = iconst.i64 77
            jump block5(v5)

        block5(v13: i64):
            v14 = icmp ult v4, v0
            brif v14, block1(v4), block6

        block6:
            return v13
        }",
    );
    assert_eq!(run_idiom(&mut f), 0, "{}", f.display());
}

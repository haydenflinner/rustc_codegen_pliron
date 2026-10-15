//! loopvec reductions whose accumulator reaches the latch (or the update
//! inst) through interior block params: the `if c { acc += d }` merge phi
//! kept inside the loop, and forwarder params feeding `acc ⊕ d` directly.
//! Each positive case must convert *and* differential-check equal.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};
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

fn run_vec(f: &mut Function, name: &str) -> usize {
    pliron_clif_opt::loopvec::run(
        f,
        &mut Analysis::default(),
        name,
        &FxHashSet::default(),
        tcfg(),
        true,
        false,
        true,
        &FxHashMap::default(),
        pliron_clif_opt::loopvec::WideCaps::BASE,
    )
}

fn has_vector_inst(f: &Function) -> bool {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .any(|i| {
            f.dfg
                .inst_results(i)
                .iter()
                .any(|&r| f.dfg.value_type(r).is_vector())
                || matches!(f.dfg.insts[i].opcode(), Opcode::Shuffle | Opcode::Splat)
        })
}

/// `if c { acc += x }` where the conditional merge survives inside the
/// loop as a phi: `body` brifs on a lane-local test, one arm computes
/// `acc + x`, the merge feeds it to the single latch. Vectorizes as a
/// masked sum.
#[test]
fn interior_merge_phi_conditional_update() {
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = iconst.i64 8
    v4 = imul v1, v3
    v5 = iadd v0, v4
    jump block1(v0, v2)
block1(v6: i64, v7: i64):
    v8 = icmp ult v6, v5
    brif v8, block2, block5
block2:
    v9 = load.i64 notrap v6
    v14 = iconst.i64 42
    v10 = icmp ugt v9, v14
    brif v10, block3, block4(v7)
block3:
    v11 = iadd v7, v9
    jump block4(v11)
block4(v12: i64):
    v13 = iadd v6, v3
    jump block1(v13, v12)
block5:
    return v7
}";
    let before = parse(src);
    let mut after = before.clone();
    let n = run_vec(&mut after, "phi_cond");
    assert!(
        n > 0,
        "interior-phi conditional update did not vectorize\n{}",
        after.display()
    );
    assert!(has_vector_inst(&after));
    let cfg = Config {
        trials: 32,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("mismatch: {m}\n---- after\n{}", after.display());
    }
}

/// The update inst reads a forwarder param: `upd = iadd q, x` where `q`
/// is an interior block param whose in-edges all supply `acc`. The param
/// is an acc alias — legal in the update, exempt from the interior-param
/// ban, and nowhere else read.
#[test]
fn update_reads_forwarded_acc_param() {
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = iconst.i64 8
    v4 = imul v1, v3
    v5 = iadd v0, v4
    jump block1(v0, v2)
block1(v6: i64, v7: i64):
    v8 = icmp ult v6, v5
    brif v8, block2, block5
block2:
    v9 = load.i64 notrap v6
    jump block3(v7)
block3(v10: i64):
    v11 = iadd v10, v9
    v12 = iadd v6, v3
    jump block1(v12, v11)
block5:
    return v7
}";
    let before = parse(src);
    let mut after = before.clone();
    let n = run_vec(&mut after, "fwd_acc");
    assert!(
        n > 0,
        "forwarded-acc update did not vectorize\n{}",
        after.display()
    );
    assert!(has_vector_inst(&after));
    let cfg = Config {
        trials: 32,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("mismatch: {m}\n---- after\n{}", after.display());
    }
}

/// A merge phi collecting the update on one arm and a NON-acc value on
/// the other is not a conditional update — the loop must stay scalar.
#[test]
fn mixed_phi_non_acc_arm_stays_scalar() {
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = iconst.i64 8
    v4 = imul v1, v3
    v5 = iadd v0, v4
    jump block1(v0, v2)
block1(v6: i64, v7: i64):
    v8 = icmp ult v6, v5
    brif v8, block2, block6
block2:
    v9 = load.i64 notrap v6
    v14 = iconst.i64 42
    v10 = icmp ugt v9, v14
    brif v10, block3, block4
block3:
    v11 = iadd v7, v9
    jump block5(v11)
block4:
    v15 = iconst.i64 8
    v16 = iadd v6, v15
    v17 = load.i64 notrap v16
    jump block5(v17)
block5(v12: i64):
    v13 = iadd v6, v3
    jump block1(v13, v12)
block6:
    return v7
}";
    let before = parse(src);
    let mut after = before.clone();
    let n = run_vec(&mut after, "mixed_phi");
    assert_eq!(
        n,
        0,
        "non-acc merge arm must not vectorize\n{}",
        after.display()
    );
}

/// `acc` forwarded through a phi and then read by a real body inst (not
/// just the update) — the read can't be satisfied lane-wise, scalar.
#[test]
fn acc_like_param_extra_use_stays_scalar() {
    let src = "function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v18: i64):
    v2 = iconst.i64 0
    v3 = iconst.i64 8
    v4 = imul v1, v3
    v5 = iadd v0, v4
    jump block1(v0, v2)
block1(v6: i64, v7: i64):
    v8 = icmp ult v6, v5
    brif v8, block2, block6
block2:
    v9 = load.i64 notrap v6
    jump block3(v7)
block3(v10: i64):
    store.i64 notrap v10, v18
    v11 = iadd v10, v9
    v12 = iadd v6, v3
    jump block1(v12, v11)
block6:
    return v7
}";
    let before = parse(src);
    let mut after = before.clone();
    let n = run_vec(&mut after, "acc_extra_use");
    assert_eq!(
        n,
        0,
        "acc read outside the update must not vectorize\n{}",
        after.display()
    );
}

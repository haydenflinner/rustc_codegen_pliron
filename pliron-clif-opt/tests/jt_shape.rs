//! `PLIRON_JT_SHAPE` (`brchain::run_with`): cluster chains, profile-ordered
//! tests and hot-case peeling. Each rewrite is compiled for the host and
//! run against the original on every index near the table plus the
//! unsigned/signed extremes.
#![cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    any(target_os = "macos", target_os = "linux")
))]

mod jtcommon;
use cranelift_codegen::ir::{Block, Function, InstructionData, Opcode};
use jtcommon::*;
use rustc_hash::FxHashMap;

/// `br_table v0, block1, [..]` with arms `targets[i]`; `blockK` returns
/// `x + 10*K`. Blocks `block1..=blockN` exist for `N = max target`.
fn table_fn(targets: &[u32]) -> String {
    let top = *targets.iter().max().unwrap().max(&1);
    let arms: Vec<String> = targets.iter().map(|t| format!("block{t}")).collect();
    let mut s = format!(
        "function %f(i32, i64) -> i64 {{\nblock0(v0: i32, v1: i64):\n    br_table v0, block1, [{}]\n",
        arms.join(", ")
    );
    for k in 1..=top {
        s += &format!(
            "block{k}:\n    v{a} = iconst.i64 {}\n    v{b} = iadd v1, v{a}\n    return v{b}\n",
            10 * k,
            a = 10 + 2 * k,
            b = 11 + 2 * k
        );
    }
    s + "}\n"
}

fn block(f: &Function, k: usize) -> Block {
    f.layout.blocks().nth(k).unwrap()
}

fn has_table(f: &Function) -> bool {
    f.layout
        .blocks()
        .filter_map(|b| f.layout.last_inst(b))
        .any(|i| f.dfg.insts[i].opcode() == Opcode::BrTable)
}

/// The first conditional test's taken target out of block0.
fn first_test_target(f: &Function) -> Block {
    let t = f.layout.last_inst(block(f, 0)).unwrap();
    match f.dfg.insts[t] {
        InstructionData::Brif { blocks, .. } => blocks[0].block(&f.dfg.value_lists),
        ref d => panic!("block0 ends in {d:?}"),
    }
}

fn same_behavior(before: &Function, after: &Function, n: i32) {
    let a = compile_func(before.clone());
    let b = compile_func(after.clone());
    let fa: extern "C" fn(i32, i64) -> i64 = unsafe { std::mem::transmute(a.entry()) };
    let fb: extern "C" fn(i32, i64) -> i64 = unsafe { std::mem::transmute(b.entry()) };
    let extremes = [i32::MIN, i32::MIN + 1, -2, -1, i32::MAX, i32::MAX - 1, 1 << 16];
    for i in (-3..n + 4).chain(extremes) {
        assert_eq!(fa(i, 1000), fb(i, 1000), "index {i}\n{}", after.display());
    }
}

fn prof(f: &Function, counts: &[(usize, u64)]) -> FxHashMap<Block, u64> {
    counts.iter().map(|&(k, c)| (block(f, k), c)).collect()
}

#[test]
fn few_clusters_become_a_range_chain() {
    // Live entries 8 (> brchain's 4), clusters 3: 0..=3, 6..=8, 11.
    let src = table_fn(&[2, 2, 2, 2, 1, 1, 3, 3, 3, 1, 1, 2]);
    let before = parse(&src);
    let mut off = before.clone();
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut off, None, false), 0);
    assert!(has_table(&off));
    let mut on = before.clone();
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut on, None, true), 1);
    assert!(!has_table(&on), "{}", on.display());
    same_behavior(&before, &on, 12);
}

#[test]
fn cluster_starting_at_nonzero_with_wrap() {
    // One cluster 5..=9: `x - 5 <= 4` must reject x < 5 (wraps high).
    let src = table_fn(&[1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 1, 1]);
    let before = parse(&src);
    let mut on = before.clone();
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut on, None, true), 1);
    assert!(!has_table(&on));
    same_behavior(&before, &on, 12);
}

#[test]
fn many_clusters_keep_the_table_without_profile() {
    let src = table_fn(&[2, 3, 4, 5, 6, 7, 8, 9]);
    let mut f = parse(&src);
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut f, None, true), 0);
    assert!(has_table(&f));
}

#[test]
fn dominant_target_is_peeled_ahead_of_the_table() {
    let src = table_fn(&[2, 3, 4, 5, 6, 7, 8, 9]);
    let before = parse(&src);
    let mut f = before.clone();
    let p = prof(&f, &[(0, 1000), (2, 10), (3, 10), (5, 900), (6, 10)]);
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut f, Some(&p), true), 1);
    assert!(has_table(&f), "peeling keeps the table");
    assert_eq!(first_test_target(&f), block(&before, 5));
    same_behavior(&before, &f, 8);
    // Off without the flag even with a profile.
    let mut g = before.clone();
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut g, Some(&p), false), 0);
}

#[test]
fn no_peel_below_share_or_count() {
    let src = table_fn(&[2, 3, 4, 5, 6, 7, 8, 9]);
    let before = parse(&src);
    let mut f = before.clone();
    let p = prof(&f, &[(0, 1000), (5, 500)]);
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut f, Some(&p), true), 0);
    let mut g = before.clone();
    let p = prof(&g, &[(0, 20), (5, 20)]);
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut g, Some(&p), true), 0);
}

#[test]
fn no_peel_when_the_hot_block_has_other_preds() {
    // block5 is reached from index 3 and also from block2: its count is
    // not the edge's, so it says nothing about the dispatch.
    let src = "function %f(i32, i64) -> i64 {
block0(v0: i32, v1: i64):
    br_table v0, block1, [block2, block3, block4, block5, block6, block7]
block1:
    return v1
block2:
    jump block5
block3:
    v3 = iconst.i64 3
    v4 = iadd v1, v3
    return v4
block4:
    v5 = iconst.i64 4
    v6 = iadd v1, v5
    return v6
block5:
    v7 = iconst.i64 5
    v8 = iadd v1, v7
    return v8
block6:
    v9 = iconst.i64 6
    v10 = iadd v1, v9
    return v10
block7:
    v11 = iconst.i64 7
    v12 = iadd v1, v11
    return v12
}
";
    let mut f = parse(src);
    let p = prof(&f, &[(0, 1000), (5, 990)]);
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut f, Some(&p), true), 0);
}

#[test]
fn profile_orders_a_short_chain() {
    // Three live entries: brchain makes a chain either way; the profile
    // puts index 2's target first.
    let src = table_fn(&[2, 3, 4]);
    let before = parse(&src);
    let mut f = before.clone();
    let p = prof(&f, &[(0, 1000), (2, 1), (3, 2), (4, 997)]);
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut f, Some(&p), true), 1);
    assert_eq!(first_test_target(&f), block(&before, 4));
    same_behavior(&before, &f, 3);
    let mut g = before.clone();
    assert_eq!(pliron_clif_opt::brchain::run_with(&mut g, None, false), 1);
    assert_eq!(first_test_target(&g), block(&before, 2));
}

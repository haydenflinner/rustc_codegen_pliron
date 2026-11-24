//! `PLIRON_LINDIS`: `a[i]` survives a store to `a[i + 1]`. Both addresses
//! are `p + (idx << 2)` with indices `i` and `i + 1`, which `root` sees as
//! two unrelated pointers. In their `base + scale * idx + k` form they
//! differ by exactly 4 bytes, so 4-byte accesses are disjoint. brotli's
//! literal-cost prefix sum (`literal_costs[i + 1] = literal_costs[i] + ..`)
//! reloaded `a[i]` after every store to `a[i + 1]` in its unrolled body.

use cranelift_codegen::ir::{Function, Opcode};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// Load `a[i]`, store `width` bytes at `a[i + k]`, load `a[i]` again.
fn src(k: i64, width: &str) -> String {
    format!(
        "function %t(i64, i64, f32) -> f32 {{
block0(v0: i64, v1: i64, v2: f32):
    v3 = iconst.i64 2
    v4 = ishl v1, v3
    v5 = iadd v0, v4
    v6 = load.f32 notrap v5
    v7 = iconst.i64 {k}
    v8 = iadd v1, v7
    v9 = ishl v8, v3
    v10 = iadd v9, v0
    v11 = {width}
    store notrap v11, v10
    v12 = load.f32 notrap v5
    v13 = fadd v6, v12
    return v13
}}"
    )
}

/// Loads left after loadfwd.
fn loads(src: &str) -> usize {
    unsafe { std::env::set_var("PLIRON_LINDIS", "1") };
    let mut f = parse(src);
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
    );
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

#[test]
fn next_element_store_keeps_load() {
    assert_eq!(loads(&src(1, "fadd v2, v2")), 1, "a[i] reloaded after a store to a[i + 1]");
    assert_eq!(loads(&src(-1, "fadd v2, v2")), 1, "a[i] reloaded after a store to a[i - 1]");
}

#[test]
fn overlapping_store_kills_load() {
    // 8 bytes at a[i - 1] cover a[i].
    assert_eq!(loads(&src(-1, "iconst.i64 7")), 2, "a store over a[i] did not kill it");
    // The same element through another index computation.
    assert_eq!(loads(&src(0, "fadd v2, v2")), 2, "a store to a[i + 0] did not kill a[i]");
}

//! A loop whose only exit leads to `resume`/`trap` is a cleanup loop (drops
//! on an unwind path), not dead code: every body block is a `deadend`, so
//! the effect scans skipped the whole body and `plan_dead` deleted the
//! loop. Real-world hit: rayon's `CollectResult<ZST>` drop loop on unwind
//! (`collect_drop_on_unwind_zst`: 741 inserts, 0 drops), 2026-10-21.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
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

/// `for _ in 0..n { DROPS.fetch_add(1) }` then `resume(exn)`.
const CLEANUP: &str = "function %f(i64, i64, i64) {
    sig0 = (i64)
    fn0 = %resume sig0
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    jump block1(v3)

block1(v4: i64):
    v5 = icmp eq v4, v1
    brif v5, block3, block2

block2:
    v6 = iconst.i64 1
    v7 = atomic_rmw.i64 add v0, v6
    v8 = iadd v4, v6
    jump block1(v8)

block3 cold:
    call fn0(v2)
    trap user2
}";

#[test]
fn cleanup_loop_survives() {
    let mut f = parse(CLEANUP);
    pliron_clif_opt::loopidiom::run(&mut f, &mut Analysis::default(), &FxHashSet::default(), tcfg(), "f");
    let atomics = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::AtomicRmw)
        .count();
    assert_eq!(atomics, 1, "cleanup loop's atomic was dropped:\n{}", f.display());
    // Still a loop: the header keeps its backedge.
    let h = f.layout.blocks().nth(1).unwrap();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    assert!(cfg.pred_iter(h).count() >= 2, "loop gone:\n{}", f.display());
}

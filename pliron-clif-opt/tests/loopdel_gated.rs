//! `PLIRON_LOOPDEL` is opt-in: without it the countdown fold is inert and
//! the loop survives (kept in its own binary — `pliron_cfg`'s env parse
//! is one-shot per process, so set/unset tests can't share one).

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Function;
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const COUNTDOWN: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v1)

block1(v10: i64):
    v11 = iconst.i64 1
    v12 = isub.i64 v10, v11
    store.i64 notrap v12, v0
    v13 = load.i64 notrap v0+8
    v14 = icmp.i64 uge v12, v11
    brif v14, block1(v12), block2(v13)

block2(v20: i64):
    return v20
}";

#[test]
fn gated_off_by_default() {
    let mut f = parse(COUNTDOWN);
    pliron_clif_opt::loopidiom::run(
        &mut f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        cranelift_codegen::isa::TargetFrontendConfig {
            default_call_conv: cranelift_codegen::isa::CallConv::SystemV,
            pointer_width: target_lexicon::PointerWidth::U64,
            page_size_align_log2: 12,
        },
        "f",
    );
    let cfg = ControlFlowGraph::with_function(&f);
    let self_loops = f
        .layout
        .blocks()
        .filter(|&b| cfg.pred_iter(b).any(|p| p.block == b))
        .count();
    assert_eq!(self_loops, 1, "folded without PLIRON_LOOPDEL:\n{}", f.display());
}

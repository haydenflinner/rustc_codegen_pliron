//! `PLIRON_LOOPDEL=0` kills the post-rotation countdown fold: the loop
//! stays reachable (kept in its own binary — `pliron_cfg`'s env parse
//! is one-shot per process, so set/unset tests can't share one).
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function, Opcode};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn live_blocks(f: &Function) -> FxHashSet<Block> {
    let entry = f.layout.entry_block().unwrap();
    let (mut seen, mut wl) = (FxHashSet::default(), vec![entry]);
    while let Some(b) = wl.pop() {
        if !seen.insert(b) {
            continue;
        }
        for i in f.layout.block_insts(b) {
            for bc in f
                .dfg
                .insts[i]
                .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                wl.push(bc.block(&f.dfg.value_lists));
            }
        }
    }
    seen
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
fn zero_disables_the_fold() {
    unsafe { std::env::set_var("PLIRON_LOOPDEL", "0") };
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
    let live_loops = live_blocks(&f)
        .into_iter()
        .filter(|&b| cfg.pred_iter(b).any(|p| p.block == b))
        .count();
    assert_eq!(live_loops, 1, "folded under =0:\n{}", f.display());
    let live_stores = live_blocks(&f)
        .into_iter()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Store)
        .count();
    assert_eq!(live_stores, 1, "extra store under =0:\n{}", f.display());
}

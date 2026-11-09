//! Escape remap dominance: `plan_dead` sinks a live-out loop value to an
//! appended `exit_dest` param, but the surviving loop still binds it —
//! uses may only be rewritten to the new param inside `exit_dest`'s
//! dominion. `egg`'s `SymbolLang::matches` ICE'd when the latch's
//! `iadd v120, 1` was rewritten to the new exit param `v147`, which
//! `block26` doesn't dominate.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Function, ValueDef};
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

/// `for { v90 += 4; v120 += 1 } while v90 != v89`, result `v120` — a
/// counting IV escaping to `block26`'s `icmp`. The closed form needs the
/// `(v89-v0) % 4 == 0` guard, so the loop survives on the non-fast path
/// and the latch must keep reading `v120`.
const ESCAPING_COUNT: &str = "function %f(i64, i64, i64) -> i8 {
block0(v0: i64, v1: i64, v116: i64):
    v144 = iconst.i64 0
    jump block23(v0, v144)

block23(v90: i64, v120: i64):
    v97 = icmp.i64 eq v90, v1
    brif v97, block26, block24

block24:
    v41 = iconst.i64 4
    v99 = iadd.i64 v90, v41
    v62 = iconst.i64 1
    v109 = iadd.i64 v120, v62
    jump block23(v99, v109)

block26:
    v111 = icmp.i64 eq v116, v120
    return v111
}";

#[test]
fn escape_use_in_surviving_loop_keeps_iv() {
    let mut f = parse(ESCAPING_COUNT);
    let n = pliron_clif_opt::loopidiom::run(
        &mut f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        tcfg(),
        "f",
    );
    assert!(n > 0, "dead-loop idiom did not fire:\n{}", f.display());
    let cfg = ControlFlowGraph::with_function(&f);
    let dt = DominatorTree::with_function(&f, &cfg);
    // Every appended exit param may only be read where it dominates:
    // inside the exit block itself or blocks it dominates. The latch's
    // `iadd` must still read the loop param, not the sunk exit param.
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            for &a in f.dfg.inst_args(i) {
                if let ValueDef::Param(pb, _) = f.dfg.value_def(a)
                    && pb != b
                    && !dt.block_dominates(pb, b)
                {
                    panic!(
                        "{a} (param of {pb}) used in non-dominated {b}:\n{}",
                        f.display()
                    );
                }
            }
        }
    }
}

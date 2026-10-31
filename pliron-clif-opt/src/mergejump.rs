//! Straight-line block merging (`PLIRON_MERGEJUMP=1`): `b: ...; jump c`
//! where `c` has no params and `b` is its only predecessor becomes one
//! block. MIR's `goto`s leave such pairs everywhere, and jumpthread only
//! threads a block whose own params feed its own `brif`: num-bigint-dig's
//! zip merged its `Option<&mut u64>` into `block70(v125)`, jumped to
//! `block9`, and tested `v125 == 0` there, so the null test on a pointer
//! known non-zero on one edge (by the dominating `v105 == 0` branch) was
//! never threaded. Runs before the first jumpthread.
//!
//! `same_region(b, c)` lets the caller keep inline-region facts sound: a
//! merge moves `c`'s instructions into `b`'s region.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function, InstructionData};

pub fn run(func: &mut Function, same_region: &dyn Fn(Block, Block) -> bool) -> usize {
    let entry = func.layout.entry_block();
    let mut n = 0;
    // Predecessor counts stay valid across merges: absorbing `c` into `b`
    // hands `c`'s out-edges to `b` one for one, and `c` had only `b`.
    let cfg = ControlFlowGraph::with_function(func);
    let npreds: rustc_hash::FxHashMap<Block, usize> =
        func.layout.blocks().map(|b| (b, cfg.pred_iter(b).count())).collect();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        // `b` may have absorbed its successor already; keep absorbing.
        loop {
            if !func.layout.is_block_inserted(b) {
                break;
            }
            let Some(t) = func.layout.last_inst(b) else { break };
            let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else { break };
            let c = destination.block(&func.dfg.value_lists);
            if c == b
                || Some(c) == entry
                || !func.dfg.block_params(c).is_empty()
                || destination.args(&func.dfg.value_lists).next().is_some()
                || npreds.get(&c).copied() != Some(1)
                || !same_region(b, c)
                || !pliron_cfg::bisect("mergejump")
            {
                break;
            }
            func.layout.remove_inst(t);
            let insts: Vec<_> = func.layout.block_insts(c).collect();
            for i in insts {
                func.layout.remove_inst(i);
                func.layout.append_inst(i, b);
            }
            func.layout.remove_block(c);
            n += 1;
        }
    }
    n
}

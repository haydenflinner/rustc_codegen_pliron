//! Sink values computed ahead of a loop's zero-trip guard into the
//! guarded side (`PLIRON_GUARDSINK=1`, opt-in).
//!
//! `licm` hoists loop invariants into the preheader; `looprot` then turns
//! that preheader into the zero-trip guard (`brif n == 0, exit, pre`) and
//! the invariants sit above the test, paid on every call even when the
//! loop is skipped. hashbrown's `reserve_rehash` (lol_html, 6.5k calls,
//! zero items on that path) loaded foldhash's seed address five times and
//! built sponge constants before branching past the rehash loop.
//!
//! Rule, per block `b` ending in `brif`: walking up from the branch, a
//! single-result instruction moves to the top of successor `s` when every
//! use lies in a block `s` dominates (or in an instruction already moving
//! to `s`), `s`'s only predecessor is `b`, `s` is not a loop header and is
//! in `b`'s innermost loop. Pure, non-trapping instructions move freely; a
//! `notrap` load moves only when nothing between it and the branch may
//! write memory. One walk per block, uses from `uses::Uses`.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Opcode};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::FxHashMap;

fn movable(func: &Function, i: Inst) -> Option<bool> {
    let d = &func.dfg.insts[i];
    let op = d.opcode();
    if func.dfg.inst_results(i).len() != 1 || op.is_branch() || op.is_call() || op.can_store() || op.other_side_effects() {
        return None;
    }
    if op.can_load() {
        // A load may move only past code that writes no memory.
        return match d {
            InstructionData::Load { flags, .. } if func.dfg.mem_flags[*flags].notrap() => Some(true),
            _ => None,
        };
    }
    if op.can_trap() || op == Opcode::BlackBox {
        return None;
    }
    Some(false)
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let uses = crate::uses::Uses::new(func);
    let single_pred = |s: Block| -> Option<Block> {
        let mut it = cfg.pred_iter(s);
        match (it.next(), it.next()) {
            (Some(p), None) => Some(p.block),
            _ => None,
        }
    };
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let InstructionData::Brif { blocks: bs, .. } = func.dfg.insts[t] else { continue };
        let succ = [bs[0].block(&func.dfg.value_lists), bs[1].block(&func.dfg.value_lists)];
        if succ[0] == succ[1] {
            continue;
        }
        let target_ok: Vec<bool> = succ
            .iter()
            .map(|&s| single_pred(s) == Some(b) && la.is_loop_header(s).is_none() && la.innermost_loop(s) == la.innermost_loop(b))
            .collect();
        if !target_ok.iter().any(|&x| x) {
            continue;
        }
        // Bottom-up: users later in `b` are decided before their defs.
        let mut dest: FxHashMap<Inst, usize> = FxHashMap::default();
        let mut mem_below = false;
        let mut cur = func.layout.prev_inst(t);
        while let Some(i) = cur {
            cur = func.layout.prev_inst(i);
            let Some(is_load) = movable(func, i) else {
                let op = func.dfg.insts[i].opcode();
                if op.can_store() || op.is_call() || op.other_side_effects() {
                    mem_below = true;
                }
                continue;
            };
            if is_load && mem_below {
                continue;
            }
            let r = func.dfg.first_result(i);
            let sites = uses.sites(r);
            if sites.is_empty() {
                continue;
            }
            // The one successor every use sits under.
            let mut pick: Option<usize> = None;
            let ok = sites.iter().all(|&(_, u)| {
                let k = if let Some(&k) = dest.get(&u) {
                    k
                } else {
                    let Some(ub) = func.layout.inst_block(u) else { return true };
                    if ub == b {
                        return false;
                    }
                    match (0..2).find(|&k| target_ok[k] && dt.block_dominates(succ[k], ub)) {
                        Some(k) => k,
                        None => return false,
                    }
                };
                match pick {
                    None => {
                        pick = Some(k);
                        true
                    }
                    Some(p) => p == k,
                }
            });
            if !ok {
                continue;
            }
            let Some(k) = pick else { continue };
            if !pliron_cfg::bisect("guardsink") {
                continue;
            }
            dest.insert(i, k);
        }
        if dest.is_empty() {
            continue;
        }
        // Move in original order to the top of each target.
        let order: Vec<Inst> = func.layout.block_insts(b).filter(|i| dest.contains_key(i)).collect();
        let tops: [Option<Inst>; 2] = [func.layout.first_inst(succ[0]), func.layout.first_inst(succ[1])];
        for i in order {
            let k = dest[&i];
            let Some(top) = tops[k] else { continue };
            func.layout.remove_inst(i);
            func.layout.insert_inst(i, top);
            n += 1;
        }
    }
    n
}

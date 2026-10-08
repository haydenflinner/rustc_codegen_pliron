//! Loop rotation (LLVM's `loop-rotate`) on CLIF. A loop header that only
//! tests the exit condition is copied into the latch, so each iteration ends
//! in one `brif` back to the body instead of `jump header` + `brif`. The
//! original header stays as the entry guard. Values of the header used past
//! it become block parameters of its two successors (each must have the
//! header as its only predecessor), fed by the header and by the copy.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstructionData, Value};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 8;
const MAX_ROTATIONS: usize = 64;

pub fn run(func: &mut Function) -> usize {
    let mut done: FxHashSet<Block> = FxHashSet::default();
    let mut n = 0;
    while n < MAX_ROTATIONS {
        let cfg = ControlFlowGraph::with_function(func);
        let domtree = DominatorTree::with_function(func, &cfg);
        let Some(h) = func
            .layout
            .blocks()
            .filter(|b| !done.contains(b))
            .find(|&h| rotatable(func, &cfg, &domtree, h))
        else {
            break;
        };
        done.insert(h);
        if rotate(func, &cfg, &domtree, h) {
            n += 1;
        }
    }
    n
}

/// Header `h` with one latch ending in `jump h`, a small call-free body and a
/// `brif` to two blocks that only `h` reaches.
fn rotatable(func: &Function, cfg: &ControlFlowGraph, domtree: &DominatorTree, h: Block) -> bool {
    if Some(h) == func.layout.entry_block() || !domtree.is_reachable(h) {
        return false;
    }
    let Some(t) = func.layout.last_inst(h) else {
        return false;
    };
    let InstructionData::Brif { blocks, .. } = &func.dfg.insts[t] else {
        return false;
    };
    let pool = &func.dfg.value_lists;
    let (s1, s2) = (blocks[0].block(pool), blocks[1].block(pool));
    if s1 == h || s2 == h || s1 == s2 {
        return false;
    }
    if blocks
        .iter()
        .any(|b| b.args(pool).any(|a| !matches!(a, BlockArg::Value(_))))
    {
        return false;
    }
    if cfg.pred_iter(s1).count() != 1 || cfg.pred_iter(s2).count() != 1 {
        return false;
    }
    let latches: Vec<_> = cfg
        .pred_iter(h)
        .filter(|p| domtree.block_dominates(h, p.block))
        .collect();
    let [ref l] = latches[..] else {
        return false;
    };
    if func.layout.last_inst(l.block) != Some(l.inst)
        || !matches!(func.dfg.insts[l.inst], InstructionData::Jump { .. })
    {
        return false;
    }
    let InstructionData::Jump { destination, .. } = &func.dfg.insts[l.inst] else {
        return false;
    };
    if destination
        .args(pool)
        .any(|a| !matches!(a, BlockArg::Value(_)))
    {
        return false;
    }
    let mut k = 0;
    for i in func.layout.block_insts(h) {
        if i == t {
            continue;
        }
        let op = func.dfg.insts[i].opcode();
        if op.is_call() || op.is_branch() || op.is_terminator() {
            return false;
        }
        k += 1;
    }
    k <= MAX_INSTS
}

fn resolved_args(func: &Function, i: Inst) -> Vec<Value> {
    func.dfg
        .inst_values(i)
        .map(|v| func.dfg.resolve_aliases(v))
        .collect()
}

fn rotate(func: &mut Function, cfg: &ControlFlowGraph, domtree: &DominatorTree, h: Block) -> bool {
    let t = func.layout.last_inst(h).unwrap();
    let InstructionData::Brif { blocks, .. } = func.dfg.insts[t] else {
        unreachable!()
    };
    let succ = [
        blocks[0].block(&func.dfg.value_lists),
        blocks[1].block(&func.dfg.value_lists),
    ];
    let l = cfg
        .pred_iter(h)
        .find(|p| domtree.block_dominates(h, p.block))
        .unwrap();

    // Values defined in `h`, and which successor region uses each one outside `h`.
    let mut defs: Vec<Value> = func.dfg.block_params(h).to_vec();
    for i in func.layout.block_insts(h) {
        defs.extend_from_slice(func.dfg.inst_results(i));
    }
    let defset: FxHashSet<Value> = defs.iter().copied().collect();
    let mut need = [FxHashSet::<Value>::default(), FxHashSet::default()];
    let mut users: Vec<(Inst, usize)> = Vec::new();
    for b in func.layout.blocks() {
        if b == h || !domtree.is_reachable(b) {
            continue;
        }
        let side = succ.iter().position(|&s| domtree.block_dominates(s, b));
        for i in func.layout.block_insts(b) {
            let used: Vec<Value> = resolved_args(func, i)
                .into_iter()
                .filter(|v| defset.contains(v))
                .collect();
            if used.is_empty() {
                continue;
            }
            let Some(side) = side else {
                return false;
            };
            need[side].extend(used);
            users.push((i, side));
        }
    }

    // New parameters on the successors; the header passes its own values.
    let mut repl = [FxHashMap::<Value, Value>::default(), FxHashMap::default()];
    let mut order: [Vec<Value>; 2] = [Vec::new(), Vec::new()];
    for side in 0..2 {
        for &v in defs.iter().filter(|v| need[side].contains(v)) {
            let ty = func.dfg.value_type(v);
            let p = func.dfg.append_block_param(succ[side], ty);
            repl[side].insert(v, p);
            order[side].push(v);
            let dfg = &mut func.dfg;
            if let InstructionData::Brif { blocks, .. } = &mut dfg.insts[t] {
                blocks[side].append_argument(v, &mut dfg.value_lists);
            }
        }
    }
    for (i, side) in users {
        let old: Vec<Value> = func.dfg.inst_values(i).collect();
        let new: Vec<Value> = old
            .iter()
            .map(|&x| {
                let r = func.dfg.resolve_aliases(x);
                *repl[side].get(&r).unwrap_or(&x)
            })
            .collect();
        if new != old {
            func.dfg.overwrite_inst_values(i, new.into_iter());
        }
    }

    // Copy the header into the latch, in place of its `jump h(args)`.
    let InstructionData::Jump { destination, .. } = func.dfg.insts[l.inst] else {
        unreachable!()
    };
    let mut map: FxHashMap<Value, Value> = FxHashMap::default();
    for (&prm, a) in func
        .dfg
        .block_params(h)
        .to_vec()
        .iter()
        .zip(destination.args(&func.dfg.value_lists).collect::<Vec<_>>())
    {
        let BlockArg::Value(a) = a else {
            unreachable!()
        };
        map.insert(prm, a);
    }
    let body: Vec<Inst> = func.layout.block_insts(h).collect();
    for i in body {
        let ni = func.dfg.clone_inst(i);
        let dfg = &mut func.dfg;
        let mut data = dfg.insts[ni];
        for d in data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
            *d = d.deep_clone(&mut dfg.value_lists);
        }
        dfg.insts[ni] = data;
        let vals: Vec<Value> = resolved_args(func, ni)
            .into_iter()
            .map(|v| *map.get(&v).unwrap_or(&v))
            .collect();
        func.dfg.overwrite_inst_values(ni, vals.into_iter());
        let res: Vec<(Value, Value)> = func
            .dfg
            .inst_results(i)
            .iter()
            .copied()
            .zip(func.dfg.inst_results(ni).iter().copied())
            .collect();
        map.extend(res);
        func.layout.insert_inst(ni, l.inst);
    }
    func.layout.remove_inst(l.inst);
    true
}

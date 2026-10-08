//! Duplicate small return blocks into their `jump` predecessors (LLVM's
//! tail duplication of return blocks). Helpers like `derive(PartialEq)`
//! end every arm with `jump ret(v)`; copying the `return` into each arm
//! removes the jump and the block-parameter move. Small `brif` blocks whose
//! values are only used inside them (a `bool` merged through a block
//! parameter, then tested) are copied the same way, so the `icmp` in each
//! predecessor feeds its own branch.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, BlockArg, Function, InstructionData, Opcode, Value, ValueDef};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 4;

fn candidate(func: &Function, uses: &FxHashMap<Value, Option<Block>>, b: Block) -> bool {
    let Some(t) = func.layout.last_inst(b) else {
        return false;
    };
    let brif =
        func.dfg.insts[t].opcode() == Opcode::Brif && crate::pass_enabled("PLIRON_TAILDUP_BRIF");
    if !func.dfg.insts[t].opcode().is_return() && !(brif && local_values(func, uses, b)) {
        return false;
    }
    let mut n = 0;
    for i in func.layout.block_insts(b) {
        if i == t {
            continue;
        }
        let op = func.dfg.insts[i].opcode();
        if op.is_call()
            || op.can_store()
            || op.can_load()
            || op.can_trap()
            || op.other_side_effects()
            || op.is_branch()
            || op.is_terminator()
        {
            return false;
        }
        n += 1;
    }
    n <= MAX_INSTS
}

/// For each value used in reachable code: the one block using it, or `None`
/// if several do.
fn use_blocks(func: &Function, domtree: &DominatorTree) -> FxHashMap<Value, Option<Block>> {
    let mut m: FxHashMap<Value, Option<Block>> = FxHashMap::default();
    for b in func.layout.blocks().filter(|&b| domtree.is_reachable(b)) {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let e = m.entry(func.dfg.resolve_aliases(v)).or_insert(Some(b));
                if *e != Some(b) {
                    *e = None;
                }
            }
        }
    }
    m
}

/// No parameter or result of `b` is used outside `b` (so copies need no SSA repair).
fn local_values(func: &Function, uses: &FxHashMap<Value, Option<Block>>, b: Block) -> bool {
    let ok = |v: &Value| uses.get(v).is_none_or(|&u| u == Some(b));
    func.dfg.block_params(b).iter().all(ok)
        && func
            .layout
            .block_insts(b)
            .all(|i| func.dfg.inst_results(i).iter().all(ok))
}

/// Every value `r` uses from outside `r` is available at the end of `p`.
fn outer_dominate(func: &Function, domtree: &DominatorTree, r: Block, p: Block) -> bool {
    func.layout.block_insts(r).all(|i| {
        func.dfg.inst_values(i).all(|v| {
            let def = match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
                ValueDef::Result(d, _) => func.layout.inst_block(d),
                ValueDef::Param(b, _) => Some(b),
                _ => None,
            };
            def.is_some_and(|b| b == r || domtree.block_dominates(b, p))
        })
    })
}

/// Returns the number of jumps replaced by a copy of the return block.
pub fn run(func: &mut Function) -> usize {
    let entry = func.layout.entry_block();
    let mut cfg = ControlFlowGraph::with_function(func);
    let mut domtree = DominatorTree::with_function(func, &cfg);
    // Dead blocks would pin values as used elsewhere and keep dangling refs
    // to removed blocks; they never run, so drop them first.
    let dead: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&b| !domtree.is_reachable(b))
        .collect();
    if !dead.is_empty() {
        for &b in &dead {
            while let Some(i) = func.layout.first_inst(b) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(b);
        }
        cfg = ControlFlowGraph::with_function(func);
        domtree = DominatorTree::with_function(func, &cfg);
    }
    let uses = use_blocks(func, &domtree);
    let rets: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| Some(b) != entry && candidate(func, &uses, b))
        .collect();
    if rets.is_empty() {
        return 0;
    }
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for p in blocks {
        let Some(t) = func.layout.last_inst(p) else {
            continue;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else {
            continue;
        };
        let r = destination.block(&func.dfg.value_lists);
        if r == p
            || !rets.contains(&r)
            || !domtree.is_reachable(p)
            || !outer_dominate(func, &domtree, r, p)
        {
            continue;
        }
        let mut map: FxHashMap<Value, Value> = FxHashMap::default();
        let args: Vec<BlockArg> = destination.args(&func.dfg.value_lists).collect();
        let mut ok = true;
        for (&prm, a) in func.dfg.block_params(r).iter().zip(&args) {
            match a {
                BlockArg::Value(v) => {
                    map.insert(prm, *v);
                }
                _ => ok = false,
            }
        }
        if !ok {
            continue;
        }
        let body: Vec<_> = func.layout.block_insts(r).collect();
        let alias: FxHashMap<Value, Value> = body
            .iter()
            .flat_map(|&i| func.dfg.inst_values(i))
            .map(|v| (v, func.dfg.resolve_aliases(v)))
            .collect();
        for i in body {
            let ni = func.dfg.clone_inst(i);
            let dfg = &mut func.dfg;
            let mut data = dfg.insts[ni];
            for d in data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                *d = d.deep_clone(&mut dfg.value_lists);
            }
            dfg.insts[ni] = data;
            let mut data = func.dfg.insts[ni];
            let dfg = &mut func.dfg;
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |v| {
                    let v = *alias.get(&v).unwrap_or(&v);
                    *map.get(&v).unwrap_or(&v)
                },
            );
            func.dfg.insts[ni] = data;
            for (&o, &nv) in func
                .dfg
                .inst_results(i)
                .iter()
                .zip(func.dfg.inst_results(ni))
            {
                map.insert(o, nv);
            }
            func.layout.insert_inst(ni, t);
        }
        func.layout.remove_inst(t);
        n += 1;
    }
    // Return blocks left without predecessors would fail dominance checks.
    let cfg = ControlFlowGraph::with_function(func);
    for r in rets {
        if cfg.pred_iter(r).next().is_none() {
            while let Some(i) = func.layout.first_inst(r) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(r);
        }
    }
    n
}

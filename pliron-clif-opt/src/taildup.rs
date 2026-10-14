//! Duplicate small return blocks into their `jump` predecessors (LLVM's
//! tail duplication of return blocks). Helpers like `derive(PartialEq)`
//! end every arm with `jump ret(v)`; copying the `return` into each arm
//! removes the jump and the block-parameter move. Small `brif` blocks whose
//! values are only used inside them (a `bool` merged through a block
//! parameter, then tested) are copied the same way, so the `icmp` in each
//! predecessor feeds its own branch.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::CondCode;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

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
            if crate::env_set("PLIRON_TDDBG") {
                eprintln!("taildup: cand {b} impure {i} (fn {})", func.name);
            }
            return false;
        }
        n += 1;
    }
    if n > MAX_INSTS && crate::env_set("PLIRON_TDDBG") {
        eprintln!("taildup: cand {b} too-big {n} (fn {})", func.name);
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
    let dbg = crate::env_set("PLIRON_TDDBG");
    let ok = |v: &Value| {
        let good = uses.get(v).is_none_or(|&u| u == Some(b));
        if dbg && !good {
            eprintln!("taildup: cand {b} escapes {v} (fn {})", func.name);
        }
        good
    };
    func.dfg.block_params(b).iter().all(ok)
        && func
            .layout
            .block_insts(b)
            .all(|i| func.dfg.inst_results(i).iter().all(ok))
}

/// The `icmp` behind `t`'s `brif` condition when it is the branch's only
/// use — complementing it is a free condition inversion (edge args keep
/// the un-complemented value, so any other use disqualifies it).
fn flip_inst(
    func: &Function,
    uses: &FxHashMap<Value, Option<Block>>,
    p: Block,
    t: Inst,
) -> Option<Inst> {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else {
        return None;
    };
    let arg = match func.dfg.value_alias_dest_for_serialization(arg) {
        Some(d) if d != Value::reserved_value() => d,
        Some(_) => return None,
        None => arg,
    };
    if uses.get(&arg) != Some(&Some(p)) {
        return None;
    }
    let ValueDef::Result(i, 0) = func.dfg.value_def(arg) else {
        return None;
    };
    if !matches!(
        func.dfg.insts[i],
        InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            ..
        }
    ) {
        return None;
    }
    let mut n = 0;
    for j in func.layout.block_insts(p) {
        n += func
            .dfg
            .inst_values(j)
            .filter(|&v| {
                func.dfg
                    .value_alias_dest_for_serialization(v)
                    .filter(|&d| d != Value::reserved_value())
                    .unwrap_or(v)
                    == arg
            })
            .count();
    }
    (n == 1).then_some(i)
}

/// Every value `r` uses from outside `r` is available at the end of `p`.
pub(crate) fn outer_dominate(
    func: &Function,
    domtree: &DominatorTree,
    r: Block,
    p: Block,
) -> bool {
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
        // A `jump` pred gets the copy in place; a `brif` pred gets one clone
        // block per edge to a candidate (the flag merge it feeds, e.g.
        // `is_alphabetic`'s bool param, then never materialises as a value).
        let is_brif = matches!(func.dfg.insts[t], InstructionData::Brif { .. });
        let dests: Vec<BlockCall> = match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => vec![destination],
            InstructionData::Brif { blocks, .. } if crate::pass_enabled("PLIRON_TAILDUP_EDGE") => {
                blocks.to_vec()
            }
            _ => continue,
        };
        // Edges whose target loops back through a dominator of `p` — latch
        // merges, worth cloning even without foldable args: the copy moves
        // the loop's hot tail onto the branch's fallthrough edge instead of
        // a `jmp` per iteration.
        let latches: Vec<bool> = dests
            .iter()
            .map(|d| {
                let r = d.block(&func.dfg.value_lists);
                is_brif
                    && r != p
                    && func.layout.last_inst(r).is_some_and(|rt| {
                        func.dfg.insts[rt]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                            .iter()
                            .any(|bc| domtree.block_dominates(bc.block(&func.dfg.value_lists), p))
                    })
            })
            .collect();
        // Compare to complement when an edge-0 latch clone must be moved
        // to the second edge to land next in emitted order.
        let mut flip0 = None;
        for (di, &dest) in dests.iter().enumerate() {
            let r = dest.block(&func.dfg.value_lists);
            let latch = latches[di];
            // When the second edge's latch clone will take the fallthrough
            // slot anyway, an edge-0 clone is pure duplication — skip it.
            if latch && di == 0 && latches.get(1) == Some(&true) {
                continue;
            }
            // Brif-edge clones only pay when an arg is a constant the cloned
            // body can fold — otherwise it's pure dispatch-chain duplication.
            if is_brif
                && !latch
                && !dest.args(&func.dfg.value_lists).any(|a| {
                    matches!(a, BlockArg::Value(v)
                        if func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()
                            .is_some_and(|i| func.dfg.insts[i].opcode() == Opcode::Iconst))
                })
            {
                continue;
            }
            // The clone lands next in emitted order only as the brif's
            // second successor, so an edge-0 latch clone needs a freely
            // invertible compare to swap onto it.
            let flip = latch.then(|| flip_inst(func, &uses, p, t)).flatten();
            if latch && di == 0 && flip.is_none() {
                continue;
            }
            let why = if r == p {
                "self"
            } else if !rets.contains(&r) {
                "not-candidate"
            } else if !domtree.is_reachable(p) {
                "unreachable"
            } else if !outer_dominate(func, &domtree, r, p) {
                "outer-dom"
            } else {
                ""
            };
            if crate::env_set("PLIRON_TDDBG") && !why.is_empty() {
                eprintln!("taildup: skip {p}->{r} edge {di}: {why} (fn {})", func.name);
            }
            if !why.is_empty() {
                continue;
            }
            let mut map: FxHashMap<Value, Value> = FxHashMap::default();
            let args: Vec<BlockArg> = dest.args(&func.dfg.value_lists).collect();
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
            if crate::env_set("PLIRON_TDDBG") {
                eprintln!("taildup: clone {r} into {p} edge {di} (fn {})", func.name);
            }
            let body: Vec<_> = func.layout.block_insts(r).collect();
            let alias: FxHashMap<Value, Value> = body
                .iter()
                .flat_map(|&i| func.dfg.inst_values(i))
                .map(|v| (v, func.dfg.resolve_aliases(v)))
                .collect();
            let mut new_insts = Vec::with_capacity(body.len());
            for i in body {
                let ni = func.dfg.clone_inst(i);
                let dfg = &mut func.dfg;
                let mut data = dfg.insts[ni];
                for d in
                    data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                {
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
                new_insts.push(ni);
            }
            if matches!(func.dfg.insts[t], InstructionData::Jump { .. }) {
                for &ni in &new_insts {
                    func.layout.insert_inst(ni, t);
                }
                func.layout.remove_inst(t);
            } else {
                let nb = func.dfg.make_block();
                func.layout.insert_block_after(nb, p);
                for &ni in &new_insts {
                    func.layout.append_inst(ni, nb);
                }
                let dfg = &mut func.dfg;
                dfg.insts[t]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
                    BlockCall::new(nb, std::iter::empty(), &mut dfg.value_lists);
            }
            if latch && di == 0 {
                flip0 = flip;
            }
            n += 1;
        }
        if let Some(cmp) = flip0 {
            // Swap the clone onto the second edge so it is next in emitted
            // order: complement the (single-use) compare, swap targets.
            let InstructionData::IntCompare { cond, .. } = &mut func.dfg.insts[cmp] else {
                continue;
            };
            *cond = cond.complement();
            let mut data = func.dfg.insts[t];
            let dfg = &mut func.dfg;
            data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                .swap(0, 1);
            dfg.insts[t] = data;
        }
    }
    // Return blocks left without predecessors would fail dominance checks.
    let cfg = ControlFlowGraph::with_function(func);
    let mut removed = false;
    for r in rets {
        if cfg.pred_iter(r).next().is_none() {
            while let Some(i) = func.layout.first_inst(r) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(r);
            removed = true;
        }
    }
    // Removing a candidate strands the blocks it dominated: they lose
    // their entry path but keep references to the removed defs. They are
    // unreachable, so sweep them too. `cfg` still carries the removed
    // blocks' out-edges; rebuild it before judging reachability.
    if removed {
        let cfg = ControlFlowGraph::with_function(func);
        let domtree = DominatorTree::with_function(func, &cfg);
        for b in func
            .layout
            .blocks()
            .filter(|&b| !domtree.is_reachable(b))
            .collect::<Vec<_>>()
        {
            while let Some(i) = func.layout.first_inst(b) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(b);
        }
    }
    n
}

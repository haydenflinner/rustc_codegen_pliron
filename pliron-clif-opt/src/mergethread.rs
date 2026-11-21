//! Thread edges through a small merge block whose branch every incoming
//! edge decides with a constant, even when the block stores
//! (`PLIRON_MERGETHREAD=1`, opt-in).
//!
//! lol_html's tag scanner consumes a byte as `Option<u8>` and matches on
//! it; `consume_ch` bumps the position on both paths:
//!
//! ```text
//! block0:   brif (pos < len), block28, block29(0, 0)
//! block28:  ch = load; jump block29(1, ch)
//! block29(flag, ch):  pos1 = pos + 1; store pos1; brif flag, block2, block8
//! block2:   ... uses ch, pos1 ...
//! ```
//!
//! `jumpthread` only threads merges of pure ops, and `taildup`'s escape
//! repair gives up when it cannot order the use-blocks; meanwhile the
//! flag is materialized in both predecessors, tested, and the byte and
//! the input pointer shuffle between registers around it, on every byte
//! a state function's loop consumes.
//!
//! Rule: a block `r` of at most `MAX_INSTS` pure ops and plain stores,
//! ending in `brif` on one of its parameters, whose every incoming edge
//! passes an `iconst` for that parameter, whose two successors are
//! distinct, not `r`, and have `r` as their only predecessor, each
//! selected by at most one incoming edge. Every use of `r`'s values
//! outside `r` must lie in a block one of the successors dominates. Each
//! incoming edge gets a copy of `r`'s body (in place before a `jump`, or
//! in a new block on a `brif` edge) ending in a jump to the successor its
//! constant selects; uses under that successor read the copy's values,
//! and `r` is removed. The stores run once on every path, as before.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
};
use rustc_hash::FxHashMap;

/// Non-terminator insts of a merge block copied per edge (`iconst`s free).
const MAX_INSTS: usize = 6;
/// Merges threaded per function (each rebuilds the CFG and dominators).
const MAX_ROUNDS: usize = 16;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

fn plain_store(op: Opcode) -> bool {
    matches!(op, Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32)
}

/// One incoming edge: the predecessor's terminator, the destination slot,
/// its arguments, and the successor index (0/1) its constant selects.
struct In {
    inst: Inst,
    slot: usize,
    args: Vec<BlockArg>,
    pick: usize,
}

struct Plan {
    r: Block,
    ins: Vec<In>,
    succ: [Block; 2],
}

fn plan(func: &Function, cfg: &ControlFlowGraph, dt: &DominatorTree, r: Block, uses: &FxHashMap<Value, Vec<Block>>) -> Option<Plan> {
    let t = func.layout.last_inst(r)?;
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[t] else { return None };
    let arg = func.dfg.resolve_aliases(arg);
    let k = func.dfg.block_params(r).iter().position(|&p| p == arg)?;
    let succ = [blocks[0].block(&func.dfg.value_lists), blocks[1].block(&func.dfg.value_lists)];
    if succ[0] == succ[1] || succ.contains(&r) || Some(r) == func.layout.entry_block() {
        return None;
    }
    let mut n = 0;
    for i in func.layout.block_insts(r) {
        if i == t {
            continue;
        }
        let op = func.dfg.insts[i].opcode();
        let pure = !(op.is_call() || op.can_load() || op.can_trap() || op.other_side_effects() || op.is_branch())
            && (!op.can_store() || plain_store(op));
        if !pure {
            return None;
        }
        if op != Opcode::Iconst {
            n += 1;
        }
    }
    if n > MAX_INSTS {
        return None;
    }
    // Each successor's only predecessor is `r`.
    for &s in &succ {
        let mut ps = cfg.pred_iter(s);
        match (ps.next(), ps.next()) {
            (Some(p), None) if p.block == r => {}
            _ => return None,
        }
    }
    // Incoming edges, each deciding the branch with a constant.
    let mut ins = Vec::new();
    let mut picked = [0usize; 2];
    for p in cfg.pred_iter(r) {
        if p.block == r {
            return None;
        }
        let pt = p.inst;
        if !matches!(func.dfg.insts[pt].opcode(), Opcode::Jump | Opcode::Brif) {
            return None;
        }
        let dests = func.dfg.insts[pt].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        for (slot, bc) in dests.iter().enumerate() {
            if bc.block(&func.dfg.value_lists) != r {
                continue;
            }
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            let c = match args.get(k) {
                Some(BlockArg::Value(v)) => iconst(func, *v)?,
                _ => return None,
            };
            let pick = if c != 0 { 0 } else { 1 };
            picked[pick] += 1;
            if picked[pick] > 1 {
                return None;
            }
            ins.push(In { inst: pt, slot, args, pick });
        }
    }
    if ins.is_empty() {
        return None;
    }
    // Every use of r's values outside r lies under one successor.
    let mut defs: Vec<Value> = func.dfg.block_params(r).to_vec();
    for i in func.layout.block_insts(r) {
        defs.extend_from_slice(func.dfg.inst_results(i));
    }
    for v in defs {
        for &b in uses.get(&v).map(|u| u.as_slice()).unwrap_or(&[]) {
            if b != r && !succ.iter().any(|&s| dt.block_dominates(s, b)) {
                return None;
            }
        }
    }
    Some(Plan { r, ins, succ })
}

/// Copy `r`'s body before `at`, params bound to `args`; returns the map of
/// `r`'s values to the copies' and the successor call it ends in.
fn copy_body(func: &mut Function, r: Block, args: &[BlockArg], pick: usize, at: Inst) -> (FxHashMap<Value, Value>, BlockCall) {
    let mut map: FxHashMap<Value, Value> = FxHashMap::default();
    for (&p, a) in func.dfg.block_params(r).to_vec().iter().zip(args) {
        if let BlockArg::Value(v) = a {
            map.insert(p, *v);
        }
    }
    let t = func.layout.last_inst(r).unwrap();
    let body: Vec<Inst> = func.layout.block_insts(r).filter(|&i| i != t).collect();
    for i in body {
        let ni = func.dfg.clone_inst(i);
        func.set_srcloc(ni, func.srcloc(i));
        let vals: Vec<Value> = func.dfg.inst_values(ni).map(|v| {
            let v = func.dfg.resolve_aliases(v);
            *map.get(&v).unwrap_or(&v)
        }).collect();
        func.dfg.overwrite_inst_values(ni, vals.into_iter());
        func.layout.insert_inst(ni, at);
        let olds: Vec<Value> = func.dfg.inst_results(i).to_vec();
        let news: Vec<Value> = func.dfg.inst_results(ni).to_vec();
        for (o, n) in olds.into_iter().zip(news) {
            map.insert(o, n);
        }
    }
    let InstructionData::Brif { blocks, .. } = func.dfg.insts[t] else { unreachable!() };
    let bc = blocks[pick];
    let dest = bc.block(&func.dfg.value_lists);
    let nargs: Vec<BlockArg> = bc
        .args(&func.dfg.value_lists)
        .map(|a| match a {
            BlockArg::Value(v) => {
                let v = func.dfg.resolve_aliases(v);
                BlockArg::Value(*map.get(&v).unwrap_or(&v))
            }
            other => other,
        })
        .collect();
    (map, BlockCall::new(dest, nargs, &mut func.dfg.value_lists))
}

fn apply(func: &mut Function, dt: &DominatorTree, p: Plan, uses_at: &FxHashMap<Value, Vec<Inst>>) {
    let r = p.r;
    let mut maps: [Option<FxHashMap<Value, Value>>; 2] = [None, None];
    for e in &p.ins {
        if func.dfg.insts[e.inst].opcode() == Opcode::Jump {
            let (map, bc) = copy_body(func, r, &e.args, e.pick, e.inst);
            let dfg = &mut func.dfg;
            dfg.insts[e.inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.slot] = bc;
            maps[e.pick] = Some(map);
        } else {
            // A `brif` edge: the copy goes in a block of its own.
            let pb = func.layout.inst_block(e.inst).unwrap();
            let nb = func.dfg.make_block();
            func.layout.insert_block_after(nb, pb);
            let j = FuncCursor::new(func).at_bottom(nb).ins().jump(p.succ[e.pick], &[]);
            let (map, bc) = copy_body(func, r, &e.args, e.pick, j);
            let dfg = &mut func.dfg;
            dfg.insts[j].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[0] = bc;
            let into = BlockCall::new(nb, [], &mut dfg.value_lists);
            dfg.insts[e.inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.slot] = into;
            maps[e.pick] = Some(map);
        }
    }
    // Uses under each successor read that successor's copy.
    for (s, map) in p.succ.iter().zip(&maps) {
        let Some(map) = map else { continue };
        for (&v, &nv) in map {
            for &u in uses_at.get(&v).map(|x| x.as_slice()).unwrap_or(&[]) {
                let Some(ub) = func.layout.inst_block(u) else { continue };
                if ub == r || !dt.block_dominates(*s, ub) {
                    continue;
                }
                let vals: Vec<Value> = func.dfg.inst_values(u).map(|x| if func.dfg.resolve_aliases(x) == v { nv } else { x }).collect();
                func.dfg.overwrite_inst_values(u, vals.into_iter());
            }
        }
    }
    // `r` has no predecessor left.
    while let Some(i) = func.layout.first_inst(r) {
        func.layout.remove_inst(i);
    }
    func.layout.remove_block(r);
}

pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    for _ in 0..MAX_ROUNDS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        // Uses by value: the blocks (for the plan) and insts (for apply).
        let mut uses: FxHashMap<Value, Vec<Block>> = FxHashMap::default();
        let mut uses_at: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    uses.entry(v).or_default().push(b);
                    uses_at.entry(v).or_default().push(i);
                }
            }
        }
        let Some(p) = func
            .layout
            .blocks()
            .filter(|&b| dt.is_reachable(b))
            .find_map(|b| plan(func, &cfg, &dt, b, &uses))
        else {
            break;
        };
        if !pliron_cfg::bisect("mergethread") {
            break;
        }
        apply(func, &dt, p, &uses_at);
        n += 1;
    }
    n
}

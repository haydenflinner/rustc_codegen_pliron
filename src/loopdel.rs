//! Loop deletion (LLVM's `loop-deletion`) on CLIF. A loop is removed, and its
//! preheader edges sent straight to its exit, when it has no side effects
//! (no calls, loads, stores, traps or returns), no value defined in it is used
//! outside it, every live exit goes to one block with the same loop-invariant
//! arguments, and it is provably finite: a test that runs every iteration
//! leaves the loop once an induction variable stepping by ±1 reaches a
//! loop-invariant bound. Rust has no forward-progress guarantee (`loop {}` is
//! defined), so finiteness must be proven, not assumed.
//!
//! Motivating case: `[(); usize::MAX] == [(); usize::MAX]`, a `usize::MAX`
//! iteration loop of `() == ()` that LLVM deletes. `PLIRON_LOOPDEL=0` disables.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    Block, BlockArg, Function, InstBuilder, InstructionData, Opcode, Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};

const MAX_DELETIONS: usize = 16;

pub fn run(func: &mut Function) -> usize {
    fold_inc_overflow(func);
    let mut n = 0;
    while n < MAX_DELETIONS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        let Some((lp, exit, args)) = la
            .loops()
            .find_map(|lp| deletable(func, &cfg, &dt, &la, lp))
        else {
            break;
        };
        let h = la.loop_header(lp);
        let preds: Vec<_> = cfg
            .pred_iter(h)
            .filter(|p| !la.is_in_loop(p.block, lp))
            .map(|p| p.inst)
            .collect();
        for inst in preds {
            let bc = func.dfg.block_call(exit, &args);
            let dfg = &mut func.dfg;
            for d in dfg.insts[inst]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
            {
                if d.block(&dfg.value_lists) == h {
                    *d = bc;
                }
            }
        }
        n += 1;
    }
    n
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

/// `v` with aliases and block parameters that receive one value on every edge looked through.
fn canon(func: &Function, cfg: &ControlFlowGraph, mut v: Value) -> Value {
    for _ in 0..8 {
        v = func.dfg.resolve_aliases(v);
        let ValueDef::Param(b, i) = func.dfg.value_def(v) else {
            break;
        };
        let mut same: Option<Value> = None;
        for p in cfg.pred_iter(b) {
            let dests = func.dfg.insts[p.inst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            for bc in dests {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let Some(BlockArg::Value(a)) = bc.args(&func.dfg.value_lists).nth(i) else {
                    return v;
                };
                let a = func.dfg.resolve_aliases(a);
                if a == v {
                    continue;
                }
                match same {
                    None => same = Some(a),
                    Some(s) if s == a => {}
                    Some(_) => return v,
                }
            }
        }
        match same {
            Some(s) => v = s,
            None => break,
        }
    }
    v
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// Live successors of `b`'s terminator: a `brif` on a constant has one.
fn live_dests(func: &Function, b: Block) -> Vec<(Block, Vec<BlockArg>)> {
    let Some(t) = func.layout.last_inst(b) else {
        return Vec::new();
    };
    let data = &func.dfg.insts[t];
    let dests = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
    let live: Vec<usize> = match *data {
        InstructionData::Brif { arg, .. } => match iconst(func, arg) {
            Some(c) => vec![if c != 0 { 0 } else { 1 }],
            None => vec![0, 1],
        },
        _ => (0..dests.len()).collect(),
    };
    live.into_iter()
        .map(|i| {
            let bc = dests[i];
            (
                bc.block(&func.dfg.value_lists),
                bc.args(&func.dfg.value_lists).collect(),
            )
        })
        .collect()
}

fn deletable(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
) -> Option<(Loop, Block, Vec<BlockArg>)> {
    let h = la.loop_header(lp);
    let inside = |v: Value| def_block(func, v).is_some_and(|b| la.is_in_loop(b, lp));
    let blocks: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let mut exit: Option<(Block, Vec<BlockArg>)> = None;
    let mut latches = Vec::new();
    for &b in &blocks {
        // Inner loops could spin forever; only single-level loops.
        if la.innermost_loop(b) != Some(lp) {
            return None;
        }
        for inst in func.layout.block_insts(b) {
            let op = func.dfg.insts[inst].opcode();
            if op.is_call()
                || op.can_load()
                || op.can_store()
                || op.can_trap()
                || op.other_side_effects()
                || op.is_return()
                || (op.is_terminator() && !op.is_branch())
            {
                return None;
            }
        }
        if func.layout.last_inst(b).is_none() {
            return None;
        }
        for (d, args) in live_dests(func, b) {
            if la.is_in_loop(d, lp) {
                if d == h {
                    latches.push((b, args));
                }
                continue;
            }
            if args
                .iter()
                .any(|a| !matches!(*a, BlockArg::Value(v) if !inside(v)))
            {
                return None;
            }
            match &exit {
                None => exit = Some((d, args)),
                Some((e, ea)) if *e == d && *ea == args => {}
                Some(_) => return None,
            }
        }
    }
    let (e, eargs) = exit?;
    if latches.is_empty() {
        return None;
    }
    for b in func.layout.blocks().filter(|&b| !la.is_in_loop(b, lp)) {
        for inst in func.layout.block_insts(b) {
            if func.dfg.inst_args(inst).iter().any(|&a| inside(a)) {
                return None;
            }
            for bc in func.dfg.insts[inst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                if bc
                    .args(&func.dfg.value_lists)
                    .any(|a| matches!(a, BlockArg::Value(v) if inside(v)))
                {
                    return None;
                }
            }
        }
    }
    finite(func, cfg, dt, la, lp, &latches).then_some((lp, e, eargs))
}

/// Step of header param `i` on every latch: each passes `i ± 1` for it.
fn step(
    func: &Function,
    cfg: &ControlFlowGraph,
    h: Block,
    i: usize,
    latches: &[(Block, Vec<BlockArg>)],
) -> Option<(i64, Value)> {
    let p = func.dfg.block_params(h)[i];
    let mut out: Option<(i64, Value)> = None;
    for (_, args) in latches {
        let BlockArg::Value(v) = *args.get(i)? else {
            return None;
        };
        let v = canon(func, cfg, v);
        let ValueDef::Result(inst, _) = func.dfg.value_def(v) else {
            return None;
        };
        let s = match func.dfg.insts[inst] {
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args: [a, b],
            } => {
                if canon(func, cfg, a) == p {
                    iconst(func, b)?
                } else if canon(func, cfg, b) == p {
                    iconst(func, a)?
                } else {
                    return None;
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Isub,
                args: [a, b],
            } if canon(func, cfg, a) == p => iconst(func, b)?.checked_neg()?,
            _ => return None,
        };
        if s != 1 && s != -1 {
            return None;
        }
        match out {
            None => out = Some((s, v)),
            Some((s0, _)) if s0 == s => {}
            Some(_) => return None,
        }
    }
    out
}

fn finite(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    latches: &[(Block, Vec<BlockArg>)],
) -> bool {
    let h = la.loop_header(lp);
    let invariant = |v: Value| {
        iconst(func, v).is_some() || def_block(func, v).is_some_and(|b| !la.is_in_loop(b, lp))
    };
    let steps: Vec<(Value, i64, Value)> = (0..func.dfg.block_params(h).len())
        .filter_map(|i| {
            step(func, cfg, h, i, latches).map(|(s, next)| (func.dfg.block_params(h)[i], s, next))
        })
        .collect();
    if steps.is_empty() {
        return false;
    }
    for b in func.layout.blocks().filter(|&b| la.is_in_loop(b, lp)) {
        if !latches
            .iter()
            .all(|&(l, _)| dt.dominates(b, l, &func.layout))
        {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[t] else {
            continue;
        };
        let then_in = la.is_in_loop(blocks[0].block(&func.dfg.value_lists), lp);
        let else_in = la.is_in_loop(blocks[1].block(&func.dfg.value_lists), lp);
        if then_in == else_in {
            continue;
        }
        let ValueDef::Result(c, _) = func.dfg.value_def(canon(func, cfg, arg)) else {
            continue;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: [x, y],
        } = func.dfg.insts[c]
        else {
            continue;
        };
        let (x, y) = (canon(func, cfg, x), canon(func, cfg, y));
        for &(p, s, next) in &steps {
            let cc = if (x == p || x == next) && invariant(y) {
                cond
            } else if (y == p || y == next) && invariant(x) {
                cond.swap_args()
            } else {
                continue;
            };
            // Condition under which the loop keeps going.
            let stay = if then_in { cc } else { cc.complement() };
            let ok = match stay {
                IntCC::NotEqual => true,
                IntCC::UnsignedLessThan | IntCC::SignedLessThan => s == 1,
                IntCC::UnsignedGreaterThan | IntCC::SignedGreaterThan => s == -1,
                _ => false,
            };
            if ok {
                return true;
            }
        }
    }
    false
}

/// `x` if `c` is an unsigned overflow test of `x + 1`: `(x+1) < x`, `x > (x+1)`, `(x+1) == 0`.
fn inc_overflow_of(func: &Function, cfg: &ControlFlowGraph, c: Value) -> Option<Value> {
    let r = |v| canon(func, cfg, v);
    let ValueDef::Result(i, _) = func.dfg.value_def(r(c)) else {
        return None;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [a, b],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let (a, b) = (r(a), r(b));
    let inc = |s: Value| -> Option<Value> {
        let ValueDef::Result(i, _) = func.dfg.value_def(s) else {
            return None;
        };
        let InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [p, q],
        } = func.dfg.insts[i]
        else {
            return None;
        };
        match (iconst(func, p), iconst(func, q)) {
            (_, Some(1)) => Some(r(p)),
            (Some(1), _) => Some(r(q)),
            _ => None,
        }
    };
    match cond {
        IntCC::UnsignedLessThan => inc(a).filter(|&x| x == b),
        IntCC::UnsignedGreaterThan => inc(b).filter(|&x| x == a),
        IntCC::Equal if iconst(func, b) == Some(0) => inc(a),
        _ => None,
    }
}

fn is_umax(func: &Function, v: Value) -> bool {
    let bits = func.dfg.value_type(v).bits();
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    iconst(func, v).is_some_and(|c| c as u64 & mask == mask)
}

/// Which edge of a `brif` on `c` implies `x != uMAX`: `x < y`, `y > x`, `x != MAX` (then),
/// `x >= y`, `y <= x`, `x == MAX` (else).
fn guard_edge(func: &Function, cfg: &ControlFlowGraph, c: Value, x: Value) -> Option<usize> {
    let r = |v| canon(func, cfg, v);
    let ValueDef::Result(i, _) = func.dfg.value_def(r(c)) else {
        return None;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [a, b],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let (a, b) = (r(a), r(b));
    let (cc, other) = if a == x {
        (cond, b)
    } else if b == x {
        (cond.swap_args(), a)
    } else {
        return None;
    };
    match cc {
        IntCC::UnsignedLessThan => Some(0),
        IntCC::UnsignedGreaterThanOrEqual => Some(1),
        IntCC::NotEqual if is_umax(func, other) => Some(0),
        IntCC::Equal if is_umax(func, other) => Some(1),
        _ => None,
    }
}

/// Debug-assertion overflow checks on `i + 1` in `while i < n` loops: the guard already
/// proves `i < uMAX`, so the panic edge is dead. Folding it lets such loops be deleted.
fn fold_inc_overflow(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut todo = Vec::new();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else {
            continue;
        };
        let Some(x) = inc_overflow_of(func, &cfg, arg) else {
            continue;
        };
        let mut cur = b;
        while let Some(d) = dt.idom(cur) {
            if let Some(di) = func.layout.last_inst(d)
                && let InstructionData::Brif { arg: c, blocks, .. } = func.dfg.insts[di]
                && let Some(e) = guard_edge(func, &cfg, c, x)
            {
                let tgt = blocks[e].block(&func.dfg.value_lists);
                if tgt != blocks[1 - e].block(&func.dfg.value_lists)
                    && cfg.pred_iter(tgt).count() == 1
                    && dt.dominates(tgt, b, &func.layout)
                {
                    todo.push(t);
                    break;
                }
            }
            cur = d;
        }
    }
    for &t in &todo {
        let mut pos = FuncCursor::new(func).at_inst(t);
        let z = pos.ins().iconst(types::I8, 0);
        if let InstructionData::Brif { arg, .. } = &mut func.dfg.insts[t] {
            *arg = z;
        }
    }
    todo.len()
}

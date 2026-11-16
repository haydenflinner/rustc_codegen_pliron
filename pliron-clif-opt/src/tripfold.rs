//! Drop a zero-based trip counter that steps in lockstep with an index.
//!
//! `for _ in 0..bound` beside `i = i0; i += 1` tests `counter < bound`.
//! When every entry into the loop has already proved that
//! `i0 + (bound - 1)` does not wrap (`uadd_overflow` with a false
//! flag, and the sum below a non-zero constant), `counter < bound`
//! is `i < i0 + bound`. The counter's slot becomes that limit: the
//! entry computes it once, the backedge forwards it, and the
//! increment leaves the loop.
//!
//! `PLIRON_TRIPFOLD=1` enables it. Native was not timed.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::FxHashSet;

use crate::pipeline::{Analysis, CfgDtLa};

const MAX_LOOPS: usize = 32;

struct Plan {
    icmp: Inst,
    /// Header param that held the counter and will hold the limit.
    c: Value,
    /// Header param of the lockstep index.
    i: Value,
    c_slot: usize,
    bound: Value,
    /// Entry edges: `(branch, dest slot, i0 on that edge)`.
    entries: Vec<(Inst, usize, Value)>,
    /// Backedges: `(branch, dest slot, the value that was incremented)`.
    backs: Vec<(Inst, usize, Value)>,
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

fn plus_one(func: &Function, v: Value) -> Option<Value> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args: [a, b],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    if iconst(func, b) == Some(1) {
        Some(func.dfg.resolve_aliases(a))
    } else if iconst(func, a) == Some(1) {
        Some(func.dfg.resolve_aliases(b))
    } else {
        None
    }
}

/// `base + k`, or `base - k` as a negative `k`.
fn affine_k(func: &Function, v: Value) -> Option<(Value, i64)> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            if let Some(k) = iconst(func, b) {
                Some((func.dfg.resolve_aliases(a), k))
            } else {
                iconst(func, a).map(|k| (func.dfg.resolve_aliases(b), k))
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args: [a, b],
        } => iconst(func, b).map(|k| (func.dfg.resolve_aliases(a), k.wrapping_neg())),
        _ => None,
    }
}

/// `v` is `bound - 1`, including when both are `length + k` and the
/// constants differ by one (`length-2` against a bound of `length-1`).
fn is_pred(func: &Function, v: Value, bound: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let bound = func.dfg.resolve_aliases(bound);
    if let Some((base, k)) = affine_k(func, v)
        && base == bound
        && k == -1
    {
        return true;
    }
    match (affine_k(func, v), affine_k(func, bound)) {
        (Some((b1, k1)), Some((b2, k2))) => b1 == b2 && k1 == k2.wrapping_sub(1),
        _ => false,
    }
}

fn edge_arg(func: &Function, inst: Inst, slot: usize, idx: usize) -> Option<Value> {
    let bc = func.dfg.insts[inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .get(slot)?;
    match bc.args(&func.dfg.value_lists).nth(idx)? {
        BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
        _ => None,
    }
}

fn header_edges(func: &Function, cfg: &ControlFlowGraph, body: &FxHashSet<Block>, h: Block) -> Vec<(Inst, usize, bool)> {
    let mut out = Vec::new();
    for p in cfg.pred_iter(h) {
        let src_in = func.layout.inst_block(p.inst).is_some_and(|b| body.contains(&b));
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) == h {
                out.push((p.inst, slot, src_in));
            }
        }
    }
    out
}

/// Block params in the loop that every incoming edge feeds with `root`
/// (or with another such param). The header itself is not re-derived.
fn copies(
    func: &Function,
    cfg: &ControlFlowGraph,
    body: &FxHashSet<Block>,
    h: Block,
    root: Value,
) -> FxHashSet<Value> {
    let root = func.dfg.resolve_aliases(root);
    let mut set: FxHashSet<Value> = [root].into_iter().collect();
    for _ in 0..4 {
        let mut grew = false;
        for &b in body {
            if b == h {
                continue;
            }
            let params: Vec<Value> = func.dfg.block_params(b).to_vec();
            for (idx, &p) in params.iter().enumerate() {
                let p = func.dfg.resolve_aliases(p);
                if set.contains(&p) {
                    continue;
                }
                let mut ok = true;
                let mut any = false;
                for pred in cfg.pred_iter(b) {
                    for (slot, bc) in func.dfg.insts[pred.inst]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .iter()
                        .enumerate()
                    {
                        if bc.block(&func.dfg.value_lists) != b {
                            continue;
                        }
                        any = true;
                        match edge_arg(func, pred.inst, slot, idx) {
                            Some(a) if set.contains(&a) => {}
                            _ => ok = false,
                        }
                    }
                }
                if any && ok && set.insert(p) {
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    set
}

/// The true arm of `inst` is taken only when `i0 + (bound - 1)` cannot wrap.
fn proved(func: &Function, inst: Inst, slot: usize, i0: Value, bound: Value) -> bool {
    if slot != 0 {
        return false;
    }
    let InstructionData::Brif { arg: cond, .. } = func.dfg.insts[inst] else {
        return false;
    };
    let cond = func.dfg.resolve_aliases(cond);
    let ValueDef::Result(bi, _) = func.dfg.value_def(cond) else {
        return false;
    };
    let InstructionData::Binary {
        opcode: Opcode::Band,
        args: [x, y],
    } = func.dfg.insts[bi]
    else {
        return false;
    };
    let mut sum = None;
    let mut flag = None;
    for v in [x, y] {
        let v = func.dfg.resolve_aliases(v);
        let ValueDef::Result(ii, _) = func.dfg.value_def(v) else {
            return false;
        };
        let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[ii] else {
            return false;
        };
        match cond {
            IntCC::UnsignedLessThan => {
                let k = iconst(func, args[1]);
                if k.is_some_and(|k| k != 0) {
                    sum = Some(func.dfg.resolve_aliases(args[0]));
                } else {
                    return false;
                }
            }
            IntCC::Equal if iconst(func, args[1]) == Some(0) => {
                flag = Some(func.dfg.resolve_aliases(args[0]));
            }
            _ => return false,
        }
    }
    let (Some(sum), Some(flag)) = (sum, flag) else {
        return false;
    };
    let (ValueDef::Result(si, sr), ValueDef::Result(fi, fr)) =
        (func.dfg.value_def(sum), func.dfg.value_def(flag))
    else {
        return false;
    };
    // Result 0 is the sum, result 1 the overflow flag.
    if si != fi || sr != 0 || fr != 1 {
        return false;
    }
    let InstructionData::Binary {
        opcode: Opcode::UaddOverflow,
        args,
    } = func.dfg.insts[si]
    else {
        return false;
    };
    let a = func.dfg.resolve_aliases(args[0]);
    let b = func.dfg.resolve_aliases(args[1]);
    let i0 = func.dfg.resolve_aliases(i0);
    let pred = if is_pred(func, a, bound) {
        b == i0
    } else if is_pred(func, b, bound) {
        a == i0
    } else {
        false
    };
    pred
}

fn uses_only(
    func: &Function,
    body: &FxHashSet<Block>,
    copies: &FxHashSet<Value>,
    icmp: Inst,
) -> bool {
    for &b in body {
        for i in func.layout.block_insts(b) {
            if i == icmp {
                continue;
            }
            let args: Vec<Value> = func.dfg.inst_values(i).map(|v| func.dfg.resolve_aliases(v)).collect();
            if !args.iter().any(|v| copies.contains(v)) {
                continue;
            }
            // The +1 that feeds the backedge, or a branch that forwards
            // the param as an argument (not as the branch condition).
            let op = func.dfg.insts[i].opcode();
            let fixed = op.constraints().num_fixed_value_arguments();
            let step = matches!(
                func.dfg.insts[i],
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    ..
                }
            ) && func
                .dfg
                .inst_args(i)
                .iter()
                .any(|&v| iconst(func, v) == Some(1));
            for (k, &v) in func.dfg.inst_args(i).iter().enumerate() {
                if !copies.contains(&func.dfg.resolve_aliases(v)) {
                    continue;
                }
                let forwarded = op.is_branch() && k >= fixed;
                if !step && !forwarded {
                    return false;
                }
            }
        }
    }
    true
}

fn plan_loop(
    func: &Function,
    cfg: &ControlFlowGraph,
    la: &LoopAnalysis,
    lp: Loop,
) -> Option<Plan> {
    let h = la.loop_header(lp);
    let body: FxHashSet<Block> = func.layout.blocks().filter(|&b| la.is_in_loop(b, lp)).collect();
    let params: Vec<Value> = func.dfg.block_params(h).iter().copied().map(|p| func.dfg.resolve_aliases(p)).collect();
    let raw = header_edges(func, cfg, &body, h);
    if raw.is_empty() {
        return None;
    }
    // A counter: entry args are 0, backedges are +1, one `ult` against an invariant.
    for (c_slot, &c) in params.iter().enumerate() {
        let cs = copies(func, cfg, &body, h, c);
        let mut entries = Vec::new();
        let mut backs = Vec::new();
        let mut shaped = true;
        for &(inst, slot, in_body) in &raw {
            let arg = edge_arg(func, inst, slot, c_slot)?;
            if in_body {
                let base = plus_one(func, arg)?;
                if !cs.contains(&base) {
                    shaped = false;
                    break;
                }
                backs.push((inst, slot, base));
            } else if iconst(func, arg) == Some(0) {
                entries.push((inst, slot));
            } else {
                shaped = false;
                break;
            }
        }
        if !shaped || entries.is_empty() || backs.is_empty() || !step_once(func, &body, &backs, c_slot) {
            continue;
        }
        let mut icmp = None;
        let mut bound = None;
        for i in func.layout.block_insts(h) {
            let InstructionData::IntCompare {
                cond: IntCC::UnsignedLessThan,
                args: [a, b],
                ..
            } = func.dfg.insts[i]
            else {
                continue;
            };
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            if a == c && def_outside(func, &body, b) {
                if icmp.is_some() {
                    icmp = None;
                    break;
                }
                icmp = Some(i);
                bound = Some(b);
            }
        }
        let (Some(icmp), Some(bound)) = (icmp, bound) else {
            continue;
        };
        if !uses_only(func, &body, &cs, icmp) {
            continue;
        }
        // The other header param that steps by +1 and whose entry value
        // is the `i0` the overflow check mentions.
        for (i_slot, &iv) in params.iter().enumerate() {
            if i_slot == c_slot {
                continue;
            }
            let iset = copies(func, cfg, &body, h, iv);
            let mut i_ok = true;
            for &(inst, slot, in_body) in &raw {
                if !in_body {
                    continue;
                }
                let arg = edge_arg(func, inst, slot, i_slot)?;
                let base = plus_one(func, arg)?;
                if !iset.contains(&base) {
                    i_ok = false;
                    break;
                }
            }
            if !i_ok {
                continue;
            }
            let mut ent = Vec::new();
            let mut proved_all = true;
            for &(inst, slot) in &entries {
                let i0 = edge_arg(func, inst, slot, i_slot)?;
                if !proved(func, inst, slot, i0, bound) {
                    proved_all = false;
                    break;
                }
                ent.push((inst, slot, i0));
            }
            if !proved_all {
                continue;
            }
            return Some(Plan {
                icmp,
                c,
                i: iv,
                c_slot,
                bound,
                entries: ent,
                backs,
            });
        }
    }
    None
}

/// The `c + 1` result on each backedge is that edge's only use of it.
/// Leaving another use would increment the limit.
fn step_once(
    func: &Function,
    body: &FxHashSet<Block>,
    backs: &[(Inst, usize, Value)],
    c_slot: usize,
) -> bool {
    let vals: Vec<Value> = backs
        .iter()
        .filter_map(|&(inst, slot, _)| edge_arg(func, inst, slot, c_slot))
        .collect();
    if vals.len() != backs.len() {
        return false;
    }
    let mut cnt = vec![0u32; vals.len()];
    for &b in body {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                for (k, w) in vals.iter().enumerate() {
                    if v == *w {
                        cnt[k] += 1;
                    }
                }
            }
        }
    }
    cnt.iter().all(|&c| c == 1)
}

fn def_outside(func: &Function, body: &FxHashSet<Block>, v: Value) -> bool {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i).is_some_and(|b| !body.contains(&b)),
        ValueDef::Param(b, _) => !body.contains(&b),
        ValueDef::Union(_, _) => false,
    }
}

fn apply(func: &mut Function, p: &Plan) {
    for &(inst, slot, i0) in &p.entries {
        let mut pos = FuncCursor::new(func).at_inst(inst);
        let lim = pos.ins().iadd(i0, p.bound);
        replace_arg(func, inst, slot, p.c_slot, lim);
    }
    for &(inst, slot, base) in &p.backs {
        replace_arg(func, inst, slot, p.c_slot, base);
    }
    if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[p.icmp] {
        *args = [p.i, p.c];
    }
}

fn replace_arg(func: &mut Function, inst: Inst, slot: usize, idx: usize, v: Value) {
    let dfg = &mut func.dfg;
    let dests = dfg.insts[inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
    let mut k = 0;
    dests[slot].update_args(&mut dfg.value_lists, |a| {
        let r = if k == idx { BlockArg::Value(v) } else { a };
        k += 1;
        r
    });
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let plans = {
        let CfgDtLa { cfg, la, .. } = an.cfg_dt_la(func);
        let mut plans = Vec::new();
        for lp in la.loops().take(MAX_LOOPS) {
            if let Some(p) = plan_loop(func, cfg, la, lp) {
                plans.push(p);
            }
        }
        plans
    };
    let n = plans.len();
    for p in &plans {
        apply(func, p);
    }
    if n > 0 {
        crate::jumpthread::remove_dead_insts(func, true);
    }
    n
}

//! Equal-length reverse compares (`Big::cmp`'s two `base[..sz].rev()`).
//!
//! Both cursors start at `end - step` and walk by the same `step` until
//! they hit `base`. When `end - base` is the same value for both, they
//! hit `base` on the same iteration, so the second end test and the
//! select that freezes the exhausted side are dead. The remaining test
//! returns `Equal` (the flag it used to pass is zero on that iteration).
//!
//! `PLIRON_REVCMP=1` enables it. Native was not timed.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};

const MAX_FOLDS: usize = 32;
const EXPOSE_BUDGET: u32 = 8;

#[derive(Clone, Copy)]
struct Edge {
    inst: Inst,
    slot: u32,
}

#[derive(Clone, Copy)]
enum Len {
    V(Value),
}

struct Fold {
    /// Latch `brif` that exits when this side is exhausted.
    latch_brif: Inst,
    sel: Value,
    sel_inst: Inst,
    stepped: Value,
    /// The other side's end test. Its taken edge currently carries a
    /// "the other iterator still has elements" flag that is zero here.
    pair_brif: Inst,
    pair_exit: u32,
    /// `base`/`cursor` of the side whose exhaustion flag is passed to
    /// the exit. That flag is zero once the lengths match.
    flag_base: Value,
    flag_cursor: Value,
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let mut folds = Vec::new();
    for b in func.layout.blocks() {
        if folds.len() >= MAX_FOLDS {
            break;
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        if let Some(f) = match_latch(func, &cfg, b, term) {
            folds.push(f);
        }
    }
    let n = folds.len();
    for f in folds {
        apply(func, &f);
    }
    if n > 0 {
        crate::jumpthread::remove_dead_insts(func, true);
    }
    n
}

fn match_latch(func: &Function, cfg: &ControlFlowGraph, latch: Block, term: Inst) -> Option<Fold> {
    let InstructionData::Brif { arg: cond, blocks, .. } = func.dfg.insts[term] else {
        return None;
    };
    let cont = blocks[1];
    let header = cont.block(&func.dfg.value_lists);
    if header == latch {
        return None;
    }
    let (sel, sel_inst, stay, stepped, k) = select_step(func, latch, cond)?;
    let ib = arg_index(func, &cont, sel)?;
    let params = func.dfg.block_params(header);
    if params.get(ib).copied() != Some(stay) {
        return None;
    }
    let (ia, cursor_a) = other_step(func, &cont, params, k, ib)?;
    let (pre, _) = entry_arg(func, cfg, header, ib, Some(latch))?;
    let (_, init_b) = entry_arg(func, cfg, header, ib, Some(latch))?;
    let (_, init_a) = entry_arg(func, cfg, header, ia, Some(latch))?;
    let end_b = expose(func, cfg, init_b, Some(k), Some(pre), header, latch, EXPOSE_BUDGET)?;
    let end_a = expose(func, cfg, init_a, Some(k), Some(pre), header, latch, EXPOSE_BUDGET)?;
    let base_b = stay_base(func, cond, stay)?;
    if !outside(func, header, latch, base_b) {
        return None;
    }
    let len_b = len_of(func, end_b, base_b)?;
    if !outside_len(func, header, latch, &len_b) {
        return None;
    }
    let pair = paired(func, cfg, latch, header, cursor_a)?;
    let len_a = len_of(func, end_a, pair.base)?;
    if !same_len(func, &len_a, &len_b) || pair.base == base_b {
        return None;
    }
    Some(Fold {
        latch_brif: term,
        sel,
        sel_inst,
        stepped,
        pair_brif: pair.brif,
        pair_exit: pair.exit,
        flag_base: base_b,
        flag_cursor: stay,
    })
}

struct Pair {
    brif: Inst,
    exit: u32,
    base: Value,
}

/// The block that branches to `latch` when its cursor is not yet at `base`.
fn paired(
    func: &Function,
    cfg: &ControlFlowGraph,
    latch: Block,
    header: Block,
    cursor: Value,
) -> Option<Pair> {
    let mut pred: Option<Inst> = None;
    for p in cfg.pred_iter(latch) {
        if pred.is_some() {
            return None;
        }
        pred = Some(p.inst);
    }
    let brif = pred?;
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[brif] else {
        return None;
    };
    let cont = if blocks[1].block(&func.dfg.value_lists) == latch {
        1
    } else if blocks[0].block(&func.dfg.value_lists) == latch {
        0
    } else {
        return None;
    };
    let ci = func.dfg.resolved_value_def(arg).inst()?;
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [x, y],
    } = func.dfg.insts[ci]
    else {
        return None;
    };
    let polarity_ok = match cond {
        IntCC::Equal => cont == 1,
        IntCC::NotEqual => cont == 0,
        _ => false,
    };
    if !polarity_ok {
        return None;
    }
    let x = func.dfg.resolve_aliases(x);
    let y = func.dfg.resolve_aliases(y);
    let base = if x == cursor {
        y
    } else if y == cursor {
        x
    } else {
        return None;
    };
    if !outside(func, header, latch, base) {
        return None;
    }
    Some(Pair {
        brif,
        exit: 1 - cont,
        base,
    })
}

/// `select exhausted, cursor, cursor+k` with `k != 0`, condition `cond`.
fn select_step(
    func: &Function,
    latch: Block,
    cond: Value,
) -> Option<(Value, Inst, Value, Value, i64)> {
    let cond = func.dfg.resolve_aliases(cond);
    for i in func.layout.block_insts(latch) {
        let InstructionData::Ternary {
            opcode: Opcode::Select | Opcode::SelectSpectreGuard,
            args: [c, stay, stepped],
        } = func.dfg.insts[i]
        else {
            continue;
        };
        if func.dfg.resolve_aliases(c) != cond {
            continue;
        }
        let stay = func.dfg.resolve_aliases(stay);
        let (base, k) = add_const(func, stepped)?;
        if k == 0 || base != stay {
            continue;
        }
        let Some(r) = func.dfg.inst_results(i).first().copied() else {
            continue;
        };
        return Some((r, i, stay, func.dfg.resolve_aliases(stepped), k));
    }
    None
}

fn stay_base(func: &Function, cond: Value, stay: Value) -> Option<Value> {
    let ci = func.dfg.resolved_value_def(cond).inst()?;
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: IntCC::Equal,
        args: [x, y],
    } = func.dfg.insts[ci]
    else {
        return None;
    };
    let x = func.dfg.resolve_aliases(x);
    let y = func.dfg.resolve_aliases(y);
    if x == stay {
        Some(y)
    } else if y == stay {
        Some(x)
    } else {
        None
    }
}

/// Continue-edge arg that is `param+k` for a header param other than `ib`.
fn other_step(
    func: &Function,
    cont: &cranelift_codegen::ir::BlockCall,
    params: &[Value],
    k: i64,
    ib: usize,
) -> Option<(usize, Value)> {
    let mut found = None;
    for (_, arg) in cont.args(&func.dfg.value_lists).enumerate() {
        let BlockArg::Value(v) = arg else { continue };
        let Some((base, imm)) = add_const(func, v) else {
            continue;
        };
        if imm != k {
            continue;
        }
        let Some(ia) = params.iter().position(|&p| p == base) else {
            continue;
        };
        if ia == ib {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some((ia, params[ia]));
    }
    found
}

fn arg_index(
    func: &Function,
    cont: &cranelift_codegen::ir::BlockCall,
    sel: Value,
) -> Option<usize> {
    let sel = func.dfg.resolve_aliases(sel);
    let mut found = None;
    for (ai, arg) in cont.args(&func.dfg.value_lists).enumerate() {
        let BlockArg::Value(v) = arg else { continue };
        if func.dfg.resolve_aliases(v) == sel {
            if found.is_some() {
                return None;
            }
            found = Some(ai);
        }
    }
    found
}

fn apply(func: &mut Function, f: &Fold) {
    let InstructionData::Brif { blocks, .. } = func.dfg.insts[f.latch_brif] else {
        return;
    };
    let mut dest = blocks[1].deep_clone(&mut func.dfg.value_lists);
    let sel = f.sel;
    let stepped = f.stepped;
    let mut idxs = Vec::new();
    for (i, arg) in dest.args(&func.dfg.value_lists).enumerate() {
        if let BlockArg::Value(v) = arg
            && func.dfg.resolve_aliases(v) == sel
        {
            idxs.push(i);
        }
    }
    let mut k = 0usize;
    dest.update_args(&mut func.dfg.value_lists, |arg| {
        let r = if idxs.contains(&k) {
            BlockArg::Value(stepped)
        } else {
            arg
        };
        k += 1;
        r
    });
    func.dfg.insts[f.latch_brif] = InstructionData::Jump {
        opcode: Opcode::Jump,
        destination: dest,
    };
    let rs: Vec<Value> = func.dfg.inst_results(f.sel_inst).to_vec();
    func.dfg.detach_inst_results(f.sel_inst);
    for r in rs {
        func.dfg.change_to_alias(r, stepped);
    }
    func.layout.remove_inst(f.sel_inst);
    zero_flag(func, f);
}

fn zero_flag(func: &mut Function, f: &Fold) {
    let Some(ty) = flag_type(func, f) else {
        return;
    };
    if ty.bits() > 8 {
        return;
    }
    let exit = f.pair_exit as usize;
    let mut which = Vec::new();
    if let InstructionData::Brif { blocks, .. } = func.dfg.insts[f.pair_brif] {
        for (i, arg) in blocks[exit].args(&func.dfg.value_lists).enumerate() {
            if let BlockArg::Value(v) = arg
                && is_flag(func, v, f.flag_base, f.flag_cursor)
            {
                which.push(i);
            }
        }
    }
    if which.is_empty() {
        return;
    }
    let mut pos = FuncCursor::new(func).at_inst(f.pair_brif);
    let z = pos.ins().iconst(ty, 0);
    let dfg = &mut pos.func.dfg;
    let mut data = dfg.insts[f.pair_brif];
    let mut k = 0usize;
    data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[exit].update_args(
        &mut dfg.value_lists,
        |arg| {
            let r = if which.contains(&k) {
                BlockArg::Value(z)
            } else {
                arg
            };
            k += 1;
            r
        },
    );
    dfg.insts[f.pair_brif] = data;
}

fn flag_type(func: &Function, f: &Fold) -> Option<cranelift_codegen::ir::Type> {
    let InstructionData::Brif { blocks, .. } = func.dfg.insts[f.pair_brif] else {
        return None;
    };
    for arg in blocks[f.pair_exit as usize].args(&func.dfg.value_lists) {
        let BlockArg::Value(v) = arg else { continue };
        if is_flag(func, v, f.flag_base, f.flag_cursor) {
            return Some(func.dfg.value_type(func.dfg.resolve_aliases(v)));
        }
    }
    None
}

fn is_flag(func: &Function, v: Value, base: Value, cursor: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let Some(i) = func.dfg.resolved_value_def(v).inst() else {
        return false;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: IntCC::Equal | IntCC::NotEqual,
        args: [x, y],
    } = func.dfg.insts[i]
    else {
        return false;
    };
    let x = func.dfg.resolve_aliases(x);
    let y = func.dfg.resolve_aliases(y);
    (x == base && y == cursor) || (x == cursor && y == base)
}

fn entry_arg(
    func: &Function,
    cfg: &ControlFlowGraph,
    block: Block,
    idx: usize,
    skip: Option<Block>,
) -> Option<(Edge, Value)> {
    let mut found = None;
    for p in cfg.pred_iter(block) {
        if Some(p.block) == skip {
            continue;
        }
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) != block {
                continue;
            }
            let BlockArg::Value(v) = bc.args(&func.dfg.value_lists).nth(idx)? else {
                return None;
            };
            if found.is_some() {
                return None;
            }
            found = Some((
                Edge {
                    inst: p.inst,
                    slot: slot as u32,
                },
                func.dfg.resolve_aliases(v),
            ));
        }
    }
    found
}

/// Follow the value that arrives on `from` back to the pointer `end`
/// such that the cursor is `end+k` (one select, then block params).
fn expose(
    func: &Function,
    cfg: &ControlFlowGraph,
    v: Value,
    unwrap: Option<i64>,
    from: Option<Edge>,
    header: Block,
    latch: Block,
    budget: u32,
) -> Option<Value> {
    if budget == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if let Some((c, t, f)) = as_select(func, v) {
        let edge = from.as_ref()?;
        let arm = select_arm(func, edge, c, t, f)?;
        return expose(func, cfg, arm, unwrap, from, header, latch, budget - 1);
    }
    if let ValueDef::Param(b, idx) = func.dfg.resolved_value_def(v) {
        let skip = if b == header { Some(latch) } else { None };
        let (e, arg) = entry_arg(func, cfg, b, idx, skip)?;
        return expose(func, cfg, arg, unwrap, Some(e), header, latch, budget - 1);
    }
    if let Some(k) = unwrap {
        let (end, imm) = add_const(func, v)?;
        if imm != k {
            return None;
        }
        return expose(func, cfg, end, None, None, header, latch, budget - 1);
    }
    Some(v)
}

fn select_arm(func: &Function, edge: &Edge, c: Value, t: Value, f: Value) -> Option<Value> {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[edge.inst] else {
        return None;
    };
    if func.dfg.resolve_aliases(arg) != func.dfg.resolve_aliases(c) {
        return None;
    }
    Some(if edge.slot == 0 { t } else { f })
}

fn as_select(func: &Function, v: Value) -> Option<(Value, Value, Value)> {
    let i = func.dfg.resolved_value_def(v).inst()?;
    let InstructionData::Ternary {
        opcode: Opcode::Select | Opcode::SelectSpectreGuard,
        args: [c, t, f],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    Some((
        func.dfg.resolve_aliases(c),
        func.dfg.resolve_aliases(t),
        func.dfg.resolve_aliases(f),
    ))
}

fn add_const(func: &Function, v: Value) -> Option<(Value, i64)> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.resolved_value_def(v).inst()?;
    let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args: [a, b],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let a = func.dfg.resolve_aliases(a);
    let b = func.dfg.resolve_aliases(b);
    if let Some(k) = iconst(func, b) {
        if k != 0 {
            return Some((a, k));
        }
    }
    if let Some(k) = iconst(func, a) {
        if k != 0 {
            return Some((b, k));
        }
    }
    None
}

fn len_of(func: &Function, end: Value, base: Value) -> Option<Len> {
    let end = func.dfg.resolve_aliases(end);
    let i = func.dfg.resolved_value_def(end).inst()?;
    let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args: [a, b],
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let a = func.dfg.resolve_aliases(a);
    let b = func.dfg.resolve_aliases(b);
    if a == base {
        Some(Len::V(b))
    } else if b == base {
        Some(Len::V(a))
    } else {
        None
    }
}

fn same_len(func: &Function, a: &Len, b: &Len) -> bool {
    match (*a, *b) {
        (Len::V(x), Len::V(y)) => x == y || iconsts_eq(func, x, y),
    }
}

fn iconsts_eq(func: &Function, a: Value, b: Value) -> bool {
    match (iconst(func, a), iconst(func, b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.resolved_value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

fn outside(func: &Function, header: Block, latch: Block, v: Value) -> bool {
    match func.dfg.resolved_value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Param(b, _) => b != header && b != latch,
        ValueDef::Result(i, _) => func
            .layout
            .inst_block(i)
            .is_some_and(|b| b != header && b != latch),
        ValueDef::Union(..) => false,
    }
}

fn outside_len(func: &Function, header: Block, latch: Block, len: &Len) -> bool {
    match *len {
        Len::V(v) => outside(func, header, latch, v),
    }
}

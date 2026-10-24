//! Twin-arm merge: `brif c, T, E` where T and E are isomorphic — the same
//! insts in the same order, identical opcodes/immediates, differing only
//! in operand values — collapses to a single copy in a new block `M`
//! whose differing operands become `select c`. Sibling blocks get this
//! shape from `taildup`'s per-edge clones and the branch-dup passes: e.g.
//! `binary_search`'s two update arms (`left = mid+1` vs `size -= half`,
//! then `brif (n >= 2) loop exit` carrying different lows). On x64 the
//! merge trades a data-dependent branch per iteration for `cmov`s, which
//! wins inside loops and is at worst neutral elsewhere.
//!
//! Legality: both arms must be reachable solely via `b`'s `brif` (single
//! incoming edge each), so merging preserves execution counts, and every
//! operand pair either resolves equal (through each arm's param binding),
//! names the two arms' positional inst results, or becomes a `select`.
//! Stores/calls/traps survive unchanged — the merged block runs exactly
//! the insts the taken arm would have.
//!
//! The degenerate `brif c, B(a..), B(b..)` form (same dest, differing edge
//! args) becomes `jump B(select c ai bi ..)` directly.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    Block, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
};
use rustc_hash::FxHashMap;

/// Max non-terminator insts per arm — keeps icache/register pressure sane.
const MAX_INSTS: usize = 24;
/// Max `select` operands introduced per merge.
const MAX_SELS: usize = 8;

/// Per-operand-position decision for a merged inst.
#[derive(Clone, Copy)]
enum Opnd {
    /// Use this value (same on both paths, or a T-local result — emit
    /// maps the latter through the result map).
    Use(Value),
    /// `select c, at, ae` — index into the dedup'd diff list.
    Sel(u32),
}

struct Work {
    /// The block carrying the `brif`.
    b: Block,
    cond: Value,
    t: Block,
    e: Block,
    /// `brif`'s edge args to T/E — T's/E's param bindings (the `same`
    /// case reuses both lists for the shared dest).
    targs: Vec<Value>,
    eargs: Vec<Value>,
    /// (T-operand, E-operand) pairs needing `select`, dedup'd.
    diffs: Vec<(Value, Value)>,
    /// Per arm-inst: (T's inst, E's inst, operand ops in `arguments`
    /// then per-destination order — the `map_values` visit order).
    insts: Vec<(Inst, Inst, Vec<Opnd>)>,
    /// `t == e`: merge edge args only.
    same: bool,
}

/// Incoming edge count per block.
fn pred_counts(func: &Function) -> FxHashMap<Block, u32> {
    let mut n: FxHashMap<Block, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for bc in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            *n.entry(bc.block(&func.dfg.value_lists)).or_default() += 1;
        }
    }
    n
}

/// `v` through the arm's param substitution then aliases.
fn rsub(func: &Function, psub: &FxHashMap<Value, Value>, v: Value) -> Value {
    let v = func.dfg.resolve_aliases(v);
    match psub.get(&v) {
        Some(&x) => func.dfg.resolve_aliases(x),
        None => v,
    }
}

/// Both types equal and scalar — `select`/`cmov` has no vector lowering
/// wrinkles worth the trouble here.
fn sel_ok(func: &Function, a: Value, b: Value) -> bool {
    let t = func.dfg.value_type(a);
    t == func.dfg.value_type(b) && !t.is_vector()
}

/// Identical opcode plus every non-value field — the parts `map_values`
/// can't rewrite. Anything exotic bails via the catchall.
fn same_shape(t: &InstructionData, e: &InstructionData) -> bool {
    use InstructionData as D;
    if t.opcode() != e.opcode() {
        return false;
    }
    match (t, e) {
        (D::Binary { .. }, D::Binary { .. })
        | (D::Unary { .. }, D::Unary { .. })
        | (D::Ternary { .. }, D::Ternary { .. })
        | (D::MultiAry { .. }, D::MultiAry { .. })
        | (D::Jump { .. }, D::Jump { .. })
        | (D::Brif { .. }, D::Brif { .. })
        | (D::NullAry { .. }, D::NullAry { .. })
        | (D::LoadNoOffset { .. }, D::LoadNoOffset { .. })
        | (D::StoreNoOffset { .. }, D::StoreNoOffset { .. }) => true,
        (D::UnaryImm { imm: a, .. }, D::UnaryImm { imm: b, .. }) => a.bits() == b.bits(),
        (D::UnaryIeee16 { imm: a, .. }, D::UnaryIeee16 { imm: b, .. }) => a.bits() == b.bits(),
        (D::UnaryIeee32 { imm: a, .. }, D::UnaryIeee32 { imm: b, .. }) => a.bits() == b.bits(),
        (D::UnaryIeee64 { imm: a, .. }, D::UnaryIeee64 { imm: b, .. }) => a.bits() == b.bits(),
        (
            D::UnaryConst {
                constant_handle: a,
                ..
            },
            D::UnaryConst {
                constant_handle: b,
                ..
            },
        ) => a == b,
        (D::BinaryImm8 { imm: a, .. }, D::BinaryImm8 { imm: b, .. })
        | (D::TernaryImm8 { imm: a, .. }, D::TernaryImm8 { imm: b, .. }) => a == b,
        (D::Shuffle { imm: a, .. }, D::Shuffle { imm: b, .. }) => a == b,
        (D::IntCompare { cond: a, .. }, D::IntCompare { cond: b, .. }) => a == b,
        (D::FloatCompare { cond: a, .. }, D::FloatCompare { cond: b, .. }) => a == b,
        (
            D::Load {
                flags: f1,
                offset: o1,
                ..
            },
            D::Load {
                flags: f2,
                offset: o2,
                ..
            },
        )
        | (
            D::Store {
                flags: f1,
                offset: o1,
                ..
            },
            D::Store {
                flags: f2,
                offset: o2,
                ..
            },
        )
        | (
            D::Scatter {
                flags: f1,
                offset: o1,
                ..
            },
            D::Scatter {
                flags: f2,
                offset: o2,
                ..
            },
        ) => f1 == f2 && o1 == o2,
        (D::Call { func_ref: a, .. }, D::Call { func_ref: b, .. }) => a == b,
        (D::CallIndirect { sig_ref: a, .. }, D::CallIndirect { sig_ref: b, .. }) => a == b,
        (
            D::AtomicRmw {
                op: a,
                flags: f1,
                ..
            },
            D::AtomicRmw {
                op: b,
                flags: f2,
                ..
            },
        ) => a == b && f1 == f2,
        (D::AtomicCas { flags: f1, .. }, D::AtomicCas { flags: f2, .. }) => f1 == f2,
        (D::CondTrap { code: a, .. }, D::CondTrap { code: b, .. })
        | (D::IntAddTrap { code: a, .. }, D::IntAddTrap { code: b, .. })
        | (D::Trap { code: a, .. }, D::Trap { code: b, .. }) => a == b,
        (D::FuncAddr { func_ref: a, .. }, D::FuncAddr { func_ref: b, .. }) => a == b,
        _ => false,
    }
}

/// All operands of `i` in `map_values` order: `arguments`, then each
/// branch destination's edge args.
fn ops_of(func: &Function, i: Inst) -> Vec<Value> {
    let mut v: Vec<Value> = func.dfg.inst_args(i).to_vec();
    for bc in func.dfg.insts[i]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
    {
        v.extend(bc.args(&func.dfg.value_lists).filter_map(|a| match a {
            cranelift_codegen::ir::BlockArg::Value(v) => Some(v),
            _ => None,
        }));
    }
    v
}

/// Detect a mergeable `brif` terminator of `b` and record the rewrite.
fn plan(func: &Function, preds: &FxHashMap<Block, u32>, b: Block) -> Option<Work> {
    let ti = func.layout.last_inst(b)?;
    let InstructionData::Brif {
        arg: cond, blocks, ..
    } = func.dfg.insts[ti]
    else {
        return None;
    };
    let t = blocks[0].block(&func.dfg.value_lists);
    let e = blocks[1].block(&func.dfg.value_lists);
    let edge_args = |slot: usize| -> Vec<Value> {
        blocks[slot]
            .args(&func.dfg.value_lists)
            .map(|x| match x {
                cranelift_codegen::ir::BlockArg::Value(v) => v,
                _ => Value::from_u32(0),
            })
            .collect()
    };
    let targs = edge_args(0);
    let eargs = edge_args(1);
    if t == e {
        // `brif c, B(a..), B(b..)` — selects on the differing edge args.
        if t == b || targs.len() != eargs.len() {
            return None;
        }
        let mut diffs = Vec::new();
        let mut ops = Vec::new();
        let mut sel_of: FxHashMap<(Value, Value), u32> = FxHashMap::default();
        for (&x, &y) in targs.iter().zip(eargs.iter()) {
            let x = func.dfg.resolve_aliases(x);
            let y = func.dfg.resolve_aliases(y);
            if x == y {
                ops.push(Opnd::Use(x));
            } else if sel_ok(func, x, y) && diffs.len() < MAX_SELS {
                let i = *sel_of.entry((x, y)).or_insert(diffs.len() as u32);
                if i as usize == diffs.len() {
                    diffs.push((x, y));
                }
                ops.push(Opnd::Sel(i));
            } else {
                return None;
            }
        }
        return Some(Work {
            b,
            cond,
            t,
            e,
            targs,
            eargs,
            diffs,
            insts: vec![(ti, ti, ops)],
            same: true,
        });
    }
    if t == b || e == b || func.layout.is_cold(t) != func.layout.is_cold(e) {
        return None;
    }
    if preds.get(&t).copied().unwrap_or(0) != 1 || preds.get(&e).copied().unwrap_or(0) != 1 {
        return None;
    }
    // Arm params bind to b's edge args.
    if func.dfg.block_params(t).len() != targs.len()
        || func.dfg.block_params(e).len() != eargs.len()
    {
        return None;
    }
    let psub_t: FxHashMap<Value, Value> = func
        .dfg
        .block_params(t)
        .iter()
        .copied()
        .zip(targs.iter().copied())
        .collect();
    let psub_e: FxHashMap<Value, Value> = func
        .dfg
        .block_params(e)
        .iter()
        .copied()
        .zip(eargs.iter().copied())
        .collect();
    let it_list: Vec<Inst> = func.layout.block_insts(t).collect();
    let ie_list: Vec<Inst> = func.layout.block_insts(e).collect();
    if it_list.is_empty() || it_list.len() != ie_list.len() || it_list.len() > MAX_INSTS + 1 {
        return None;
    }
    let it = *it_list.last().unwrap();
    let ie = *ie_list.last().unwrap();
    if !func.dfg.insts[it].opcode().is_terminator()
        || !func.dfg.insts[ie].opcode().is_terminator()
        || !func
            .layout
            .block_insts(t)
            .take(it_list.len() - 1)
            .all(|i| !func.dfg.insts[i].opcode().is_terminator())
    {
        return None;
    }
    // value -> inst position in each arm (the local-result pairing).
    let mut loc_t: FxHashMap<Value, usize> = FxHashMap::default();
    let mut loc_e: FxHashMap<Value, usize> = FxHashMap::default();
    for (k, &i) in it_list.iter().enumerate() {
        for &r in func.dfg.inst_results(i) {
            loc_t.insert(r, k);
        }
    }
    for (k, &i) in ie_list.iter().enumerate() {
        for &r in func.dfg.inst_results(i) {
            loc_e.insert(r, k);
        }
    }
    // Terminator dest blocks must match pairwise — arms that branch to
    // different continuations aren't twins.
    let t_dests: Vec<Block> = func.dfg.insts[it]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .iter()
        .map(|bc| bc.block(&func.dfg.value_lists))
        .collect();
    let e_dests: Vec<Block> = func.dfg.insts[ie]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .iter()
        .map(|bc| bc.block(&func.dfg.value_lists))
        .collect();
    if t_dests != e_dests {
        return None;
    }
    let mut diffs: Vec<(Value, Value)> = Vec::new();
    let mut sel_of: FxHashMap<(Value, Value), u32> = FxHashMap::default();
    let mut insts: Vec<(Inst, Inst, Vec<Opnd>)> = Vec::new();
    for (&it, &ie) in it_list.iter().zip(ie_list.iter()) {
        let dt_ = func.dfg.insts[it];
        let de_ = func.dfg.insts[ie];
        // The controlling type isn't in `InstructionData`: `iconst.i8 0`
        // and `iconst.i32 0` (or `load.i8`/`load.i32` at one offset) share
        // a shape but not their results.
        if !same_shape(&dt_, &de_)
            || func.dfg.ctrl_typevar(it) != func.dfg.ctrl_typevar(ie)
            || !func
                .dfg
                .inst_results(it)
                .iter()
                .map(|&r| func.dfg.value_type(r))
                .eq(func.dfg.inst_results(ie).iter().map(|&r| func.dfg.value_type(r)))
        {
            return None;
        }
        let args_t = ops_of(func, it);
        let args_e = ops_of(func, ie);
        if args_t.len() != args_e.len() {
            return None;
        }
        let mut ops = Vec::with_capacity(args_t.len());
        for (&at_raw, &ae_raw) in args_t.iter().zip(args_e.iter()) {
            let at = rsub(func, &psub_t, at_raw);
            let ae = rsub(func, &psub_e, ae_raw);
            if at == ae {
                ops.push(Opnd::Use(at));
                continue;
            }
            match (loc_t.get(&at), loc_e.get(&ae)) {
                (Some(&jt), Some(&je)) if jt == je => {
                    // Corresponding results of isomorphic insts — the
                    // merged inst's result supplies the operand.
                    ops.push(Opnd::Use(at));
                }
                (None, None) if sel_ok(func, at, ae) => {
                    let i = *sel_of.entry((at, ae)).or_insert(diffs.len() as u32);
                    if i as usize == diffs.len() {
                        diffs.push((at, ae));
                    }
                    ops.push(Opnd::Sel(i));
                }
                _ => return None,
            }
        }
        insts.push((it, ie, ops));
    }
    if diffs.len() > MAX_SELS {
        return None;
    }
    Some(Work {
        b,
        cond,
        t,
        e,
        targs,
        eargs,
        diffs,
        insts,
        same: false,
    })
}

fn apply(func: &mut Function, w: Work) {
    let cond = w.cond;
    let ti = func.layout.last_inst(w.b).unwrap();
    if w.same {
        // Emit the selects just before `b`'s terminator, then rewrite it
        // to `jump B(merged args)`.
        let mut sels = Vec::new();
        {
            let mut pos = FuncCursor::new(func).at_inst(ti);
            for &(at, ae) in &w.diffs {
                sels.push(pos.ins().select(cond, at, ae));
            }
        }
        let args: Vec<Value> = w.insts[0]
            .2
            .iter()
            .map(|o| match o {
                Opnd::Use(v) => *v,
                Opnd::Sel(i) => sels[*i as usize],
            })
            .collect();
        let bc = BlockCall::new(w.t, args.iter().copied().map(cranelift_codegen::ir::BlockArg::Value), &mut func.dfg.value_lists);
        func.dfg.insts[ti] = InstructionData::Jump {
            opcode: Opcode::Jump,
            destination: bc,
        };
        return;
    }
    // Create M holding the selects + merged insts, then retarget b.
    let m = func.dfg.make_block();
    func.layout.insert_block_after(m, w.t);
    if func.layout.is_cold(w.t) {
        func.layout.set_cold(m);
    }
    let mut sels: Vec<Value> = Vec::new();
    let mut rmap: FxHashMap<Value, Value> = FxHashMap::default();
    {
        let mut pos = FuncCursor::new(func).at_bottom(m);
        for &(at, ae) in &w.diffs {
            sels.push(pos.ins().select(cond, at, ae));
        }
        for (it, _ie, ops) in &w.insts {
            let ni = pos.func.dfg.clone_inst(*it);
            // `-g`: the copy is the same source operation.
            pos.func.set_srcloc(ni, pos.func.srcloc(*it));
            let mut data = pos.func.dfg.insts[ni];
            let mut it2 = ops.iter().copied();
            {
                let dfg = &mut pos.func.dfg;
                data.map_values(
                    &mut dfg.value_lists,
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                    |v| match it2.next() {
                        Some(Opnd::Use(x)) => rmap.get(&x).copied().unwrap_or(x),
                        Some(Opnd::Sel(i)) => sels[i as usize],
                        None => v,
                    },
                );
            }
            pos.func.dfg.insts[ni] = data;
            pos.func.layout.append_inst(ni, m);
            let rt: Vec<Value> = pos.func.dfg.inst_results(*it).to_vec();
            let rm: Vec<Value> = pos.func.dfg.inst_results(ni).to_vec();
            for (x, y) in rt.into_iter().zip(rm) {
                rmap.insert(x, y);
            }
        }
    }
    // Outside uses of either arm's results resolve to the merged ones —
    // E's result at (inst j, res k) maps to M's clone of T-inst j res k.
    let m_insts: Vec<Inst> = func.layout.block_insts(m).collect();
    let mut aliased: Vec<(Value, Value)> = Vec::new();
    for ((it, ie, _), &ni) in w.insts.iter().zip(m_insts.iter().skip(w.diffs.len())) {
        let rm: Vec<Value> = func.dfg.inst_results(ni).to_vec();
        for (&rt_, &rm_) in func.dfg.inst_results(*it).iter().zip(rm.iter()) {
            aliased.push((rt_, rm_));
        }
        for (&re_, &rm_) in func.dfg.inst_results(*ie).iter().zip(rm.iter()) {
            aliased.push((re_, rm_));
        }
        func.dfg.detach_inst_results(*it);
        func.dfg.detach_inst_results(*ie);
    }
    for (old, new) in aliased {
        if old != new {
            func.dfg.change_to_alias(old, new);
        }
    }
    // Arm params resolve to the edge args that bound them.
    let tparams: Vec<Value> = func
        .dfg
        .detach_block_params(w.t)
        .as_slice(&func.dfg.value_lists)
        .to_vec();
    for (p, &a) in tparams.iter().copied().zip(w.targs.iter()) {
        let a = func.dfg.resolve_aliases(a);
        func.dfg.change_to_alias(p, a);
    }
    let eparams: Vec<Value> = func
        .dfg
        .detach_block_params(w.e)
        .as_slice(&func.dfg.value_lists)
        .to_vec();
    for (p, &a) in eparams.iter().copied().zip(w.eargs.iter()) {
        let a = func.dfg.resolve_aliases(a);
        func.dfg.change_to_alias(p, a);
    }
    // b's brif -> jump M (no params).
    let bc = BlockCall::new(m, [], &mut func.dfg.value_lists);
    func.dfg.insts[ti] = InstructionData::Jump {
        opcode: Opcode::Jump,
        destination: bc,
    };
    func.layout.remove_block_and_insts(w.t);
    func.layout.remove_block_and_insts(w.e);
}

pub fn run(func: &mut Function) -> usize {
    let preds = pred_counts(func);
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let entry = func.layout.entry_block();
    for b in blocks {
        if entry == Some(b) {
            continue;
        }
        let Some(w) = plan(func, &preds, b) else {
            continue;
        };
        apply(func, w);
        n += 1;
    }
    n
}

//! Reverse-iteration normalization.
//!
//! `for i in (lo..hi).rev()` (and friends) arrive from rustc as a counted
//! loop whose iv descends `hi → lo` by `-s`. Neither the count model nor
//! the stream analysis reads that direction, so every reverse loop falls
//! back to scalar. Rebase the iv to an ascending `j: 0 → hi-lo` with
//! `iv = hi - j` — an exact mod-2^64 bijection:
//!
//! - every in-body use of the old iv is replaced by `isub(hi, j)`;
//! - `iv ± k` / `k - iv` defs rebase to `(hi ± k) - j` / `(k - hi) + j`,
//!   with the `hi ± k` constants materialized in the preheader (keeps the
//!   canonical `C - iv` / `iv`-affine shapes the guard analysis reads);
//! - the stay test `iv != lo` / `iv > 0` becomes `j != hi-lo` / `j < hi`;
//! - the entry edge passes 0 and every latch passes `j + |s|`.
//!
//! Only `ne` (exact under the bijection for any step) and `ugt 0`
//! (exact for a unit decrement, which hits 0 before wrapping) stay
//! conditions qualify — inequality tests against other floors are not
//! residue-preserving when `hi < lo` or the step can skip the bound.
//!
//! `PLIRON_REVNORM=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::loopidiom::{gather, iconst, outv, param_kinds, Param};

pub(crate) fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let loops: Vec<Loop> = la.loops().collect();
    let mut n = 0;
    for lp in loops {
        if normalize(func, &cfg, &dt, &la, lp) {
            n += 1;
        }
    }
    n
}

/// Emit `isub(e, p)` — the rebased original-iv value — at the top of `b`.
fn iv_sub(
    func: &mut Function,
    subs: &mut FxHashMap<(Block, Value), Value>,
    b: Block,
    p: Value,
    e: Value,
) -> Value {
    if let Some(&v) = subs.get(&(b, p)) {
        return v;
    }
    let v = FuncCursor::new(func).at_first_inst(b).ins().isub(e, p);
    subs.insert((b, p), v);
    v
}

/// Emit `inst` before the edge's terminator.
fn emit_before(func: &mut Function, e: crate::loopidiom::Edge, f: impl FnOnce(&mut FuncCursor) -> Value) -> Value {
    let mut pos = FuncCursor::new(func).at_inst(e.inst);
    f(&mut pos)
}

/// Which header-param index does `p` occupy (and its entry value).
fn param_entry(params: &[Value], entry_args: &[Value], p: Value) -> (usize, Value) {
    let i = params.iter().position(|&q| q == p).unwrap();
    (i, entry_args[i])
}

fn normalize(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
) -> bool {
    let Some(info) = gather(func, cfg, dt, la, lp) else {
        return false;
    };
    let kinds = param_kinds(func, &info);
    let params = func.dfg.block_params(info.h).to_vec();
    if !info.body.contains(&info.h) {
        return false;
    }
    let desc: FxHashSet<Value> = params
        .iter()
        .zip(&kinds)
        .filter_map(|(&p, k)| matches!(k, Param::Step(s) if *s < 0).then_some(p))
        .collect();
    if desc.is_empty() {
        return false;
    }
    // Pre-tested stay: the header terminator is `brif icmp ...` with
    // exactly one destination inside the loop.
    let t = func.layout.last_inst(info.h).unwrap();
    let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else {
        return false;
    };
    let in_body = {
        let dests = func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        [
            info.body.contains(&dests[0].block(&func.dfg.value_lists)),
            info.body.contains(&dests[1].block(&func.dfg.value_lists)),
        ]
    };
    if in_body[0] == in_body[1] {
        return false;
    }
    // The brif polarity is kept, so the icmp must preserve the ORIGINAL
    // condition's truth value — just translated to j. Normalize the iv onto
    // the left operand.
    let c = func.dfg.resolve_aliases(arg);
    let ValueDef::Result(ci, _) = func.dfg.value_def(c) else {
        return false;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: cc,
        args: [x, y],
    } = func.dfg.insts[ci]
    else {
        return false;
    };
    // Find the desc param the test counts on: one side resolves to it, the
    // other is loop-invariant.
    let mut found: Option<(usize, Value, bool)> = None;
    for (a, b, on_right) in [(x, y, false), (y, x, true)] {
        let a = func.dfg.resolve_aliases(a);
        if !desc.contains(&a) {
            continue;
        }
        let Some(lo) = outv(func, &info, &kinds, b) else {
            continue;
        };
        found = Some((param_entry(&params, &info.entry_args, a).0, lo, on_right));
        break;
    }
    let Some((pidx, lo, on_right)) = found else {
        return false;
    };
    let mut scc = cc;
    if on_right {
        scc = scc.swap_args();
    }
    // The icmp result must feed only this brif — rewriting its operands
    // changes the value every other use would see.
    let used_elsewhere = |func: &Function, c: Value, ci: Inst| -> bool {
        func.layout.blocks().any(|b| {
            func.layout.block_insts(b).any(|i| {
                // `c` legitimately feeds the stay brif `t`'s cond operand.
                if i != ci
                    && i != t
                    && func
                        .dfg
                        .inst_args(i)
                        .iter()
                        .any(|&a| func.dfg.resolve_aliases(a) == c)
                {
                    return true;
                }
                func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .any(|bc| {
                        bc.args(&func.dfg.value_lists).any(|a| match a {
                            BlockArg::Value(v) => func.dfg.resolve_aliases(v) == c,
                            _ => false,
                        })
                    })
            })
        })
    };
    if used_elsewhere(func, c, ci) {
        return false;
    }
    let piv = params[pidx];
    let Param::Step(step) = kinds[pidx] else {
        return false;
    };
    let e = info.entry_args[pidx];
    // `(new cond, bound)` keeping the raw polarity: `iv == lo` ⟺
    // `j == e-lo`, `iv != lo` ⟺ `j != e-lo` (both exact mod-2^64 for any
    // step), and `iv > 0` ⟺ `j < e` for a unit decrement (hits 0 exactly).
    let (ncc, need_lo) = match scc {
        IntCC::Equal => (IntCC::Equal, true),
        IntCC::NotEqual => (IntCC::NotEqual, true),
        IntCC::UnsignedGreaterThan if step == -1 && iconst(func, lo) == Some(0) => {
            (IntCC::UnsignedLessThan, false)
        }
        _ => return false,
    };
    // `bnd` for j: `e - lo` (ne) or `e` (ugt 0).
    let bnd = if need_lo {
        emit_before(func, info.entry, |pos| pos.ins().isub(e, lo))
    } else {
        e
    };

    // Phase A: rebase `p ± k` / `k - p` defs onto `C ± j`, materializing the
    // `e`-relative constants in the preheader.
    let mut normed: FxHashSet<Inst> = FxHashSet::default();
    for &b in &info.body {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            if normed.contains(&i) || i == ci {
                continue;
            }
            let (new_op, new_args): (Opcode, [Value; 2]) = match func.dfg.insts[i] {
                InstructionData::Binary {
                    opcode: Opcode::Isub,
                    args: [a, k],
                } => {
                    let (a, k) = (
                        func.dfg.resolve_aliases(a),
                        func.dfg.resolve_aliases(k),
                    );
                    if desc.contains(&a) {
                        // iv - k = (e - k) - j
                        let Some(kv) = outv(func, &info, &kinds, k) else {
                            continue;
                        };
                        let (_, ev) = param_entry(&params, &info.entry_args, a);
                        let cst = emit_before(func, info.entry, |pos| pos.ins().isub(ev, kv));
                        (Opcode::Isub, [cst, a])
                    } else if desc.contains(&k) {
                        // k - iv = (k - e) + j
                        let Some(kv) = outv(func, &info, &kinds, a) else {
                            continue;
                        };
                        let (_, ev) = param_entry(&params, &info.entry_args, k);
                        let cst = emit_before(func, info.entry, |pos| pos.ins().isub(kv, ev));
                        (Opcode::Iadd, [cst, k])
                    } else {
                        continue;
                    }
                }
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args: [a, b2],
                } => {
                    let (a, b2) = (
                        func.dfg.resolve_aliases(a),
                        func.dfg.resolve_aliases(b2),
                    );
                    let (p, k) = if desc.contains(&a) && !desc.contains(&b2) {
                        (a, b2)
                    } else if desc.contains(&b2) && !desc.contains(&a) {
                        (b2, a)
                    } else {
                        continue;
                    };
                    // iv + k = (e + k) - j
                    let Some(kv) = outv(func, &info, &kinds, k) else {
                        continue;
                    };
                    let (_, ev) = param_entry(&params, &info.entry_args, p);
                    let cst = emit_before(func, info.entry, |pos| pos.ins().iadd(ev, kv));
                    (Opcode::Isub, [cst, p])
                }
                _ => continue,
            };
            match &mut func.dfg.insts[i] {
                InstructionData::Binary { opcode, args } => {
                    *opcode = new_op;
                    *args = new_args;
                }
                _ => unreachable!(),
            }
            normed.insert(i);
        }
    }

    // Phase B: every remaining use of a desc param reads `isub(e, j)`.
    // Scan ALL blocks, not just the loop body: a desc param also escapes
    // through exit edges — e.g. `insert_tail`'s hole pointer is the iv's
    // final value, read by the exit block after `brif stay, latch, exit`
    // (leaving it as `j` turned the hole into a small integer pointer).
    // Header-param uses are confined to blocks dominated by the header, so
    // blocks outside can't reference `p` anyway. `inst_values`/`map_inst_values`
    // cover inst args, branch-call args, and exception contexts uniformly.
    let mut subs: FxHashMap<(Block, Value), Value> = FxHashMap::default();
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            if i == ci || normed.contains(&i) {
                continue;
            }
            let mut repl: FxHashMap<Value, Value> = FxHashMap::default();
            let uses: Vec<Value> = func.dfg.inst_values(i).collect();
            for a in uses {
                let p = func.dfg.resolve_aliases(a);
                if desc.contains(&p) {
                    let (_, ev) = param_entry(&params, &info.entry_args, p);
                    let v = iv_sub(func, &mut subs, b, p, ev);
                    repl.insert(a, v);
                }
            }
            if !repl.is_empty() {
                func.dfg
                    .map_inst_values(i, |x| repl.get(&x).copied().unwrap_or(x));
            }
        }
    }

    // Phase C: edge args — entry passes `0`, latches pass `j + |s|`.
    for (i, &p) in params.iter().enumerate() {
        let Param::Step(s) = kinds[i] else { continue };
        if s >= 0 {
            continue;
        }
        let ty = func.dfg.value_type(p);
        let zero = emit_before(func, info.entry, |pos| pos.ins().iconst(ty, 0));
        {
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[info.entry.inst]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                [info.entry.slot];
            let mut cur = 0usize;
            bc.update_args(&mut dfg.value_lists, |a| {
                let r = if cur == i { BlockArg::Value(zero) } else { a };
                cur += 1;
                r
            });
        }
        for le in &info.latches {
            let le = *le;
            let nxt = emit_before(func, le, |pos| {
                let k = pos.ins().iconst(ty, -s);
                pos.ins().iadd(p, k)
            });
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[le.inst]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                [le.slot];
            let mut cur = 0usize;
            bc.update_args(&mut dfg.value_lists, |a| {
                let r = if cur == i { BlockArg::Value(nxt) } else { a };
                cur += 1;
                r
            });
        }
    }

    // Phase D: the count icmp compares j against the new bound.
    match &mut func.dfg.insts[ci] {
        InstructionData::IntCompare { cond, args, .. } => {
            *cond = ncc;
            *args = [piv, bnd];
        }
        _ => unreachable!(),
    }
    true
}

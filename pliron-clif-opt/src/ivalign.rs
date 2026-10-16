//! Shift a rotated loop's tested induction variable back one step so the
//! latch tests the incremented value — the `add; cmp; b.cond` do-while
//! shape LLVM emits, instead of `cmp; mov; b` plus a backedge copy.
//!
//! `bcheck`/`looprot` leave single-block loops that test a block param
//! `p` while the backedge carries `iadd p, s`: the compare reads the
//! pre-increment value, so `p` and `p + s` need different registers and
//! the copy lands in the hot path. When every entry edge computes its
//! `p` argument as `iadd a_i, s` with the same step `s`, the param can
//! be re-timed: entries pass `a_i` and all of `p`'s other uses switch to
//! the iadd's result, whose value after the shift is the old `p`. The
//! iadd then feeds the test *and* the backedge — register allocation
//! folds it into an in-place `add` on the carried register.
//!
//! The rewrite repurposes the iadd's value (it computes the old `p`,
//! not `p + s`), so it is only sound while the iadd's sole use is that
//! backedge argument — any other use would observe `p` shifted by `s`.
//!
//! `PLIRON_IVALIGN=0` disables it.

use cranelift_codegen::ir::{
    Block, BlockArg, DataFlowGraph, Function, Inst, InstructionData, Opcode, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::pipeline::Analysis;
use crate::uses::Uses;

/// One planned param shift: block `b`'s param at index `pi`.
struct Shift {
    b: Block,
    pi: usize,
    p: Value,
    /// `iadd p, s` in `b` — hoisted to the block top so its result
    /// dominates every use it gains.
    iadd: Inst,
    p_next: Value,
    /// Insts in `b` that used `p`; each reads `p_next` afterwards.
    subst: Vec<Inst>,
    /// (pred terminator, block-call index, entry `a_i`, entry iadd)
    /// per edge — the iadd is reaped if the strip leaves it dead.
    entries: Vec<(Inst, usize, Value, Inst)>,
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let uses = an.uses(func);
    let mut shifts = Vec::new();
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        if !matches!(func.dfg.insts[term], InstructionData::Brif { .. }) {
            continue;
        }
        let dests = func.dfg.insts[term]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        let mut self_edges = dests
            .iter()
            .enumerate()
            .filter(|(_, c)| c.block(&func.dfg.value_lists) == b);
        let (Some((self_di, _)), None) = (self_edges.next(), self_edges.next()) else {
            continue;
        };
        let params = func.dfg.block_params(b).to_vec();
        for (pi, p) in params.into_iter().enumerate() {
            if let Some(s) = analyze(func, an, &uses, b, pi, p, term, self_di) {
                shifts.push(s);
            }
        }
    }
    let n = shifts.len();
    for s in &shifts {
        apply(func, s);
    }
    // Entry `iadd a_i, s` computes exist only to feed the stripped edge
    // arg — reap the ones that went dead (a stray `add` on a loop
    // preheader still reaches codegen otherwise). One tally over the
    // function rather than a scan per candidate.
    if !shifts.is_empty() {
        let mut candidates: FxHashMap<Value, Inst> = FxHashMap::default();
        for s in &shifts {
            for (_, _, _, ei) in &s.entries {
                if func.layout.inst_block(*ei).is_some() {
                    candidates
                        .entry(func.dfg.resolve_aliases(func.dfg.first_result(*ei)))
                        .or_insert(*ei);
                }
            }
        }
        if !candidates.is_empty() {
            let mut live: FxHashSet<Value> = FxHashSet::default();
            for b in func.layout.blocks() {
                for i in func.layout.block_insts(b) {
                    for a in func.dfg.inst_values(i) {
                        let a = func.dfg.resolve_aliases(a);
                        if candidates.contains_key(&a) {
                            live.insert(a);
                        }
                    }
                }
            }
            for (v, ei) in candidates {
                if !live.contains(&v) {
                    func.dfg.clear_results(ei);
                    func.layout.remove_inst(ei);
                }
            }
        }
    }
    n
}

/// `a` names the same step as `s` — the same value, or `iconst`s with
/// equal payloads (sibling clones stamp fresh `iconst` values).
fn same_step(dfg: &DataFlowGraph, a: Value, s: Value) -> bool {
    if dfg.resolve_aliases(a) == s {
        return true;
    }
    if let (ValueDef::Result(ai, _), ValueDef::Result(si, _)) = (dfg.value_def(a), dfg.value_def(s))
    {
        matches!(
            (&dfg.insts[ai], &dfg.insts[si]),
            (
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm: x,
                },
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm: y,
                }
            ) if x.bits() == y.bits()
        )
    } else {
        false
    }
}

/// `p`'s args at entry edges must decompose as `iadd a_i, s`; returns
/// the non-step operand `a_i` and the iadd itself. `None` on a mismatch.
fn entry_arg(dfg: &DataFlowGraph, call_arg: BlockArg, s: Value) -> Option<(Value, Inst)> {
    let v = dfg.resolve_aliases(call_arg.as_value()?);
    let ValueDef::Result(i, _) = dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args,
    } = dfg.insts[i]
    else {
        return None;
    };
    if same_step(dfg, args[1], s) {
        Some((args[0], i))
    } else if same_step(dfg, args[0], s) {
        Some((args[1], i))
    } else {
        None
    }
}

fn analyze(
    func: &Function,
    an: &mut Analysis,
    uses: &Uses,
    b: Block,
    pi: usize,
    p: Value,
    term: Inst,
    self_di: usize,
) -> Option<Shift> {
    let dfg = &func.dfg;
    let (pool, jt, et) = (&dfg.value_lists, &dfg.jump_tables, &dfg.exception_tables);
    // The backedge's `p` arg must be `iadd p, s` computed in `b`.
    let p_next = dfg.resolve_aliases(
        dfg.insts[term].branch_destination(jt, et)[self_di]
            .args(pool)
            .nth(pi)?
            .as_value()?,
    );
    let ValueDef::Result(iadd, _) = dfg.value_def(p_next) else {
        return None;
    };
    let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args,
    } = dfg.insts[iadd]
    else {
        return None;
    };
    let s = if dfg.resolve_aliases(args[0]) == p {
        args[1]
    } else if dfg.resolve_aliases(args[1]) == p {
        args[0]
    } else {
        return None;
    };
    let s = dfg.resolve_aliases(s);
    // The iadd gets hoisted to the block top; `s` must be available there.
    if let ValueDef::Result(si, _) = dfg.value_def(s) {
        if func.layout.inst_block(si) == Some(b) {
            return None;
        }
    }
    // `p`'s uses must all live in `b` so `p_next` can reach them; the
    // iadd keeps `p`, everything else switches to `p_next`.
    let sites = uses.sites(p);
    let mut subst = Vec::new();
    for &(ub, ui) in sites {
        if ub != b {
            return None;
        }
        if ui != iadd {
            subst.push(ui);
        }
    }
    if subst.is_empty() {
        return None;
    }
    // `p_next`'s only use may be that backedge arg — after the shift it
    // carries the old `p` value, which is what `p`'s uses need but not
    // what a stray second consumer would expect.
    if uses.sites(p_next) != [(b, term)] {
        return None;
    }
    // Every other entry edge must pass `iadd a_i, s` with the same `s`.
    let cfg = an.cfg(func);
    let mut entries = Vec::new();
    for pred in cfg.pred_iter(b) {
        if pred.inst == term {
            continue;
        }
        let dests = dfg.insts[pred.inst].branch_destination(jt, et);
        for (di, call) in dests.iter().enumerate() {
            if call.block(pool) != b {
                continue;
            }
            let (a, ei) = entry_arg(dfg, call.args(pool).nth(pi)?, s)?;
            entries.push((pred.inst, di, a, ei));
        }
    }
    if entries.is_empty() {
        return None;
    }
    Some(Shift {
        b,
        pi,
        p,
        iadd,
        p_next,
        subst,
        entries,
    })
}

fn apply(func: &mut Function, s: &Shift) {
    // Hoist the increment so its result dominates the uses it gains;
    // `s` is not defined in `b`, so the block top is always legal.
    if let Some(first) = func.layout.first_inst(s.b) {
        if first != s.iadd {
            func.layout.remove_inst(s.iadd);
            func.layout.insert_inst(s.iadd, first);
        }
    }
    for &i in &s.subst {
        // `subst` was collected by resolved use site; a use reaching `p`
        // through an alias must still be rewritten, so compare resolved.
        let aliased: FxHashSet<Value> = func
            .dfg
            .inst_values(i)
            .filter(|&v| func.dfg.resolve_aliases(v) == s.p)
            .collect();
        func.dfg
            .map_inst_values(i, |v| if aliased.contains(&v) { s.p_next } else { v });
    }
    for &(inst, di, a, _) in &s.entries {
        let dfg = &mut func.dfg;
        let dests =
            dfg.insts[inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
        let mut k = 0;
        dests[di].update_args(&mut dfg.value_lists, |arg| {
            let r = if k == s.pi { BlockArg::Value(a) } else { arg };
            k += 1;
            r
        });
    }
}

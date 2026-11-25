//! Compute back-edge arguments at the edge tail (`PLIRON_ARGTAIL`).
//!
//! A loop-header parameter's live range covers the loop body; an arg
//! computed inside it shares that span, so regalloc2's `merge_bundles`
//! sees overlapping ranges, refuses the merge, and pays a register copy
//! on the back edge:
//!
//! ```text
//! P:  a = iadd p, 1          P:  ...              E: a' = iadd p, 1
//!     l = load r        =>       brif c, E, X  =>     jump H(a')
//!     brif c, H(a), X
//! ```
//!
//! Evaluating the arg at the edge tail (the bottom of a `jump`
//! predecessor, or a fresh edge block on a `brif` edge — the block the
//! lowering splits the critical edge for anyway) makes its range abut
//! the parameter's instead of overlapping it: the bundles merge and the
//! `iadd` writes the parameter's register directly — LLVM's
//! bottom-tested-loop shape. The def travels with its only uses, so its
//! operands (the header's parameters, live at the edge) stay valid.
//!
//! An arg qualifies when its def is one pure single-result inst and
//! every use is an argument slot on a back-edge call — each such use is
//! then a slot this pass can rewire, so the original def dies. Args
//! with ordinary inst uses keep their shared def and their moves.
//!
//! Runs at the very end of the pipeline: any GVN after it would merge
//! the per-edge copies back into the shared def it came from.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, Opcode, Value, ValueDef,
};
use rustc_hash::FxHashMap;

use crate::pipeline::{Analysis, CfgDtLa};

/// Per-edge copies one arg may pay for; beyond this the code growth
/// outweighs a copy on a back edge.
const MAX_SITES: usize = 4;

/// One arg-slot use of a value on a back edge: `term` of `pred` carries
/// it on call `side` into `dst`, at arg position `slot`.
#[derive(Clone, Copy)]
struct Site {
    pred: Block,
    term: Inst,
    side: usize,
    slot: usize,
    dst: Block,
}

/// `v`'s def if it is a movable back-edge arg def: pure in the
/// speculation sense, one result, and not a constant (a constant arg
/// needs the same `mov` on the edge; cloning it only renames).
fn movable(func: &Function, v: Value) -> Option<Inst> {
    let ValueDef::Result(d, _) = func.dfg.value_def(v) else {
        return None;
    };
    if func.dfg.inst_results(d).len() != 1 {
        return None;
    }
    let op = func.dfg.insts[d].opcode();
    if matches!(
        op,
        Opcode::Iconst
            | Opcode::F16const
            | Opcode::F32const
            | Opcode::F64const
            | Opcode::Vconst
            | Opcode::SymbolValue
            | Opcode::FuncAddr
    ) || op.can_trap()
        || op.can_load()
        || op.can_store()
        || op.is_call()
        || op.is_branch()
        || op.other_side_effects()
    {
        return None;
    }
    Some(d)
}

/// `v` feeds a call argument or a `return` operand — a fixed-register
/// use. A bundle holding it is pinned, and an unconstrained arg's
/// merge into it is refused (`PLIRON_RA2_NOPINMERGE`), so sinking the
/// arg cannot remove its edge move.
fn pinned(func: &Function, uses: &crate::uses::Uses, v: Value) -> bool {
    uses.sites(v).iter().any(|&(_, i)| {
        let op = func.dfg.insts[i].opcode();
        op.is_call() || op == Opcode::Return || op == Opcode::ReturnCall || op == Opcode::ReturnCallIndirect
    })
}

/// The values `v`'s producer chain reads: its def's operands plus,
/// transitively, the operands of movable defs feeding them — what an
/// at-use rematerialization of `v` may read where `v` is needed.
/// Capped; a bigger chain just stops early.
fn chain_reads(func: &Function, v: Value) -> rustc_hash::FxHashSet<Value> {
    let mut reads = rustc_hash::FxHashSet::default();
    let mut stack = vec![v];
    while let Some(x) = stack.pop() {
        let Some(d) = movable(func, x) else {
            continue;
        };
        for &o in func.dfg.inst_args(d) {
            let o = func.dfg.resolve_aliases(o);
            if reads.insert(o) && reads.len() < 32 {
                stack.push(o);
            }
        }
    }
    reads
}

/// The register-live operands `d`'s tail materialization reads:
/// direct operands that cannot move, plus the non-movable leaves of
/// the chains that can — a movable operand relocates its def into the
/// tail and reads ITS operands there instead.
fn tail_leaves(func: &Function, d: Inst) -> rustc_hash::FxHashSet<Value> {
    let mut leaves = rustc_hash::FxHashSet::default();
    let mut stack: Vec<Value> = func.dfg.inst_args(d).to_vec();
    while let Some(o) = stack.pop() {
        let o = func.dfg.resolve_aliases(o);
        match movable(func, o) {
            Some(d2) => stack.extend_from_slice(func.dfg.inst_args(d2)),
            None => {
                leaves.insert(o);
            }
        }
    }
    leaves
}

/// `hp` has a use at-or-after `d` inside `d`'s live range — the
/// overlap the sink removes. Uses dominated by `d`'s block (the arg's
/// range covers them all) plus uses in `d`'s own block after `d`; the
/// operand use at `d` itself abuts and does not count.
fn conflicts(func: &Function, dt: &cranelift_codegen::dominator_tree::DominatorTree, d: Inst, hp: Value, uses: &crate::uses::Uses) -> bool {
    let Some(db) = func.layout.inst_block(d) else {
        return false;
    };
    uses.sites(hp).iter().any(|&(b, i)| {
        if i == d {
            false
        } else if b != db {
            dt.block_dominates(db, b)
        } else {
            // Same block as `d`: a use after `d`'s position.
            let mut seen_d = false;
            for x in func.layout.block_insts(db) {
                if x == d {
                    seen_d = true;
                } else if x == i {
                    return seen_d;
                }
            }
            false
        }
    })
}

/// Bounded reachability: a use of `hp` reachable from `from` without
/// passing `h` — `latchspec`'s `live_past` rule. `true` past the
/// budget.
fn live_past(
    cfg: &cranelift_codegen::flowgraph::ControlFlowGraph,
    from: Block,
    h: Block,
    uses: &rustc_hash::FxHashSet<Block>,
) -> bool {
    let mut seen: rustc_hash::FxHashSet<Block> = Default::default();
    let mut stack = vec![from];
    let mut visited = 0usize;
    while let Some(b) = stack.pop() {
        if b == h || !seen.insert(b) {
            continue;
        }
        visited += 1;
        if visited > 256 || uses.contains(&b) {
            return true;
        }
        stack.extend(cfg.succ_iter(b));
    }
    false
}

/// The values the slots' bundles can already hold: resolved args
/// passed for each slot on `dst`'s other incoming calls (the call at
/// `term`/`side` excluded).
fn incoming_of(
    func: &Function,
    cfg: &cranelift_codegen::flowgraph::ControlFlowGraph,
    pred: Block,
    side: usize,
    dst: Block,
) -> Vec<Vec<Value>> {
    let mut incoming: Vec<Vec<Value>> = Vec::new();
    for p2 in cfg.pred_iter(dst) {
        let p2 = p2.block;
        let Some(t2) = func.layout.last_inst(p2) else {
            continue;
        };
        for (ix, c2) in func.dfg.insts[t2]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if c2.block(&func.dfg.value_lists) != dst || (p2 == pred && ix == side) {
                continue;
            }
            incoming.push(
                c2.args(&func.dfg.value_lists)
                    .filter_map(|a| a.as_value().map(|v| func.dfg.resolve_aliases(v)))
                    .collect(),
            );
        }
    }
    incoming
}

/// `d` cloned with fresh results; the new result.
fn clone_def(func: &mut Function, d: Inst) -> (Inst, Value) {
    let data = func.dfg.insts[d];
    let ty = func.dfg.ctrl_typevar(d);
    let ni = func.dfg.make_inst(data);
    func.set_srcloc(ni, func.srcloc(d));
    func.dfg.make_inst_results(ni, ty);
    (ni, func.dfg.first_result(ni))
}

/// Rewire `slots` of `term`'s branch call `side` to `nv`.
fn rewire(func: &mut Function, term: Inst, side: usize, slots: &[usize], nv: Value) {
    let dfg = &mut func.dfg;
    let calls = dfg.insts[term].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
    let mut ix = 0;
    calls[side].update_args(&mut dfg.value_lists, |arg| {
        let hit = slots.contains(&ix);
        ix += 1;
        if hit { nv.into() } else { arg }
    });
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let uses = an.uses(func);
    let CfgDtLa { cfg, dt, .. } = an.cfg_dt_la(func);
    // Back-edge arg uses: resolved arg value -> the slots carrying it.
    // A value's `uses` count covers each arg occurrence; a candidate is
    // coverable only when its count equals the back-edge slots found.
    let mut slots: FxHashMap<Value, Vec<Site>> = FxHashMap::default();
    for pred in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(pred) else {
            continue;
        };
        if !matches!(func.dfg.insts[term].opcode(), Opcode::Jump | Opcode::Brif) {
            continue;
        }
        let calls = func
            .dfg
            .insts[term]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        for (side, call) in calls.iter().enumerate() {
            let dst = call.block(&func.dfg.value_lists);
            // `dst` dominating `pred` makes the edge a back edge.
            if !dt.block_dominates(dst, pred) {
                continue;
            }
            for (slot, a) in call.args(&func.dfg.value_lists).enumerate() {
                let Some(v) = a.as_value() else { continue };
                let a = func.dfg.resolve_aliases(v);
                if a != v {
                    continue;
                }
                slots.entry(a).or_default().push(Site {
                    pred,
                    term,
                    side,
                    slot,
                    dst,
                });
            }
        }
    }
    if slots.is_empty() {
        return 0;
    }
    // Coverable args: def `d` movable and every use a back-edge slot.
    let mut covered: FxHashMap<Value, Inst> = FxHashMap::default();
    for (a, sites) in &slots {
        let Some(d) = movable(func, *a) else {
            continue;
        };
        if sites.len() > MAX_SITES || uses.sites(*a).len() != sites.len() {
            continue;
        }
        covered.insert(*a, d);
    }
    if covered.is_empty() {
        return 0;
    }
    // Group covered slots by the call-side carrying them: one edge tail
    // per call holds a copy of each arg's def, in def order. Each arg
    // gets one clone per call serving all its slots there.
    type CallKey = (Block, Inst, usize, Block);
    let mut by_call: FxHashMap<CallKey, Vec<(Value, Inst, Vec<usize>)>> = FxHashMap::default();
    let mut left: FxHashMap<Value, usize> = FxHashMap::default();
    for (a, d) in &covered {
        let mut groups = 0;
        for s in &slots[a] {
            let e = by_call
                .entry((s.pred, s.term, s.side, s.dst))
                .or_default();
            match e.iter_mut().find(|(x, _, _)| x == a) {
                Some((_, _, ss)) => ss.push(s.slot),
                None => {
                    e.push((*a, *d, vec![s.slot]));
                    groups += 1;
                }
            }
        }
        left.insert(*a, groups);
    }
    // `PLIRON_ARGTAIL_BRIF=0`: only `jump` predecessors — the copy
    // replaces the def 1:1 in the same block (never a new `b`), so a
    // sink cannot add instructions even when the merge still fails.
    let brif_ok = !pliron_cfg::env_is("PLIRON_ARGTAIL_BRIF", "0");
    // `PLIRON_ARGTAIL_PIN=1`: also refuse when a bundle member pins a
    // fixed register — broader than the allocator's own pin check.
    let pin_ok = pliron_cfg::env_is("PLIRON_ARGTAIL_PIN", "1");
    let mut calls: Vec<(CallKey, Vec<(Value, Inst, Vec<usize>)>)> = by_call.into_iter().collect();
    // Deterministic order, and def order inside one edge tail keeps a
    // cloned chain def-before-use.
    calls.sort_unstable();
    for (_, args) in &mut calls {
        args.sort_by_key(|&(_, d, _)| d);
    }
    // Blocks each value is used in — the `live_past` reachability
    // oracle for `brif` edges.
    let use_blocks = |uses: &crate::uses::Uses, hp: Value| -> rustc_hash::FxHashSet<Block> {
        uses.sites(hp).iter().map(|&(b, _)| b).collect()
    };
    // Phase 1: per call-group, per arg — is the sink needed, and can it
    // happen? An arg sinks only when it can sink on EVERY call-group
    // carrying it: a leftover shared def plus per-edge copies only adds
    // instructions.
    let mut ok: FxHashMap<Value, bool> = FxHashMap::default();
    for (a, _) in &covered {
        ok.insert(*a, true);
    }
    for ((pred, term, side, dst), args) in &calls {
        let (pred, term, side, dst) = (*pred, *term, *side, *dst);
        let sinkable = func.layout.inst_block(term) == Some(pred)
            && (func.dfg.insts[term].opcode() != Opcode::Brif || brif_ok);
        if !sinkable {
            for (a, _, _) in args {
                ok.insert(*a, false);
            }
            continue;
        }
        let is_brif = func.dfg.insts[term].opcode() == Opcode::Brif;
        // Resolved arg values on this call, and on a `brif` the other
        // call's block and args.
        let (this_args, other) = {
            let calls = func.dfg.insts[term].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            );
            let args_of = |c: &BlockCall| -> Vec<Value> {
                c.args(&func.dfg.value_lists)
                    .filter_map(|a| a.as_value().map(|v| func.dfg.resolve_aliases(v)))
                    .collect()
            };
            let t = args_of(&calls[side]);
            let o = if is_brif {
                let oc = &calls[1 - side];
                Some((oc.block(&func.dfg.value_lists), args_of(oc)))
            } else {
                None
            };
            (t, o)
        };
        // The values the slot's bundle can already hold: args passed
        // for the same slot on `dst`'s other incoming calls. A member
        // whose range overlaps the arg's (a `conflicts` hit) means a
        // refused merge the sink can remove — provided the member's
        // range does not also reach this edge's tail.
        let incoming = incoming_of(func, cfg, pred, side, dst);
        // Sink an arg only when it pays: a slot's target param or a
        // fellow bundle member must be live inside the arg's range
        // (otherwise the merge already succeeds and the copy is pure
        // waste — a `brif` edge block still costs its `b`), the member
        // must not ride this call itself nor pin the bundle (then it is
        // live to the edge wherever the def sits, or the merge is
        // refused regardless).
        'arg: for (a, d, ss) in args {
            for &s in ss {
                let Some(&hp) = func.dfg.block_params(dst).get(s) else {
                    continue;
                };
                let member_at_tail = |v: Value| v != *a && this_args.contains(&v);
                if member_at_tail(hp)
                    || (pin_ok && pinned(func, &uses, hp))
                    || incoming.iter().any(|ia| {
                        ia.get(s).is_some_and(|&v2| {
                            v2 != *a
                                && (member_at_tail(v2) || (pin_ok && pinned(func, &uses, v2)))
                        })
                    })
                {
                    ok.insert(*a, false);
                    continue 'arg;
                }
            }
            let need = ss.iter().any(|&s| {
                let Some(&hp) = func.dfg.block_params(dst).get(s) else {
                    return false;
                };
                let live_tail = |hp: Value| {
                    other.as_ref().is_some_and(|(ob, oa)| {
                        oa.contains(&hp) || live_past(cfg, *ob, dst, &use_blocks(&uses, hp))
                    })
                };
                conflicts(func, dt, *d, hp, &uses)
                    || live_tail(hp)
                    || incoming.iter().any(|ia| {
                        ia.get(s).is_some_and(|&v2| {
                            v2 != *a && (conflicts(func, dt, *d, v2, &uses) || live_tail(v2))
                        })
                    })
            });
            if !need {
                ok.insert(*a, false);
            }
        }
    }
    // Phase 2: per call-group, keep only the clones that can merge —
    // and an arg must clone on EVERY group carrying it: a leftover
    // shared def plus per-edge copies only adds instructions (the
    // copy then swaps the move 1:1 at best), so a group refusing an
    // arg forfeits it everywhere.
    let mut plans: Vec<(CallKey, Vec<(Value, Inst, Vec<usize>)>)> = Vec::with_capacity(calls.len());
    let mut survived: FxHashMap<Value, usize> = FxHashMap::default();
    for ((pred, term, side, dst), args) in calls {
        let mut plan: Vec<(Value, Inst, Vec<usize>)> = Vec::new();
        for (a, d, ss) in args {
            if ok[&a] {
                plan.push((a, d, ss));
            }
        }
        if plan.is_empty() {
            plans.push(((pred, term, side, dst), plan));
            continue;
        }
        // Clone `c` merges into its slot's bundle only when no member
        // of that bundle is read in the tail AFTER `c`'s def. `c`'s
        // own operand read abuts, but every other value that must
        // materialize at this call's tail — a co-tail clone or a
        // rematerialized arg — reads wherever the elaborator or the
        // allocator puts it, which is not ours to control. So `c`
        // forfeits whenever another call arg's producer chain (what
        // its tail materialization can read) touches a member.
        let incoming = incoming_of(func, cfg, pred, side, dst);
        let members: Vec<rustc_hash::FxHashSet<Value>> = plan
            .iter()
            .map(|(_, _, ss)| {
                ss.iter()
                    .flat_map(|&s| {
                        func.dfg
                            .block_params(dst)
                            .get(s)
                            .copied()
                            .into_iter()
                            .chain(incoming.iter().filter_map(move |ia| ia.get(s)).copied())
                    })
                    .map(|v| func.dfg.resolve_aliases(v))
                    .collect()
            })
            .collect();
        let this_args: Vec<Value> = func.dfg.insts[term]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .get(side)
            .map(|c| {
                c.args(&func.dfg.value_lists)
                    .filter_map(|a| a.as_value().map(|v| func.dfg.resolve_aliases(v)))
                    .collect()
            })
            .unwrap_or_default();
        let arg_reads: Vec<rustc_hash::FxHashSet<Value>> =
            this_args.iter().map(|&u| chain_reads(func, u)).collect();
        let planned: rustc_hash::FxHashMap<Value, usize> =
            plan.iter().enumerate().map(|(i, (a, _, _))| (*a, i)).collect();
        // And each leaf the tail materialization reads must already be
        // live there — used at `term` (an arg, the `brif` cond, the
        // other arm) or reachable from `dst` — or be a constant that
        // folds into the clone's encoding. A dead leaf extends a live
        // range to the tail and buys a reload the removed move did not
        // pay.
        let entry = func.layout.entry_block().unwrap_or(dst);
        let leaf_free = |o: Value| -> bool {
            uses.sites(o).iter().any(|&(_, i)| i == term)
                || live_past(cfg, dst, entry, &use_blocks(&uses, o))
                || func.dfg.value_def(o).inst().is_some_and(|d| {
                    matches!(
                        func.dfg.insts[d].opcode(),
                        Opcode::Iconst
                            | Opcode::F16const
                            | Opcode::F32const
                            | Opcode::F64const
                            | Opcode::Vconst
                    )
                })
        };
        let n = plan.len();
        let mut keep = vec![true; n];
        for c in 0..n {
            if tail_leaves(func, plan[c].1).iter().any(|&o| !leaf_free(o)) {
                keep[c] = false;
            }
        }
        for c in 0..n {
            for (&u, reads) in this_args.iter().zip(&arg_reads) {
                if u == plan[c].0 {
                    continue;
                }
                let hits = if let Some(&x) = planned.get(&u) {
                    // `u` sinks too: its clone reads the def's operands
                    // right in the tail — a certain member read.
                    func.dfg
                        .inst_args(plan[x].1)
                        .iter()
                        .any(|&o| members[c].contains(&func.dfg.resolve_aliases(o)))
                } else {
                    // Not sunk — a threat only when the call is its
                    // whole live range: then its value materializes
                    // fresh in the tail (a sunk def or a remat) and
                    // reads the chain there. Uses elsewhere keep it in
                    // a register, and the tail materialization is a
                    // copy that reads no member.
                    uses.sites(u).iter().all(|&(_, i)| i == term)
                        && reads.iter().any(|o| members[c].contains(o))
                };
                if hits {
                    keep[c] = false;
                    break;
                }
            }
        }
        let plan: Vec<(Value, Inst, Vec<usize>)> = plan
            .into_iter()
            .zip(keep)
            .filter_map(|(e, k)| k.then_some(e))
            .collect();
        for (a, _, _) in &plan {
            *survived.entry(*a).or_default() += 1;
        }
        plans.push(((pred, term, side, dst), plan));
    }
    // Apply: only an arg that survived on every group carrying it
    // (`survived == left`) fully replaces its def.
    let mut changed = 0;
    for ((pred, term, side, dst), plan) in plans {
        let plan: Vec<(Value, Inst, Vec<usize>)> = plan
            .into_iter()
            .filter(|(a, _, _)| survived.get(a).copied().unwrap_or(0) == left[a])
            .collect();
        if plan.is_empty() {
            continue;
        }
        match func.dfg.insts[term].opcode() {
            // A `jump` predecessor: the copies go at its bottom, in
            // member-use order.
            Opcode::Jump => {
                for (a, d, ss) in plan {
                    let (ni, nv) = clone_def(func, d);
                    func.layout.insert_inst(ni, term);
                    rewire(func, term, side, &ss, nv);
                    *left.get_mut(&a).unwrap() -= 1;
                    changed += 1;
                }
            }
            // A `brif` edge: split it with a block holding the copies —
            // the block the lowering already pays for the edge's moves.
            Opcode::Brif => {
                let mid = func.dfg.make_block();
                func.layout.insert_block_after(mid, pred);
                let mut news: FxHashMap<Value, Value> = FxHashMap::default();
                for (a, d, _) in &plan {
                    let (ni, nv) = clone_def(func, *d);
                    func.layout.append_inst(ni, mid);
                    news.insert(*a, nv);
                    *left.get_mut(a).unwrap() -= 1;
                }
                let nargs: Vec<BlockArg> = {
                    let calls = func.dfg.insts[term].branch_destination(
                        &func.dfg.jump_tables,
                        &func.dfg.exception_tables,
                    );
                    calls[side]
                        .args(&func.dfg.value_lists)
                        .map(|arg| {
                            arg.as_value()
                                .and_then(|v| news.get(&func.dfg.resolve_aliases(v)).copied())
                                .map_or(arg, |v| v.into())
                        })
                        .collect()
                };
                FuncCursor::new(func).at_bottom(mid).ins().jump(dst, &nargs);
                let dfg = &mut func.dfg;
                dfg.insts[term].branch_destination_mut(
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                )[side] = BlockCall::new(mid, [], &mut dfg.value_lists);
                changed += 1;
            }
            _ => {}
        }
    }
    for (a, n) in &left {
        if *n == 0 {
            let d = covered[a];
            func.layout.remove_inst(d);
            func.dfg.clear_results(d);
        }
    }
    if changed > 0 {
        an.invalidate(func);
    }
    changed
}

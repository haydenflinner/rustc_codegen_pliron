//! Edge specialization for store-covered continuation loads.
//!
//! `HashMap::entry(k).or_insert(0)` returns `&mut v` through a block
//! param: the fresh-insert edge stores `0` and `jump`s into the shared
//! `*v += 1` continuation. loadfwd can't forward through the param
//! merge — the other edges' loads genuinely differ — so the pair
//! survives as `movl $0,(p)` + `addl $1,(p)` while LLVM emits a single
//! `movl $1` on the fresh edge. When a `jump` edge binds a param such
//! that the continuation's load lands on a location the pred just
//! stored, clone the (small) continuation into the pred and alias the
//! cloned load's result to the stored value; lcse/dse finish the
//! `store 0 → store 1` fold.
//!
//! Only unconditional `jump` edges fire — the fold needs the edge's
//! arg bindings with no in-block condition to preserve — and the clone
//! only happens when a forward was actually found.
//!
//! `PLIRON_MEMSPEC=0` disables it.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, FuncRef, Function, Inst, InstructionData, Opcode, Value,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Root};
use crate::taildup::outer_dominate;

/// Largest continuation worth duplicating onto an edge.
const MAX_INSTS: usize = 12;
/// Backward-scan bound inside the predecessor.
const MAX_SCAN: usize = 128;

/// `(root, offset, width)` of a plain load/store inst.
fn mem_loc(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    i: Inst,
) -> Option<(Root, i64, i64)> {
    let (addr, off, w) = match func.dfg.insts[i] {
        InstructionData::Load {
            opcode,
            arg,
            offset,
            ..
        } => {
            let w = match opcode {
                Opcode::Load => i64::from(func.dfg.value_type(func.dfg.first_result(i)).bytes()),
                Opcode::Uload8 | Opcode::Sload8 => 1,
                Opcode::Uload16 | Opcode::Sload16 => 2,
                Opcode::Uload32 | Opcode::Sload32 => 4,
                _ => return None,
            };
            (arg, i32::from(offset), w)
        }
        InstructionData::Store {
            opcode,
            args,
            offset,
            ..
        } => {
            let w = match opcode {
                Opcode::Store => i64::from(func.dfg.value_type(args[0]).bytes()),
                Opcode::Istore8 => 1,
                Opcode::Istore16 => 2,
                Opcode::Istore32 => 4,
                _ => return None,
            };
            (args[1], i32::from(offset), w)
        }
        _ => return None,
    };
    // Loc keys must sit on the normalized root: a `noalias` scope name
    // peels to its slot so a scope-keyed read still meets slot-keyed
    // writes (the `dse::mem_loc` invariant — a raw root lets a live
    // store/read pair key differently).
    let (r, o) = loadfwd::norm_root(func, ps, na, addr);
    Some((r, o.wrapping_add(i64::from(off)), w))
}

/// The value stored by `p`'s last write covering `(r, o, w)`, or `None`
/// when no write covers it or anything between that write and `p`'s end
/// may clobber it. Same alias rule as `dse`: different roots may alias
/// unless either is isolated, and opaque writers can't reach isolated
/// roots at all.
fn last_writer(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    iso: &FxHashSet<Root>,
    nw: &FxHashMap<FuncRef, bool>,
    eh: bool,
    p: Block,
    t: Inst,
    loc: (Root, i64, i64),
) -> Option<Value> {
    let (r, o, w) = loc;
    let loc_iso = iso.contains(&r);
    let insts: Vec<Inst> = func.layout.block_insts(p).take_while(|&i| i != t).collect();
    for i in insts.into_iter().rev().take(MAX_SCAN) {
        let d = func.dfg.insts[i];
        if let Some((sr, so, sw)) = mem_loc(func, ps, na, i) {
            if d.opcode().can_load() && !d.opcode().can_store() {
                continue;
            }
            if sr == r {
                if so <= o && o + w <= so + sw {
                    // The most recent covering write; forward its value
                    // only on an exact-width match.
                    return match d {
                        InstructionData::Store { args, .. } if sw == w => Some(args[0]),
                        _ => None,
                    };
                }
                if o < so + sw && so < o + w {
                    return None;
                }
                continue;
            }
            if !iso.contains(&sr) && !loc_iso {
                if pliron_cfg::tracing_on()
                    && tracing::enabled!(target: "pliron::memspec", tracing::Level::DEBUG)
                {
                    let norm = |r: &Root| match r {
                        Root::V(v) => loadfwd::concrete(func, ps, *v),
                        _ => (*r, 0),
                    };
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::memspec", "memspec:   bail {i} store ({sr:?},{so},{sw}) ~{:?} vs ({r:?},{o},{w}) ~{:?}",
                            norm(&sr).0,
                            norm(&r).0
                        )
                    };
                }
                return None;
            }
            continue;
        }
        let op = d.opcode();
        // Write-free calls can't clobber; everything else that may
        // write (calls, atomics, fences, other side effects) is a
        // barrier unless the loc's root is isolated.
        let write_free = op.is_call() && loadfwd::write_free(func, i, nw, eh);
        if !write_free
            && (op.is_call() || op.can_store() || op.other_side_effects() || op.is_terminator())
            && !loc_iso
        {
            return None;
        }
    }
    None
}

/// Cheap structural test for a `jump` edge worth a closer look: the
/// target is small and starts with a load whose address is (or peels to)
/// one of its params — no dataflow analyses needed yet.
fn edge_candidate(func: &Function, p: Block, t: Inst) -> Option<(Block, Vec<Inst>)> {
    let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else {
        return None;
    };
    let r = destination.block(&func.dfg.value_lists);
    if r == p {
        return None;
    }
    let body: Vec<Inst> = func.layout.block_insts(r).collect();
    if body.len() > MAX_INSTS
        || !body.iter().all(|&i| {
            let op = func.dfg.insts[i].opcode();
            !op.is_call() && !op.can_trap() && !op.other_side_effects()
        })
    {
        return None;
    }
    let prms = func.dfg.block_params(r);
    if prms.is_empty() {
        return None;
    }
    // A load through a param: arg peels through `iadd`-consts only —
    // `root` refinement happens in `run`.
    let param_load = body.iter().any(|&i| {
        let InstructionData::Load { arg, .. } = func.dfg.insts[i] else {
            return false;
        };
        let mut v = func.dfg.resolve_aliases(arg);
        while let Some(d) = func.dfg.value_def(v).inst() {
            match func.dfg.insts[d] {
                InstructionData::Binary {
                    opcode: Opcode::Iadd | Opcode::Isub,
                    args,
                } => {
                    v = func.dfg.resolve_aliases(args[0]);
                }
                _ => break,
            }
        }
        prms.contains(&v)
    });
    param_load.then_some((r, body))
}

pub fn run(
    func: &mut Function,
    noalias: &FxHashSet<Value>,
    nowrite: &FxHashMap<FuncRef, bool>,
    cfrozen: &FxHashMap<FuncRef, u64>,
) -> usize {
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::memspec", tracing::Level::DEBUG);
    // First pass: only structural checks — most functions have no
    // candidate edges and skip the CFG/domtree/param_src/isolated work.
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut cands = Vec::new();
    for &p in &blocks {
        let Some(t) = func.layout.last_inst(p) else {
            continue;
        };
        if let Some(c) = edge_candidate(func, p, t) {
            cands.push((p, t, c));
        }
    }
    if cands.is_empty() {
        return 0;
    }
    // Each accepted clone replaces `p`'s jump with `r`'s body — edges
    // out of `p` change, so cfg/domtree/param_src/escapes all go stale
    // and must be rebuilt before the next candidate is judged.
    let mut cfg = ControlFlowGraph::with_function(func);
    let mut domtree = DominatorTree::with_function(func, &cfg);
    let mut ps = loadfwd::param_src(func);
    let (mut iso, mut escapes, _) =
        loadfwd::isolated_escapes(func, noalias, &ps, cfrozen, &FxHashSet::default());
    let eh = crate::pass_enabled("PLIRON_NOWRITE_EH");
    // Roots that only escape on blocks that can't reach `p` are still
    // isolated *at* `p`: before the first escaping use can run (the cold
    // `reserve` call on hashmap's grow-exit), no memory cell holds the
    // address, so a pointer loaded from memory can't reach its region.
    // Same rule licm's `iso_l` applies per loop, scoped to the edge.
    let mut reachers: FxHashSet<Block> = FxHashSet::default();
    let mut uses = crate::uses::Uses::new(func);
    let mut n = 0;
    'blocks: for (p, t, (r, body)) in cands {
        let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else {
            continue;
        };
        // `r` dominating `p` makes it a loop header; its params/defs may
        // be used in `p` itself and the clone can't supply them.
        if !domtree.is_reachable(r) || domtree.block_dominates(r, p) {
            continue;
        }
        let prms = func.dfg.block_params(r);
        let args: Vec<BlockArg> = destination.args(&func.dfg.value_lists).collect();
        if args.len() != prms.len() || !outer_dominate(func, &domtree, r, p) {
            if dbg {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::memspec", "memspec: {p}->{r} skip args/dom (fn {})", func.name)
                };
            }
            continue;
        }
        // Results defined in `r` may only be used inside `r` (an edge
        // arg rides `r`'s terminator and remaps cleanly in the clone).
        // A bare use in a later block is dominated by `r` today, but
        // the clone gives `r`'s successors a new path that bypasses
        // `r` — the use would read a def that never ran.
        let defs: FxHashSet<Value> = body
            .iter()
            .flat_map(|&i| func.dfg.inst_results(i).iter().copied())
            .filter(|&v| func.dfg.resolve_aliases(v) == v)
            .collect();
        if !defs.is_empty() && defs.iter().any(|v| uses.escapes(*v, r)) {
            if dbg {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::memspec", "memspec: {p}->{r} skip escaping def (fn {})", func.name)
                };
            }
            continue;
        }
        // The clone gives every block `s` that `r`'s body targets a new
        // predecessor `p`. A strict dominator of `s` that doesn't
        // dominate `p` stops dominating `s` — and every use of its defs
        // downstream dangles: inside `s`'s region AND at joins `s`
        // reaches but doesn't dominate (hashmap's grow-exit re-entered
        // below the slotprom'd params). `idom(s)` dominating `p` keeps
        // the whole dominator chain above `s` intact on the new edge,
        // which is exactly what every use below relies on. `s == r` is
        // a self-loop: the `p → r` edge already existed.
        let succs_ok = body.iter().all(|&i| {
            func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .into_iter()
                .all(|bc| {
                    let s = bc.block(&func.dfg.value_lists);
                    s == r
                        || domtree
                            .idom(s)
                            .is_some_and(|id| domtree.block_dominates(id, p))
                })
        });
        if !succs_ok {
            continue;
        }
        // `iso` widened to roots whose escapes can't reach `p`.
        let mut iso_l = iso.clone();
        if !escapes.is_empty() {
            reachers.clear();
            let mut stack = vec![p];
            while let Some(b) = stack.pop() {
                for q in cfg.pred_iter(b) {
                    if reachers.insert(q.block) {
                        stack.push(q.block);
                    }
                }
            }
            reachers.insert(p);
            for (r2, bs) in &escapes {
                if bs.iter().all(|b| !reachers.contains(b)) {
                    iso_l.insert(*r2);
                }
            }
        }
        // Loads through `r`'s params whose edge arg roots at a location
        // `p` just stored — each becomes a `change_to_alias` candidate.
        let mut fwd: Vec<(Inst, Value)> = Vec::new();
        for &i in &body {
            let InstructionData::Load { .. } = func.dfg.insts[i] else {
                continue;
            };
            let Some((lr, lo, w)) = mem_loc(func, &ps, noalias, i) else {
                continue;
            };
            // A param-rooted load takes its location from this edge's
            // arg; a load `param_src` already resolved (single pred or
            // uniform args) is concrete and needs no binding.
            let loc = match lr {
                Root::V(lv) => {
                    let Some(pi) = prms.iter().position(|&pr| {
                        func.dfg.resolve_aliases(pr) == func.dfg.resolve_aliases(lv)
                    }) else {
                        continue;
                    };
                    let BlockArg::Value(a) = args[pi] else {
                        continue 'blocks;
                    };
                    let (ar, ao) = loadfwd::norm_root(func, &ps, noalias, a);
                    (ar, ao.wrapping_add(lo), w)
                }
                _ => (lr, lo, w),
            };
            let hit = last_writer(func, &ps, noalias, &iso_l, nowrite, eh, p, t, loc);
            if dbg {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::memspec", "memspec: {p}->{r} load {i} loc {loc:?} hit={} (fn {})",
                        hit.is_some(),
                        func.name
                    )
                };
            }
            if let Some(sv) = hit {
                let res = func.dfg.inst_results(i);
                if res.len() == 1 && func.dfg.value_type(res[0]) == func.dfg.value_type(sv) {
                    fwd.push((i, sv));
                }
            }
        }
        if fwd.is_empty() {
            continue;
        }
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::memspec", "memspec: clone {r} into {p} edge ({} fwd) (fn {})",
                    fwd.len(),
                    func.name
                )
            };
        }
        // Splice a copy of `r` into `p` in place of the jump (taildup's
        // mechanic): params map to edge args, results to fresh values.
        let mut map: FxHashMap<Value, Value> = FxHashMap::default();
        for (&prm, a) in prms.iter().zip(&args) {
            match a {
                BlockArg::Value(v) => {
                    map.insert(prm, *v);
                }
                _ => continue 'blocks,
            }
        }
        let fwd_of: FxHashMap<Inst, Value> = fwd.iter().copied().collect();
        let mut fwd_new: Vec<(Inst, Value)> = Vec::with_capacity(fwd.len());
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
            if let Some(&sv) = fwd_of.get(&i) {
                fwd_new.push((ni, sv));
            }
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
        // Bind the cloned loads to the stored values they provably read.
        for (ni, sv) in fwd_new {
            let res = func.dfg.first_result(ni);
            func.layout.remove_inst(ni);
            func.dfg.clear_results(ni);
            func.dfg.change_to_alias(res, sv);
        }
        n += 1;
        cfg = ControlFlowGraph::with_function(func);
        domtree = DominatorTree::with_function(func, &cfg);
        uses = crate::uses::Uses::new(func);
        ps = loadfwd::param_src(func);
        (iso, escapes, _) =
            loadfwd::isolated_escapes(func, noalias, &ps, cfrozen, &FxHashSet::default());
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::types;
    use cranelift_codegen::ir::{
        AbiParam, Function, InstBuilder, MemFlagsData, Signature, StackSlotData, StackSlotKind,
        UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
            panic!("memspec produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `*slot = 5; jump cont(&slot)` where `cont` does `*p += 1`: the
    /// continuation clones into the pred, the cloned load binds to `5`,
    /// and the `jump` disappears.
    #[test]
    fn forwards_covering_store_to_param_load() {
        let mut func =
            Function::with_name_signature(UserFuncName::user(0, 0), Signature::new(CallConv::Fast));
        let p = func.dfg.make_block();
        let r = func.dfg.make_block();
        func.layout.append_block(p);
        func.layout.append_block(r);
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let pp = func.dfg.append_block_param(r, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p);
        let addr = cur.ins().stack_addr(types::I64, ss, 0);
        let five = cur.ins().iconst(types::I64, 5);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), five, addr, 0);
        cur.ins().jump(r, &[addr.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(r);
        let l = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), pp, 0);
        let one = cur.ins().iconst(types::I64, 1);
        let inc = cur.ins().iadd(l, one);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), inc, pp, 0);
        cur.ins().return_(&[]);

        assert_eq!(
            run(
                &mut func,
                &FxHashSet::default(),
                &FxHashMap::default(),
                &FxHashMap::default(),
            ),
            1
        );
        // The jump is gone; `p` now ends with the cloned RMW and the
        // cloned load's result aliases the stored `5`.
        let term = func.layout.last_inst(p).unwrap();
        assert!(func.dfg.insts[term].opcode().is_return());
        let n_loads = func
            .layout
            .block_insts(p)
            .filter(|&i| matches!(func.dfg.insts[i], InstructionData::Load { .. }))
            .count();
        assert_eq!(n_loads, 0);
        verify(&func);
    }

    /// Cloning `r` into `p`'s edge makes `p` a new predecessor of every
    /// block `r` targets. A strict dominator of such a target that
    /// doesn't dominate `p` stops dominating it — and every use of its
    /// defs downstream dangles, including at joins the target reaches
    /// but doesn't dominate. `d` dominates join `j` only through its
    /// two brif destinations; the clone must be refused.
    #[test]
    fn rejects_dominator_bypassing_edge() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let e = func.dfg.make_block();
        let q = func.dfg.make_block();
        let p = func.dfg.make_block();
        let d = func.dfg.make_block();
        let s1 = func.dfg.make_block();
        let s2 = func.dfg.make_block();
        let j = func.dfg.make_block();
        for b in [e, q, p, d, s1, s2, j] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let flags = MemFlagsData::new().with_notrap();
        let arg0 = func.dfg.append_block_param(e, types::I64);
        let pd = func.dfg.append_block_param(d, types::I64);
        let sp1 = func.dfg.append_block_param(s1, types::I64);
        let sp2 = func.dfg.append_block_param(s2, types::I64);
        let pj = func.dfg.append_block_param(j, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(e);
        let z = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, arg0, z);
        cur.ins().brif(c, p, &[], q, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(q);
        let seven = cur.ins().iconst(types::I64, 7);
        cur.ins().jump(d, &[seven.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        let five = cur.ins().iconst(types::I64, 5);
        cur.ins().store(flags, five, a, 0);
        cur.ins().jump(d, &[a.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(d);
        let l = cur.ins().load(types::I64, flags, pd, 0);
        let one = cur.ins().iconst(types::I64, 1);
        let inc = cur.ins().iadd(l, one);
        cur.ins().store(flags, inc, pd, 0);
        let c2 = cur.ins().icmp(IntCC::Equal, inc, z);
        cur.ins().brif(c2, s1, &[pd.into()], s2, &[pd.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s1);
        cur.ins().jump(j, &[sp1.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s2);
        cur.ins().jump(j, &[sp2.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(j);
        // `pd` is `d`'s param: `j` is dominated by `d` through `s1`/`s2`
        // — a clone giving `s1`/`s2` a `p` edge breaks that dominance.
        let out = cur.ins().iadd(pj, pd);
        cur.ins().return_(&[out]);

        assert_eq!(
            run(
                &mut func,
                &FxHashSet::default(),
                &FxHashMap::default(),
                &FxHashMap::default(),
            ),
            0
        );
        verify(&func);
    }

    /// Same shape, but an intervening store to an unrelated,
    /// non-isolated root may alias the target — no transform.
    #[test]
    fn rejects_may_alias_intervening_store() {
        let mut func =
            Function::with_name_signature(UserFuncName::user(0, 0), Signature::new(CallConv::Fast));
        let p = func.dfg.make_block();
        let r = func.dfg.make_block();
        func.layout.append_block(p);
        func.layout.append_block(r);
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let pp = func.dfg.append_block_param(r, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p);
        let addr = cur.ins().stack_addr(types::I64, ss, 0);
        let five = cur.ins().iconst(types::I64, 5);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), five, addr, 0);
        // Opaque pointer (a function param would work too; a call result
        // is simplest): a store through it may alias the slot — but only
        // once the slot's address has escaped the tracked roots, since a
        // `jump` binding `addr` to a param keeps the scope tracked.
        let seven = cur.ins().iconst(types::I64, 7);
        let other = cur.ins().iadd_imm_u(five, 0x1000);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), addr, other, 0);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), seven, other, 0);
        cur.ins().jump(r, &[addr.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(r);
        let l = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), pp, 0);
        let one = cur.ins().iconst(types::I64, 1);
        let inc = cur.ins().iadd(l, one);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), inc, pp, 0);
        cur.ins().return_(&[]);

        // The `iadd_imm`'s root is the `iconst` — `V(iconst)` may alias
        // `S(ss)` (the slot's address can't be ruled out in memory), so
        // the pass must refuse.
        assert_eq!(
            run(
                &mut func,
                &FxHashSet::default(),
                &FxHashMap::default(),
                &FxHashMap::default(),
            ),
            0
        );
    }

    /// The continuation's load result is used bare in a successor
    /// block: cloning would give the successor a new path where the
    /// def never ran — the pass must refuse.
    #[test]
    fn rejects_result_escaping_to_successor() {
        let mut func =
            Function::with_name_signature(UserFuncName::user(0, 0), Signature::new(CallConv::Fast));
        let p = func.dfg.make_block();
        let r = func.dfg.make_block();
        let s = func.dfg.make_block();
        for b in [p, r, s] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let pp = func.dfg.append_block_param(r, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p);
        let addr = cur.ins().stack_addr(types::I64, ss, 0);
        let five = cur.ins().iconst(types::I64, 5);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), five, addr, 0);
        cur.ins().jump(r, &[addr.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(r);
        let l = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), pp, 0);
        cur.ins().jump(s, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let x = cur.ins().iadd(l, l);
        cur.ins().return_(&[x]);

        assert_eq!(
            run(
                &mut func,
                &FxHashSet::default(),
                &FxHashMap::default(),
                &FxHashMap::default(),
            ),
            0
        );
    }
}

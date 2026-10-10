//! Late forwarder-block bypass (`PLIRON_EDGEFWD`).
//!
//! jumpthread's `bypass_forwarders` retargets edges through blocks that only
//! `jump` onward, but it only accepts `jump`/`brif`/`br_table` predecessors.
//! Landing-pad chains don't qualify: MIR unwind edges become
//! `try_call f, ret_blk, [tag0: pad_blk]` whose pad block is often just
//! `jump shared_cleanup`, and the *return* edge can likewise point at a
//! `jump`-only block. Each surviving forwarder costs one emitted `b` (regex-
//! syntax: ~400 trampolines in `visit_post` alone; ~3.9k crate-wide).
//!
//! This pass generalises the same rewrite to every predecessor terminator,
//! including `try_call`/`try_call_indirect`. Edge args that are not plain
//! `Value`s (`TryCallRet`/`TryCallExn` pseudo-values) are forwarded verbatim
//! onto the same edge of the same `try_call`, where they remain valid —
//! they describe values materialised on that edge, independent of the
//! destination block. After retargeting, blocks left without predecessors
//! are dropped via `remove_unreachable_blocks`.
//!
//! Written by the x64 agent as a separated function per coordination policy;
//! intentionally self-contained (no jumpthread internals reused beyond the
//! shared `pure_op` predicate and `remove_unreachable_blocks`).

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, TrapCode, Value,
    ValueDef,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::jumpthread::{pure_op, remove_unreachable_blocks};

/// `call f; jump trap_blk` -> `call f; trap`. MIR `-> !` calls (panics,
/// bounds-check failures) are followed by `unreachable`, which lowering
/// emits as `jump` to a shared one-inst trap block — hundreds of
/// `bl f; b shared_udf` pairs per noreturn-heavy function. Copying the
/// trap into the predecessor removes the `b` entirely (the trap block
/// stays for its `brif`-edge predecessors).
fn inline_trap_tails(func: &mut Function) -> usize {
    // Blocks that are exactly one unconditional `trap`.
    let mut traps: FxHashMap<Block, TrapCode> = FxHashMap::default();
    for b in func.layout.blocks() {
        let mut it = func.layout.block_insts(b);
        let (Some(t0), None) = (it.next(), it.next()) else {
            continue;
        };
        if func.dfg.insts[t0].opcode() == Opcode::Trap
            && let Some(code) = func.dfg.insts[t0].trap_code()
        {
            traps.insert(b, code);
        }
    }
    if traps.is_empty() {
        return 0;
    }
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(lt) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[lt] else {
            continue;
        };
        let t = destination.block(&func.dfg.value_lists);
        let Some(&code) = traps.get(&t) else {
            continue;
        };
        let data = InstructionData::Trap {
            opcode: Opcode::Trap,
            code,
        };
        let ni = func.dfg.make_inst(data);
        func.layout.remove_inst(lt);
        func.layout.append_inst(ni, b);
        n += 1;
    }
    n
}

/// One retargeting round: returns edges retargeted. Mirrors
/// `bypass_forwarders`' criteria for what counts as a forwarder — an empty
/// or pure, locally-consumed body ending in `jump` — but accepts any
/// predecessor terminator.
fn bypass_round(func: &mut Function, cfg: &ControlFlowGraph) -> usize {
    let domtree = DominatorTree::with_function(func, cfg);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();

    // Block params used outside their own block: bypassing that block would
    // leave those uses undominated.
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for &x in &blocks {
        for i in func.layout.block_insts(x) {
            let mut note = |v: Value| {
                let v = func.dfg.resolve_aliases(v);
                *uses.entry(v).or_default() += 1;
                if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                    && d != x
                {
                    escaping.insert(v);
                }
            };
            func.dfg.inst_args(i).iter().for_each(|&v| note(v));
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        note(v);
                    }
                }
            }
        }
    }

    let mut n = 0;
    for b in blocks {
        if Some(b) == entry
            || func
                .dfg
                .block_params(b)
                .iter()
                .any(|p| escaping.contains(p))
        {
            continue;
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        // Any other insts must be pure and used only by each other.
        let body: Vec<Inst> = func.layout.block_insts(b).filter(|&i| i != term).collect();
        let mut local: FxHashMap<Value, u32> = FxHashMap::default();
        for &i in &body {
            for &a in func.dfg.inst_args(i) {
                *local.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
        }
        if !body.iter().all(|&i| {
            pure_op(func, i)
                && func.dfg.inst_results(i).iter().all(|&r| {
                    let r = func.dfg.resolve_aliases(r);
                    uses.get(&r).copied().unwrap_or(0)
                        == local.get(&r).copied().unwrap_or(0)
                })
        }) {
            continue;
        }
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let t = destination.block(&func.dfg.value_lists);
        if t == b {
            continue;
        }
        // The forwarder's jump args must all be plain Values (a `jump`
        // can never carry edge pseudo-values, but guard anyway).
        let mut targs = Vec::new();
        let mut all_values = true;
        for a in destination.args(&func.dfg.value_lists) {
            match a {
                BlockArg::Value(v) => targs.push(func.dfg.resolve_aliases(v)),
                _ => {
                    all_values = false;
                    break;
                }
            }
        }
        if !all_values || targs.len() != func.dfg.num_block_params(t) {
            continue;
        }
        let params = func.dfg.block_params(b).to_vec();
        let preds: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        for (pb, pinst) in preds {
            if pb == b {
                continue;
            }
            // Unlike bypass_forwarders there is no opcode whitelist: any
            // terminator edge into b is a retarget candidate. For
            // `try_call`, non-`Value` edge args (ret/exn markers) are
            // forwarded verbatim — they stay on the same edge of the same
            // instruction, so their validity is preserved.
            let dests: Vec<BlockCall> = func.dfg.insts[pinst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            for (di, bc) in dests.into_iter().enumerate() {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let pargs: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if pargs.len() != params.len() {
                    continue;
                }
                let mut new = Vec::with_capacity(targs.len());
                let mut ok = true;
                for &v in &targs {
                    if let Some(k) = params.iter().position(|&p| p == v) {
                        // Verbatim: may be a `Value` or a try_call edge
                        // pseudo-value; either is valid on this edge.
                        new.push(pargs[k]);
                        continue;
                    }
                    let dom = match func.dfg.value_def(v) {
                        ValueDef::Result(i, _) => {
                            i != pinst && domtree.dominates(i, pinst, &func.layout)
                        }
                        ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
                        _ => false,
                    };
                    if !dom {
                        ok = false;
                        break;
                    }
                    new.push(BlockArg::Value(v));
                }
                if !ok || new.len() != targs.len() {
                    continue;
                }
                let nbc = BlockCall::new(t, new, &mut func.dfg.value_lists);
                let dfg = &mut func.dfg;
                dfg.insts[pinst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
                    nbc;
                n += 1;
            }
        }
    }
    n
}

/// Provably-equal edge-arg values: the same SSA value, or two `iconst`s of
/// the same type and immediate. Used to substitute a forwarder's
/// rematerialised constant with one already available on the pred edge
/// instead of cloning it.
fn same_value(func: &Function, a: Value, b: Value) -> bool {
    let a = func.dfg.resolve_aliases(a);
    let b = func.dfg.resolve_aliases(b);
    if a == b || func.dfg.value_type(a) != func.dfg.value_type(b) {
        return a == b;
    }
    let iconst = |v: Value| -> Option<i64> {
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
    };
    iconst(a).is_some() && iconst(a) == iconst(b)
}

/// `iconst; jump T(iconst, ...)`: a forwarder that materialises values only
/// for its own jump args can't be bypassed (the args don't dominate the
/// incoming edges), but the body is pure — clone it into each predecessor
/// and retarget. This is the residual shape left after `bypass_round`
/// (shared `-1`/`8` constants fed to merged error-path blocks).
fn remat_round(func: &mut Function, cfg: &ControlFlowGraph) -> usize {
    const MAX_BODY: usize = 4;
    let domtree = DominatorTree::with_function(func, cfg);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();

    // Same escaping-param and use accounting as bypass_round.
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for &x in &blocks {
        for i in func.layout.block_insts(x) {
            let mut note = |v: Value| {
                let v = func.dfg.resolve_aliases(v);
                *uses.entry(v).or_default() += 1;
                if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                    && d != x
                {
                    escaping.insert(v);
                }
            };
            func.dfg.inst_args(i).iter().for_each(|&v| note(v));
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        note(v);
                    }
                }
            }
        }
    }

    let mut n = 0;
    for b in blocks {
        if Some(b) == entry
            || func
                .dfg
                .block_params(b)
                .iter()
                .any(|p| escaping.contains(p))
        {
            continue;
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let body: Vec<Inst> = func.layout.block_insts(b).filter(|&i| i != term).collect();
        if body.is_empty() || body.len() > MAX_BODY || !body.iter().all(|&i| pure_op(func, i)) {
            continue;
        }
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let t = destination.block(&func.dfg.value_lists);
        if t == b {
            continue;
        }
        let mut targs = Vec::new();
        let mut all_values = true;
        for a in destination.args(&func.dfg.value_lists) {
            match a {
                BlockArg::Value(v) => targs.push(func.dfg.resolve_aliases(v)),
                _ => {
                    all_values = false;
                    break;
                }
            }
        }
        if !all_values || targs.len() != func.dfg.num_block_params(t) {
            continue;
        }
        // Every use of a body result must be inside b itself (other body
        // insts or the jump's edge args); otherwise cloning leaves a
        // dangling use.
        let mut local: FxHashMap<Value, u32> = FxHashMap::default();
        for &i in &body {
            for &a in func.dfg.inst_args(i) {
                *local.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
        }
        for a in destination.args(&func.dfg.value_lists) {
            if let BlockArg::Value(v) = a {
                *local.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
        let body_ok = body.iter().all(|&i| {
            func.dfg.inst_results(i).iter().all(|&r| {
                let r = func.dfg.resolve_aliases(r);
                uses.get(&r).copied().unwrap_or(0) == local.get(&r).copied().unwrap_or(0)
            })
        });
        if !body_ok {
            continue;
        }
        let params = func.dfg.block_params(b).to_vec();
        let preds: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        for (pb, pinst) in preds {
            if pb == b {
                continue;
            }
            let dests: Vec<BlockCall> = func.dfg.insts[pinst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            for (di, bc) in dests.into_iter().enumerate() {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let pargs: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if pargs.len() != params.len() {
                    continue;
                }
                // Pre-validate: every body-inst arg must come from a
                // `Value` parg, an earlier body result, or dominate pinst.
                // Body-def results are filled in during the clone below.
                let body_defs: FxHashSet<Value> = body
                    .iter()
                    .flat_map(|&i| func.dfg.inst_results(i).iter().copied())
                    .map(|v| func.dfg.resolve_aliases(v))
                    .collect();
                let mut ok = true;
                for &i in &body {
                    for &a in func.dfg.inst_args(i) {
                        let a = func.dfg.resolve_aliases(a);
                        if body_defs.contains(&a) {
                            continue;
                        }
                        if let Some(k) = params.iter().position(|&p| p == a) {
                            if !matches!(pargs[k], BlockArg::Value(_)) {
                                ok = false;
                            }
                            continue;
                        }
                        let dom = match func.dfg.value_def(a) {
                            ValueDef::Result(i2, _) => {
                                i2 != pinst && domtree.dominates(i2, pinst, &func.layout)
                            }
                            ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
                            _ => false,
                        };
                        if !dom {
                            ok = false;
                        }
                    }
                }
                // And every targ must be a param (edge arg forwards
                // verbatim, markers included), a body result, or dominate.
                if ok {
                    for &v in &targs {
                        if params.contains(&v) || body_defs.contains(&v) {
                            continue;
                        }
                        let dom = match func.dfg.value_def(v) {
                            ValueDef::Result(i2, _) => {
                                i2 != pinst && domtree.dominates(i2, pinst, &func.layout)
                            }
                            ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
                            _ => false,
                        };
                        if !dom {
                            ok = false;
                        }
                    }
                }
                if !ok {
                    continue;
                }
                // Substitution before cloning: a body result feeding the
                // jump args is usually a rematerialised constant that an
                // already-valid edge arg also carries (`jump T(-1,...,-1)`)
                // — reuse that value instead of emitting a fresh iconst.
                let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
                for (k, &p) in params.iter().enumerate() {
                    if let BlockArg::Value(v) = pargs[k] {
                        vmap.insert(p, v);
                    }
                }
                // Candidate substitutes must themselves be valid edge args
                // at pinst: Value pargs, or targs that dominate pinst (not
                // b's params or body results).
                let mut cand: Vec<Value> = pargs
                    .iter()
                    .filter_map(|a| match a {
                        BlockArg::Value(v) => Some(*v),
                        _ => None,
                    })
                    .collect();
                for &v in &targs {
                    if !params.contains(&v) && !body_defs.contains(&v) {
                        cand.push(v);
                    }
                }
                // An `iconst` already materialised in pb (before pinst) is
                // also a valid substitute — it dominates the edge.
                for i in func.layout.block_insts(pb) {
                    if i == pinst {
                        break;
                    }
                    if let InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        ..
                    } = func.dfg.insts[i]
                    {
                        cand.extend(func.dfg.inst_results(i).iter().copied());
                    }
                }
                for &i in &body {
                    let subs: Vec<Option<Value>> = func
                        .dfg
                        .inst_results(i)
                        .iter()
                        .map(|&r| {
                            let r = func.dfg.resolve_aliases(r);
                            cand.iter().copied().find(|&c| same_value(func, r, c))
                        })
                        .collect();
                    for (&r, s) in func
                        .dfg
                        .inst_results(i)
                        .iter()
                        .zip(subs.iter())
                    {
                        if let &Some(s) = s {
                            vmap.insert(func.dfg.resolve_aliases(r), s);
                        }
                    }
                    if subs.iter().all(|s| s.is_some()) {
                        continue;
                    }
                    let data = func.dfg.insts[i];
                    let ctv = func.dfg.ctrl_typevar(i);
                    let ni = func.dfg.make_inst(data);
                    func.dfg.make_inst_results(ni, ctv);
                    func.layout.insert_inst(ni, pinst);
                    let nargs: Vec<Value> = func
                        .dfg
                        .inst_args(ni)
                        .iter()
                        .map(|&a| func.dfg.resolve_aliases(a))
                        .collect();
                    let mut new_args = Vec::with_capacity(nargs.len());
                    for a in nargs {
                        if let Some(&nv) = vmap.get(&a) {
                            new_args.push(nv);
                        } else if let Some(k) = params.iter().position(|&p| p == a) {
                            if let BlockArg::Value(v) = pargs[k] {
                                new_args.push(v);
                            } else {
                                new_args.push(a);
                            }
                        } else {
                            new_args.push(a);
                        }
                    }
                    func.dfg
                        .inst_args_mut(ni)
                        .iter_mut()
                        .zip(new_args)
                        .for_each(|(a, nv)| *a = nv);
                    for ((&o, s), &nr) in func
                        .dfg
                        .inst_results(i)
                        .iter()
                        .zip(subs.iter())
                        .zip(func.dfg.inst_results(ni).iter())
                    {
                        if s.is_none() {
                            vmap.insert(func.dfg.resolve_aliases(o), nr);
                        }
                    }
                }
                let mut new = Vec::with_capacity(targs.len());
                for &v in &targs {
                    if let Some(&nv) = vmap.get(&v) {
                        new.push(BlockArg::Value(nv));
                    } else if let Some(k) = params.iter().position(|&p| p == v) {
                        new.push(pargs[k]);
                    } else {
                        // Already validated to dominate pinst.
                        new.push(BlockArg::Value(v));
                    }
                }
                let nbc = BlockCall::new(t, new, &mut func.dfg.value_lists);
                let dfg = &mut func.dfg;
                dfg.insts[pinst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
                    nbc;
                n += 1;
            }
        }
    }
    n
}

/// Retarget edges past forwarder blocks, then delete the ones left without
/// predecessors. Runs to a fixpoint so forwarder chains collapse end to end.
/// Returns edges retargeted plus traps inlined.
pub fn run(func: &mut Function) -> usize {
    let mut total = inline_trap_tails(func);
    for _ in 0..16 {
        let cfg = ControlFlowGraph::with_function(func);
        let n = bypass_round(func, &cfg) + remat_round(func, &cfg);
        total += n;
        let removed = remove_unreachable_blocks(func);
        if n == 0 && removed == 0 {
            break;
        }
    }
    // Note: privatising `try_call` normal edges to shared trap blocks
    // (`try_call f, ret=shared_trap, [pad]` -> a per-site `trap` block
    // after the call) was tried and rejected: `Function::is_effectively_cold`
    // marks every trap-terminated block cold, so the private traps always
    // sink and the `callq; jmp ud2` stays — the transform only added ~1.2k
    // `ud2` instructions in regex-syntax for ~187 removed jumps.
    total
}

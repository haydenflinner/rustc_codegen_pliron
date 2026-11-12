//! Fold single-pred forwarding stubs into the predecessor edge
//! `PLIRON_STUBHOIST` (default-on, `=0` off). `b: <pure insts>; jump t(args)` with exactly
//! one incoming edge copies its pure body into the pred before the
//! terminator and retargets the edge straight at `t`; the stub then
//! has no preds and is swept. jumpthread's `bypass_forwarders` can
//! only retarget edges whose args already dominate the pred — a stub
//! whose jump args are computed *in the stub* (`uext n; jump
//! ret(v)`) is invisible to it, so LLVM's fib leaf shape (`jb`
//! straight into the shared exit) survives here as an out-of-line
//! block plus an extra `jmp` on every leaf visit.
//!
//! `brif`/`br_table` preds inside loops are skipped: the moved insts
//! would run on every iteration's loop-carried arm as dead work.
//! `jump` preds fold anywhere — the body executes once per pred
//! execution either way.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::jumpthread::{pure_op, remove_unreachable_blocks};

/// Pure insts moved per stub (LLVM's cheap-hoist bound; fib needs 1).
const MAX_BODY: usize = 4;
/// Chained stubs (`b1; jump b2`, `b2; jump t`) fold innermost-first;
/// the budget bounds the rescan, it is not a per-block cap.
const MAX_ROUNDS: usize = 4;

/// `v` reaches `pinst` in `pb` — same dominance test as
/// `bypass_forwarders`: results dominate the terminator, params
/// dominate the pred block.
fn reaches(func: &Function, dt: &DominatorTree, v: Value, pb: Block, pinst: Inst) -> bool {
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => i != pinst && dt.try_dominates(i, pinst, &func.layout),
        ValueDef::Param(d, _) => dt.block_dominates(d, pb),
        _ => false,
    }
}

/// One pass over the blocks; returns the number of edges retargeted.
fn round(func: &mut Function, la: &LoopAnalysis) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let entry = func.layout.entry_block();
    // Total use counts (inst args + branch args) for the
    // results-stay-local check, and params used outside their own
    // block — removing a stub whose params escape would orphan them.
    let mut uses: FxHashMap<Value, u32> = Default::default();
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let mut note = |v: Value| {
                let v = func.dfg.resolve_aliases(v);
                *uses.entry(v).or_default() += 1;
                if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                    && d != b
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
    for b in func.layout.blocks().collect::<Vec<_>>() {
        if Some(b) == entry || func.dfg.block_params(b).iter().any(|p| escaping.contains(p)) {
            continue;
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let t = destination.block(&func.dfg.value_lists);
        if t == b {
            continue;
        }
        let Some(targs) = destination
            .args(&func.dfg.value_lists)
            .map(|a| match a {
                BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if targs.len() != func.dfg.num_block_params(t) {
            continue;
        }
        let body: Vec<Inst> = func.layout.block_insts(b).filter(|&i| i != term).collect();
        // Iconst is excluded when the result is i128: clones made after
        // `iconst128` has run would survive to isel with no rule.
        if body.len() > MAX_BODY
            || !body.iter().all(|&i| {
                pure_op(func, i)
                    && !(func.dfg.insts[i].opcode() == Opcode::Iconst
                        && func.dfg.value_type(func.dfg.first_result(i)) == types::I128)
            })
        {
            continue;
        }
        // Every body result's uses must stay inside `b` (body args plus
        // the terminator's branch args) so copying the body out leaves
        // nothing dangling.
        let mut inb: FxHashMap<Value, u32> = Default::default();
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *inb.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
        if !body.iter().all(|&i| {
            let r = func.dfg.resolve_aliases(func.dfg.first_result(i));
            uses.get(&r).copied().unwrap_or(0) == inb.get(&r).copied().unwrap_or(0)
        }) {
            continue;
        }
        // Exactly one pred edge: a `brif` with both arms on `b` lists
        // once in `pred_iter` but has two destinations.
        let preds: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        let &[(pb, pinst)] = preds.as_slice() else {
            continue;
        };
        let pop = func.dfg.insts[pinst].opcode();
        if pb == b
            || !matches!(pop, Opcode::Jump | Opcode::Brif | Opcode::BrTable)
            || (pop != Opcode::Jump && la.innermost_loop(pb).is_some())
        {
            continue;
        }
        let dests = func
            .dfg
            .insts[pinst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        let di: Vec<usize> = (0..dests.len())
            .filter(|&d| dests[d].block(&func.dfg.value_lists) == b)
            .collect();
        let &[di] = di.as_slice() else {
            continue;
        };
        let pargs: Vec<BlockArg> = dests[di].args(&func.dfg.value_lists).collect();
        let params = func.dfg.block_params(b).to_vec();
        if pargs.len() != params.len() {
            continue;
        }
        // `b` param -> the edge arg that reaches it. Body operands and
        // targs resolve through this map; a param fed a non-Value edge
        // arg (try_call pseudo-values) may only pass through to `t`'s
        // args, never into a cloned inst.
        let pmap: FxHashMap<Value, BlockArg> = params
            .iter()
            .map(|&p| func.dfg.resolve_aliases(p))
            .zip(pargs.iter().copied())
            .collect();
        // Verify every body operand and targ reaches `pinst` (through
        // pmap or by dominance) before cloning anything.
        let mut ok = true;
        for &i in &body {
            for &a in func.dfg.inst_args(i) {
                let a = func.dfg.resolve_aliases(a);
                let def_in_b = matches!(func.dfg.value_def(a), ValueDef::Result(di, _) if func.layout.inst_block(di) == Some(b));
                if def_in_b {
                    continue;
                }
                ok &= match pmap.get(&a) {
                    Some(&BlockArg::Value(v)) => reaches(func, &dt, func.dfg.resolve_aliases(v), pb, pinst),
                    Some(_) => false,
                    None => reaches(func, &dt, a, pb, pinst),
                };
                if !ok {
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        if ok {
            for &v in &targs {
                let def_in_b = matches!(func.dfg.value_def(v), ValueDef::Result(di, _) if func.layout.inst_block(di) == Some(b));
                if def_in_b {
                    continue; // produced by the clone
                }
                ok &= match pmap.get(&v) {
                    Some(&BlockArg::Value(pv)) => reaches(func, &dt, func.dfg.resolve_aliases(pv), pb, pinst),
                    Some(&a) => {
                        let _ = a;
                        true // pseudo-values forward verbatim on the same edge
                    }
                    None => reaches(func, &dt, v, pb, pinst),
                };
                if !ok {
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        // Clone the body into `pb` right before `pinst`, mapping params
        // to edge args and earlier results to their clones.
        let mut vmap: FxHashMap<Value, Value> = Default::default();
        for (&p, &a) in params.iter().zip(&pargs) {
            if let BlockArg::Value(v) = a {
                vmap.insert(func.dfg.resolve_aliases(p), v);
            }
        }
        let alias: FxHashMap<Value, Value> = body
            .iter()
            .flat_map(|&i| func.dfg.inst_values(i))
            .map(|v| (v, func.dfg.resolve_aliases(v)))
            .collect();
        for &i in &body {
            let ni = func.dfg.clone_inst(i);
            func.set_srcloc(ni, func.srcloc(i));
            let mut data = func.dfg.insts[ni];
            let dfg = &mut func.dfg;
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |v| {
                    let v = *alias.get(&v).unwrap_or(&v);
                    *vmap.get(&v).unwrap_or(&v)
                },
            );
            func.dfg.insts[ni] = data;
            func.layout.insert_inst(ni, pinst);
            for (&o, &nv) in func
                .dfg
                .inst_results(i)
                .iter()
                .zip(func.dfg.inst_results(ni))
            {
                vmap.insert(func.dfg.resolve_aliases(o), nv);
            }
        }
        // `insert_inst` places before `pinst` in `pb` — but each clone
        // landed before `pinst` in program order already; operands
        // referenced earlier clones, so order them as emitted. The
        // layout keeps insertion order, which matches operand order.
        let new: Vec<BlockArg> = targs
            .iter()
            .map(|&v| {
                if let Some(&nv) = vmap.get(&v) {
                    return BlockArg::Value(nv);
                }
                match pmap.get(&v) {
                    Some(&a) => a,
                    None => BlockArg::Value(v),
                }
            })
            .collect();
        let dfg = &mut func.dfg;
        dfg.insts[pinst]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
            BlockCall::new(t, new, &mut dfg.value_lists);
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::stubhoist", "stubhoist: {b} into {pb} -> {t} ({} insts)", body.len());
        }
        n += 1;
    }
    n
}

pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    for _ in 0..MAX_ROUNDS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        let k = round(func, &la);
        n += k;
        if k == 0 {
            break;
        }
    }
    if n > 0 {
        remove_unreachable_blocks(func);
    }
    n
}

//! Duplicate small return blocks into their `jump` predecessors (LLVM's
//! tail duplication of return blocks). Helpers like `derive(PartialEq)`
//! end every arm with `jump ret(v)`; copying the `return` into each arm
//! removes the jump and the block-parameter move. Small `brif` blocks whose
//! values are only used inside them (a `bool` merged through a block
//! parameter, then tested) are copied the same way, so the `icmp` in each
//! predecessor feeds its own branch.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::CondCode;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, Value, ValueDef,
};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 4;
/// Escape-repair bounds: values threaded / use-blocks param'd per block.
const MAX_ESCAPE_VALS: usize = 4;
const MAX_ESCAPE_BLOCKS: usize = 8;
/// Repair attempts rebuild the CFG, so cap them to keep the pass linear.
const MAX_REPAIRS: usize = 16;

/// Feature switches (`PLIRON_TAILDUP_{BRIF,ESCAPE,EDGE}`); `run` reads
/// the env, tests construct it directly.
#[derive(Clone, Copy, Default)]
pub struct Opts {
    pub brif: bool,
    pub escape: bool,
    pub edge: bool,
}

fn candidate(
    func: &Function,
    uses: &FxHashMap<Value, Option<Block>>,
    b: Block,
    opts: Opts,
) -> bool {
    let Some(t) = func.layout.last_inst(b) else {
        return false;
    };
    let brif = func.dfg.insts[t].opcode() == Opcode::Brif && opts.brif;
    // Escaping values are repaired by parameterizing their use-blocks
    // (checked in `run`, which has the CFG); egraph2 merges per-edge
    // check tails whose args feed uses in successor bodies, exactly
    // this shape.
    let locals_ok = local_values(func, uses, b) || (brif && opts.escape);
    if !func.dfg.insts[t].opcode().is_return() && !(brif && locals_ok) {
        return false;
    }
    let mut n = 0;
    for i in func.layout.block_insts(b) {
        if i == t {
            continue;
        }
        let op = func.dfg.insts[i].opcode();
        // `iconst` is free to duplicate (remat sinks or hoists them
        // anyway); don't charge it against the size bound — late in the
        // pipeline it is what makes a 4-op check tail look "too big".
        if op.is_call()
            || op.can_store()
            || op.can_load()
            || op.can_trap()
            || op.other_side_effects()
            || op.is_branch()
            || op.is_terminator()
        {
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
            {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::taildup", "taildup: cand {b} impure {i} (fn {})", func.name)
                };
            }
            return false;
        }
        if op != Opcode::Iconst {
            n += 1;
        }
    }
    if n > MAX_INSTS
        && pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
    {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::taildup", "taildup: cand {b} too-big {n} (fn {})", func.name)
        };
    }
    n <= MAX_INSTS
}

/// For each value used in reachable code: the one block using it, or `None`
/// if several do.
fn use_blocks(func: &Function, domtree: &DominatorTree) -> FxHashMap<Value, Option<Block>> {
    let mut m: FxHashMap<Value, Option<Block>> = FxHashMap::default();
    for b in func.layout.blocks().filter(|&b| domtree.is_reachable(b)) {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let e = m.entry(func.dfg.resolve_aliases(v)).or_insert(Some(b));
                if *e != Some(b) {
                    *e = None;
                }
            }
        }
    }
    m
}

/// No parameter or result of `b` is used outside `b` (so copies need no SSA repair).
fn local_values(func: &Function, uses: &FxHashMap<Value, Option<Block>>, b: Block) -> bool {
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG);
    let ok = |v: &Value| {
        let good = uses.get(v).is_none_or(|&u| u == Some(b));
        if dbg && !good {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::taildup", "taildup: cand {b} escapes {v} (fn {})", func.name)
            };
        }
        good
    };
    func.dfg.block_params(b).iter().all(ok)
        && func
            .layout
            .block_insts(b)
            .all(|i| func.dfg.inst_results(i).iter().all(ok))
}

/// For each value used anywhere in the layout: every block containing a
/// use, RPO-sorted by first appearance. No reachability filter — clone
/// blocks minted mid-run are invisible to the caller's domtree snapshot,
/// and a dead block's extra site only over-approximates an escape set.
fn use_sites(func: &Function) -> FxHashMap<Value, Vec<Block>> {
    let mut m: FxHashMap<Value, Vec<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                let e = m.entry(v).or_default();
                if e.last() != Some(&b) {
                    e.push(b);
                }
            }
        }
    }
    m
}

/// A plan for repairing a candidate's escaping values: each use-block
/// takes the values it uses as fresh params, and every edge into it
/// supplies the pred's binding (`r`'s value on `r`'s own edges, the
/// pred's fresh param on a use-block's edges — `block174`'s check then
/// `block166`'s call chain them).
struct Repair {
    /// (use-block, values it gets params for) in RPO order.
    steps: Vec<(Block, Vec<Value>)>,
    /// (pred terminator, edge index, use-block) — args appended at apply.
    edges: Vec<(Inst, usize, Block)>,
}

/// `r`'s values escaping to use-blocks `D` are repairable when every
/// pred of every `D` can supply a binding: `r` itself (its value) or a
/// use-block already param'd in this plan (its fresh param). Anything
/// else — a third pred, a back edge in — bails: the clone would reach
/// `D` without the binding. The CFG is rebuilt fresh: clones earlier in
/// this run added edges (and removed pred terminators) the caller's
/// snapshot cannot see — a stale edge list both misses arg appends and
/// dereferences dead `Inst`s.
fn repair_plan(
    func: &Function,
    sites: &FxHashMap<Value, Vec<Block>>,
    r: Block,
) -> Option<Repair> {
    let cfg = ControlFlowGraph::with_function(func);
    let mut vals: Vec<Value> = func.dfg.block_params(r).to_vec();
    vals.extend(
        func.layout
            .block_insts(r)
            .flat_map(|i| func.dfg.inst_results(i).iter().copied().collect::<Vec<_>>()),
    );
    let escapees: Vec<Value> = vals
        .iter()
        .copied()
        .filter(|v| sites.get(v).is_some_and(|bs| bs.iter().any(|&b| b != r)))
        .collect();
    if escapees.is_empty() || escapees.len() > MAX_ESCAPE_VALS {
        return None;
    }
    let mut dbs: FxHashSet<Block> = escapees
        .iter()
        .flat_map(|v| sites[v].iter().copied())
        .filter(|&b| b != r)
        .collect();
    if dbs.len() > MAX_ESCAPE_BLOCKS {
        return None;
    }
    if pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
    {
        tracing::debug!(target: "pliron::taildup", "taildup: repair {r} escapees {escapees:?} use-blocks {dbs:?} (fn {})", func.name);
    }
    // A use-block supplies the escapees its successors need too, not
    // just the ones it uses — `needed[d]` is the use-block-local
    // backward union over use-block successors.
    let mut needed: FxHashMap<Block, FxHashSet<Value>> = dbs
        .iter()
        .map(|&d| {
            (
                d,
                escapees
                    .iter()
                    .copied()
                    .filter(|v| sites[v].contains(&d))
                    .collect(),
            )
        })
        .collect();
    loop {
        let mut grew = false;
        for &d in &dbs {
            let extra: Vec<Value> = cfg
                .succ_iter(d)
                .filter(|s| dbs.contains(s))
                .flat_map(|s| needed[&s].iter().copied().collect::<Vec<_>>())
                .collect();
            for v in extra {
                grew |= needed.get_mut(&d).unwrap().insert(v);
            }
        }
        if !grew {
            break;
        }
    }
    // Order steps so supply blocks precede the blocks they feed:
    // process a use-block once every pred can supply each `needed`
    // value — `r` (its value) or an already-stepped block (its param).
    let mut bound: FxHashSet<(Block, Value)> =
        escapees.iter().map(|&v| (r, v)).collect();
    let mut steps = Vec::new();
    let mut edges = Vec::new();
    while !dbs.is_empty() {
        let Some(&d) = dbs.iter().find(|&&d| {
            cfg.pred_iter(d).all(|bp| {
                needed[&d]
                    .iter()
                    .all(|&v| bound.contains(&(bp.block, v)))
            })
        }) else {
            return None;
        };
        for bp in cfg.pred_iter(d) {
            for (di, bc) in func.dfg.insts[bp.inst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                if bc.block(&func.dfg.value_lists) == d {
                    edges.push((bp.inst, di, d));
                }
            }
        }
        let vs: Vec<Value> = escapees
            .iter()
            .copied()
            .filter(|v| needed[&d].contains(v))
            .collect();
        for &v in &vs {
            bound.insert((d, v));
        }
        steps.push((d, vs));
        dbs.remove(&d);
    }
    Some(Repair { steps, edges })
}

/// Apply `plan`: param each use-block, rewrite its uses, then append the
/// pred's binding to every edge in. Runs before `r`'s clones so a clone
/// copies the repaired edge args mapped to its own analogs.
fn apply_repair(func: &mut Function, r: Block, plan: &Repair) {
    let mut supply: FxHashMap<(Block, Value), Value> = FxHashMap::default();
    for &(d, ref vs) in &plan.steps {
        for &v in vs {
            let ty = func.dfg.value_type(v);
            let p = func.dfg.append_block_param(d, ty);
            supply.insert((d, v), p);
        }
        let to: FxHashMap<Value, Value> = func
            .layout
            .block_insts(d)
            .flat_map(|i| {
                func.dfg
                    .inst_values(i)
                    .filter_map(|v| {
                        supply
                            .get(&(d, func.dfg.resolve_aliases(v)))
                            .map(|&p| (v, p))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        if !to.is_empty() {
            let insts: Vec<Inst> = func.layout.block_insts(d).collect();
            for i in insts {
                func.dfg
                    .map_inst_values(i, |v| to.get(&v).copied().unwrap_or(v));
            }
        }
    }
    for &(t, di, d) in &plan.edges {
        let p = func.layout.inst_block(t).unwrap();
        let vs: Vec<Value> = plan
            .steps
            .iter()
            .find(|(s, _)| *s == d)
            .map(|(_, vs)| vs.clone())
            .unwrap_or_default();
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[t]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di];
        for &v in &vs {
            let arg = if p == r { v } else { supply[&(p, v)] };
            bc.append_argument(arg, &mut dfg.value_lists);
        }
    }
}

/// The `icmp` behind `t`'s `brif` condition when it is the branch's only
/// use — complementing it is a free condition inversion (edge args keep
/// the un-complemented value, so any other use disqualifies it).
fn flip_inst(
    func: &Function,
    uses: &FxHashMap<Value, Option<Block>>,
    p: Block,
    t: Inst,
) -> Option<Inst> {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else {
        return None;
    };
    let arg = match func.dfg.value_alias_dest_for_serialization(arg) {
        Some(d) if d != Value::reserved_value() => d,
        Some(_) => return None,
        None => arg,
    };
    if uses.get(&arg) != Some(&Some(p)) {
        return None;
    }
    let ValueDef::Result(i, 0) = func.dfg.value_def(arg) else {
        return None;
    };
    if !matches!(
        func.dfg.insts[i],
        InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            ..
        }
    ) {
        return None;
    }
    let mut n = 0;
    for j in func.layout.block_insts(p) {
        n += func
            .dfg
            .inst_values(j)
            .filter(|&v| {
                func.dfg
                    .value_alias_dest_for_serialization(v)
                    .filter(|&d| d != Value::reserved_value())
                    .unwrap_or(v)
                    == arg
            })
            .count();
    }
    (n == 1).then_some(i)
}

/// Every value `r` uses from outside `r` is available at the end of `p`.
pub(crate) fn outer_dominate(func: &Function, domtree: &DominatorTree, r: Block, p: Block) -> bool {
    func.layout.block_insts(r).all(|i| {
        func.dfg.inst_values(i).all(|v| {
            let def = match func.dfg.resolved_value_def(v) {
                ValueDef::Result(d, _) => func.layout.inst_block(d),
                ValueDef::Param(b, _) => Some(b),
                _ => None,
            };
            def.is_some_and(|b| b == r || domtree.block_dominates(b, p))
        })
    })
}

/// Is the CFG irreducible: a retreating edge (in RPO) whose target does not
/// dominate its source? Rebuilds the CFG and domtree — diagnostics only.
pub fn irreducible(func: &Function) -> bool {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let mut rpo: FxHashMap<Block, usize> = FxHashMap::default();
    for (i, &b) in domtree.cfg_rpo().enumerate() {
        rpo.insert(b, i);
    }
    func.layout.blocks().filter(|&b| domtree.is_reachable(b)).any(|b| {
        cfg.succ_iter(b)
            .any(|s| rpo[&s] <= rpo[&b] && !domtree.block_dominates(s, b))
    })
}

/// Returns the number of jumps replaced by a copy of the return block.
pub fn run(func: &mut Function, prof: &FxHashMap<Block, u64>) -> usize {
    run_opts(
        func,
        prof,
        Opts {
            brif: pliron_cfg::env_is("PLIRON_TAILDUP_BRIF", "1"),
            escape: pliron_cfg::env_is("PLIRON_TAILDUP_ESCAPE", "1"),
            edge: pliron_cfg::env_is("PLIRON_TAILDUP_EDGE", "1"),
        },
    )
}

pub fn run_opts(func: &mut Function, prof: &FxHashMap<Block, u64>, opts: Opts) -> usize {
    let edge_on = opts.edge;
    // `PLIRON_TAILDUP_COLD` (opt-in): skip tails and preds a cold mark
    // or a zero PGO count flags — duplicating into them is code size
    // with no hot-path payoff.
    let cold_blocks: FxHashSet<Block> = if pliron_cfg::env_is("PLIRON_TAILDUP_COLD", "1") {
        func.layout
            .blocks()
            .filter(|&b| func.layout.is_cold(b) || prof.get(&b) == Some(&0))
            .collect()
    } else {
        FxHashSet::default()
    };
    let is_cold = |b: Block| cold_blocks.contains(&b);
    let reducible = crate::jumpthread::keep_reducible();
    let entry = func.layout.entry_block();
    let mut cfg = ControlFlowGraph::with_function(func);
    let mut domtree = DominatorTree::with_function(func, &cfg);
    // Dead blocks would pin values as used elsewhere and keep dangling refs
    // to removed blocks; they never run, so drop them first.
    let dead: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&b| !domtree.is_reachable(b))
        .collect();
    if !dead.is_empty() {
        for &b in &dead {
            while let Some(i) = func.layout.first_inst(b) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(b);
        }
        cfg = ControlFlowGraph::with_function(func);
        domtree = DominatorTree::with_function(func, &cfg);
    }
    let mut la = LoopAnalysis::new();
    if reducible {
        la.compute(func, &cfg, &domtree);
    }
    if pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
        && irreducible(func)
    {
        tracing::debug!(target: "pliron::taildup", "taildup: IRREDUCIBLE at entry (fn {})", func.name);
    }
    let mut uses = use_blocks(func, &domtree);
    let rets: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| Some(b) != entry && !is_cold(b) && candidate(func, &uses, b, opts))
        .collect();
    if rets.is_empty() {
        return 0;
    }
    let mut sites: Option<FxHashMap<Value, Vec<Block>>> = None;
    let mut repairs: FxHashMap<Block, Option<Repair>> = FxHashMap::default();
    let mut repairs_attempted = 0;
    let mut repaired: FxHashSet<Block> = FxHashSet::default();
    // Clone-block -> def-blocks of the values it uses but does not
    // define. A clone copies its source's *current* terminator, edges
    // already retargeted to earlier clones included; that new edge
    // makes the clone's host a fresh pred on a path the target's
    // free-value defs may not dominate.
    let mut clone_imports: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for p in blocks {
        if is_cold(p) {
            continue;
        }
        let Some(t) = func.layout.last_inst(p) else {
            continue;
        };
        // A `jump` pred gets the copy in place; a `brif` pred gets one clone
        // block per edge to a candidate (the flag merge it feeds, e.g.
        // `is_alphabetic`'s bool param, then never materialises as a value).
        let is_brif = matches!(func.dfg.insts[t], InstructionData::Brif { .. });
        let dests: Vec<BlockCall> = match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => vec![destination],
            InstructionData::Brif { blocks, .. } if edge_on => {
                blocks.to_vec()
            }
            _ => continue,
        };
        // Edges whose target loops back through a dominator of `p` — latch
        // merges, worth cloning even without foldable args: the copy moves
        // the loop's hot tail onto the branch's fallthrough edge instead of
        // a `jmp` per iteration.
        let latches: Vec<bool> = dests
            .iter()
            .map(|d| {
                let r = d.block(&func.dfg.value_lists);
                is_brif
                    && r != p
                    && func.layout.last_inst(r).is_some_and(|rt| {
                        func.dfg.insts[rt]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                            .iter()
                            .any(|bc| domtree.block_dominates(bc.block(&func.dfg.value_lists), p))
                    })
            })
            .collect();
        // Compare to complement when an edge-0 latch clone must be moved
        // to the second edge to land next in emitted order.
        let mut flip0 = None;
        for (di, &dest) in dests.iter().enumerate() {
            let r = dest.block(&func.dfg.value_lists);
            let latch = latches[di];
            // When the second edge's latch clone will take the fallthrough
            // slot anyway, an edge-0 clone is pure duplication — skip it.
            if latch && di == 0 && latches.get(1) == Some(&true) {
                continue;
            }
            // Brif-edge clones only pay when an arg is a constant the cloned
            // body can fold — otherwise it's pure dispatch-chain duplication.
            if is_brif
                && !latch
                && !dest.args(&func.dfg.value_lists).any(|a| {
                    matches!(a, BlockArg::Value(v)
                        if func.dfg.resolved_value_def(v).inst()
                            .is_some_and(|i| func.dfg.insts[i].opcode() == Opcode::Iconst))
                })
            {
                continue;
            }
            // The clone lands next in emitted order only as the brif's
            // second successor, so an edge-0 latch clone needs a freely
            // invertible compare to swap onto it.
            let flip = latch.then(|| flip_inst(func, &uses, p, t)).flatten();
            if latch && di == 0 && flip.is_none() {
                continue;
            }
            let why = if r == p {
                "self"
            } else if !rets.contains(&r) {
                "not-candidate"
            } else if !domtree.is_reachable(p) {
                "unreachable"
            } else if !outer_dominate(func, &domtree, r, p) {
                "outer-dom"
            } else if reducible && crate::jumpthread::side_enters(func, &la, p, r) {
                "irreducible"
            } else {
                ""
            };
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
                && !why.is_empty()
            {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::taildup", "taildup: skip {p}->{r} edge {di}: {why} (fn {})", func.name)
                };
            }
            if !why.is_empty() {
                continue;
            }
            // `p` must not use `r`'s own defs anywhere — the clone (or a
            // surviving sibling edge arg) would keep them referenced after
            // `r` loses its last pred and is removed (quicksort's
            // `brif .., block68(a,b), block1191(param68,param68)` case).
            let uses_r_defs = {
                let mut defs: FxHashSet<Value> = func.dfg.block_params(r).iter().copied().collect();
                defs.extend(
                    func.layout
                        .block_insts(r)
                        .flat_map(|i| func.dfg.inst_results(i).iter().copied().collect::<Vec<_>>()),
                );
                func.layout.block_insts(p).any(|i| {
                    func.dfg
                        .inst_values(i)
                        .any(|v| defs.contains(&func.dfg.resolve_aliases(v)))
                })
            };
            if uses_r_defs {
                continue;
            }
            // Edge args defined inside any candidate are likewise unsafe:
            // that candidate may lose its last pred and be removed,
            // stranding the clone's reference (the `isub v3588, v3593`
            // clone of block1191 carried block68's params).
            let arg_in_candidate = dest.args(&func.dfg.value_lists).any(|a| {
                if let BlockArg::Value(v) = a {
                    let b = match func.dfg.resolved_value_def(v) {
                        ValueDef::Result(d, _) => func.layout.inst_block(d),
                        ValueDef::Param(b, _) => Some(b),
                        _ => None,
                    };
                    b.is_some_and(|b| rets.contains(&b))
                } else {
                    false
                }
            });
            if arg_in_candidate {
                continue;
            }
            // A candidate whose params or results escape into successor
            // bodies is cloned only after the use-blocks are param'd and
            // the in-edges carry the bindings — otherwise the clone's
            // path reaches them unbound. Repair attempts are capped:
            // each rebuilds the CFG, so the pass stays linear overall.
            if !local_values(func, &uses, r) {
                if !repairs.contains_key(&r) {
                    if repairs_attempted >= MAX_REPAIRS {
                        continue;
                    }
                    repairs_attempted += 1;
                    let s = sites.get_or_insert_with(|| use_sites(func));
                    repairs.insert(r, repair_plan(func, s, r));
                }
                match repairs.get(&r).unwrap() {
                    // `local_values` failed on an already-repaired `r`: a
                    // post-repair clone minted a new use-block for an
                    // escapee (a free value in another candidate's copy).
                    // No plan covers it — keep `r`'s pred so the def's
                    // dominance over the new site survives.
                    Some(_) if repaired.contains(&r) => continue,
                    Some(_) => {
                        apply_repair(func, r, repairs[&r].as_ref().unwrap());
                        // Post-repair an escapee's only use is `r`'s own
                        // edge args: publish that into `uses`/`sites` so
                        // `local_values` fast-passes the remaining edges
                        // and the arm above sees only genuinely new sites.
                        for (_, vs) in &repairs[&r].as_ref().unwrap().steps {
                            for &v in vs {
                                uses.insert(v, Some(r));
                                if let Some(s) = sites.as_mut() {
                                    s.insert(v, vec![r]);
                                }
                            }
                        }
                        repaired.insert(r);
                    }
                    None => continue,
                }
            }
            // The clone copies `r`'s terminator — including edges that
            // earlier clones retargeted onto clone blocks. Those clones'
            // free values are valid only where their defs dominate; a new
            // edge sourced from `p`/`nb` must stay under them (original
            // targets are safe: an imported def dominates `r`, hence `p`).
            let bad_edge = func.layout.last_inst(r).is_some_and(|rt| {
                func.dfg.insts[rt]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .any(|bc| {
                        clone_imports.get(&bc.block(&func.dfg.value_lists)).is_some_and(
                            |defs| defs.iter().any(|&d| !domtree.block_dominates(d, p)),
                        )
                    })
            });
            if bad_edge {
                continue;
            }
            let mut map: FxHashMap<Value, Value> = FxHashMap::default();
            let args: Vec<BlockArg> = dest.args(&func.dfg.value_lists).collect();
            let mut ok = true;
            for (&prm, a) in func.dfg.block_params(r).iter().zip(&args) {
                match a {
                    BlockArg::Value(v) => {
                        map.insert(prm, *v);
                    }
                    _ => ok = false,
                }
            }
            if !ok {
                continue;
            }
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
            {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::taildup", "taildup: clone {r} into {p} edge {di} (fn {})", func.name)
                };
            }
            let body: Vec<_> = func.layout.block_insts(r).collect();
            let alias: FxHashMap<Value, Value> = body
                .iter()
                .flat_map(|&i| func.dfg.inst_values(i))
                .map(|v| (v, func.dfg.resolve_aliases(v)))
                .collect();
            let mut new_insts = Vec::with_capacity(body.len());
            for i in body {
                let ni = func.dfg.clone_inst(i);
                // `-g`: the copy is the same source operation.
                func.set_srcloc(ni, func.srcloc(i));
                let dfg = &mut func.dfg;
                let mut data = dfg.insts[ni];
                for d in
                    data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                {
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
                for (&o, &nv) in func
                    .dfg
                    .inst_results(i)
                    .iter()
                    .zip(func.dfg.inst_results(ni))
                {
                    map.insert(o, nv);
                }
                new_insts.push(ni);
            }
            let host = if matches!(func.dfg.insts[t], InstructionData::Jump { .. }) {
                for &ni in &new_insts {
                    func.layout.insert_inst(ni, t);
                }
                func.layout.remove_inst(t);
                p
            } else {
                let nb = func.dfg.make_block();
                func.layout.insert_block_after(nb, p);
                for &ni in &new_insts {
                    func.layout.append_inst(ni, nb);
                }
                let dfg = &mut func.dfg;
                dfg.insts[t]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
                    BlockCall::new(nb, std::iter::empty(), &mut dfg.value_lists);
                nb
            };
            // `host` now uses every free value the clone's body does
            // (`v2235`-style: another block's def stays unmapped). Record
            // them — a later `local_values`/`sites` query must see these
            // fresh uses or it will clone the def block unbound. For a
            // new block, also record the def-blocks so later clones keep
            // any new edge into `host` under them.
            let mut defs = FxHashSet::default();
            for &ni in &new_insts {
                for v in func.dfg.inst_values(ni) {
                    let (d, ok) = match func.dfg.resolved_value_def(v) {
                        ValueDef::Result(d, _) => {
                            (func.layout.inst_block(d), func.layout.inst_block(d) != Some(host))
                        }
                        ValueDef::Param(d, _) => (Some(d), d != host),
                        _ => (None, false),
                    };
                    if !ok {
                        continue;
                    }
                    let Some(d) = d else { continue };
                    defs.insert(d);
                    let v = func.dfg.resolve_aliases(v);
                    if let Some(s) = sites.as_mut() {
                        let e = s.entry(v).or_default();
                        if !e.contains(&host) {
                            e.push(host);
                        }
                    }
                    let u = uses.entry(v).or_insert(Some(host));
                    if *u != Some(host) {
                        *u = None;
                    }
                }
            }
            if host != p {
                clone_imports.insert(host, defs);
            }
            if latch && di == 0 {
                flip0 = flip;
            }
            n += 1;
        }
        if let Some(cmp) = flip0 {
            // Swap the clone onto the second edge so it is next in emitted
            // order: complement the compare, swap targets. `uses` predates
            // the clones this run minted — a clone's brif can still share
            // the compare, so complement a private copy rather than the
            // shared def (rustlantis seed1: the in-place `eq`→`ne` flip
            // silently inverted the clone's own brif — loop never exited).
            let ni = func.dfg.clone_inst(cmp);
            // `-g`: the copy is the same source operation.
            func.set_srcloc(ni, func.srcloc(cmp));
            if let InstructionData::IntCompare { cond, .. } = &mut func.dfg.insts[ni] {
                *cond = cond.complement();
            }
            func.layout.insert_inst(ni, t);
            let nv = func.dfg.first_result(ni);
            let mut data = func.dfg.insts[t];
            {
                let dfg = &mut func.dfg;
                if let InstructionData::Brif { arg, .. } = &mut data {
                    *arg = nv;
                }
                data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                    .swap(0, 1);
            }
            func.dfg.insts[t] = data;
        }
    }
    // Return blocks left without predecessors would fail dominance checks.
    let cfg = ControlFlowGraph::with_function(func);
    let mut removed = false;
    for r in rets {
        if cfg.pred_iter(r).next().is_none() {
            while let Some(i) = func.layout.first_inst(r) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(r);
            removed = true;
        }
    }
    // Removing a candidate strands the blocks it dominated: they lose
    // their entry path but keep references to the removed defs. They are
    // unreachable, so sweep them too. `cfg` still carries the removed
    // blocks' out-edges; rebuild it before judging reachability.
    if removed {
        let cfg = ControlFlowGraph::with_function(func);
        let domtree = DominatorTree::with_function(func, &cfg);
        for b in func
            .layout
            .blocks()
            .filter(|&b| !domtree.is_reachable(b))
            .collect::<Vec<_>>()
        {
            while let Some(i) = func.layout.first_inst(b) {
                func.layout.remove_inst(i);
            }
            func.layout.remove_block(b);
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::types;
    use cranelift_codegen::ir::{AbiParam, ExternalName, Signature, UserFuncName};
    use cranelift_codegen::ir::{Function, InstBuilder};
    use cranelift_codegen::isa::CallConv;

    /// `q`'s `jump` clones `r`'s `brif c` in place; later `r` is processed
    /// as a pred and its edge-0 latch clone flips `c`. `uses` predates the
    /// clone, so complementing `c` in place silently inverted `q`'s copy
    /// (rustlantis seed1 — the flipped loop test never exited). The flip
    /// must complement a private compare instead.
    #[test]
    fn flip_leaves_cloned_brif_polarity() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);

        let entry = func.dfg.make_block();
        let q = func.dfg.make_block();
        let r = func.dfg.make_block();
        let latch = func.dfg.make_block();
        let ret_a = func.dfg.make_block();
        let ret_b = func.dfg.make_block();
        for b in [entry, q, r, latch, ret_a, ret_b] {
            func.layout.append_block(b);
        }

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        cur.ins().jump(q, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(q);
        let x = cur.ins().iconst(types::I64, 5);
        let y = cur.ins().iconst(types::I64, 7);
        let c = cur.ins().icmp(IntCC::Equal, x, y);
        cur.ins().jump(r, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(r);
        cur.set_srcloc(cranelift_codegen::ir::SourceLoc::new(42));
        cur.ins().brif(c, latch, &[], ret_a, &[]);

        // `latch` branches back to `q` — a dominator of `r` — so edge 0 of
        // `r` is a latch clone and triggers the compare complement.
        let mut cur = FuncCursor::new(&mut func).at_bottom(latch);
        let c2 = cur.ins().icmp(IntCC::UnsignedLessThan, x, y);
        cur.ins().brif(c2, q, &[], ret_b, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(ret_a);
        cur.ins().return_(&[x]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(ret_b);
        cur.ins().return_(&[y]);

        let n = run_opts(
            &mut func,
            &FxHashMap::default(),
            Opts {
                brif: true,
                escape: false,
                edge: true,
            },
        );
        assert!(n >= 2, "expected both clones, ran {n}:\n{}", func.display());

        // The brif cloned into `q` must still test `c == ..`, i.e. its arg's
        // def keeps `Equal`; the in-place flip would leave `NotEqual`.
        let qt = func.layout.last_inst(q).unwrap();
        let InstructionData::Brif { arg, .. } = func.dfg.insts[qt] else {
            panic!("q no longer ends in brif:\n{}", func.display());
        };
        let arg = func.dfg.resolve_aliases(arg);
        let d = func.dfg.value_def(arg).inst().unwrap();
        let InstructionData::IntCompare { cond, .. } = func.dfg.insts[d] else {
            panic!("cloned brif arg not an icmp:\n{}", func.display());
        };
        assert_eq!(cond, IntCC::Equal, "flip leaked into q's clone:\n{}", func.display());
        // `-g`: the clone is the same source operation, so it keeps the
        // source location (else its code joins the previous line's row).
        assert_eq!(func.srcloc(qt), cranelift_codegen::ir::SourceLoc::new(42));

        let flags = cranelift_codegen::settings::Flags::new(
            cranelift_codegen::settings::builder(),
        );
        cranelift_codegen::verify_function(&func, &flags).unwrap();
    }
}

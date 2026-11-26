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
    Block, BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, Type, Value, ValueDef,
};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 4;
/// Escape-repair bounds: values threaded / use-blocks param'd per block.
const MAX_ESCAPE_VALS: usize = 4;
/// Use-blocks plus transit blocks (preds that only forward a binding
/// deeper into the cone — cold unwind chains run long).
const MAX_ESCAPE_BLOCKS: usize = 16;
/// Forward reachability bound for `r`'s successor cone.
const MAX_CONE: usize = 64;
/// Repair attempts rebuild the CFG, so cap them to keep the pass linear.
const MAX_REPAIRS: usize = 16;

/// Feature switches (`PLIRON_TAILDUP_{BRIF,ESCAPE,EDGE}`); `run` reads
/// the env, tests construct it directly.
#[derive(Clone, Copy, Default)]
pub struct Opts {
    pub brif: bool,
    pub escape: bool,
    pub edge: bool,
    /// `PLIRON_TAILDUP_STORE=1`: plain stores in a `brif` candidate whose
    /// condition is one of its own parameters, copied only along edges that
    /// pass a constant for it (the branch then folds: jump threading through
    /// a merge with a side effect). lol_html's `consume_ch`: `pos += 1` is
    /// stored in the merge of `Some(ch)`/`None` before the `match`.
    pub store: bool,
    /// `PLIRON_TAILDUP_STOREICMP=1` (opt-in): the `store` shape with the
    /// `brif` on `icmp param, const` instead of the bare parameter —
    /// a promoted discriminant tested against its sentinel (`icmp eq
    /// tag, -1` over `Option<_>`'s niche in lol_html's `finish_tag_name`:
    /// `is_end = false` is stored in the same merge before `if let
    /// Some(..)`). The edge's constant for the parameter still decides
    /// the clone's compare.
    pub store_icmp: bool,
}

/// The parameter index deciding `b`'s `brif`: the condition itself when
/// it is a parameter, or — with `icmp_ok` — the parameter an
/// `icmp param, iconst` compares against the constant. Returning an
/// index under `icmp_ok` promises each clone's compare folds to a
/// constant: the edge supplies a constant for the parameter (the caller
/// checks) and the other operand is already an `iconst`.
fn cond_param(func: &Function, b: Block, icmp_ok: bool) -> Option<usize> {
    let t = func.layout.last_inst(b)?;
    let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else { return None };
    let arg = func.dfg.resolve_aliases(arg);
    let params = func.dfg.block_params(b);
    if let Some(k) = params.iter().position(|&p| p == arg) {
        return Some(k);
    }
    if !icmp_ok {
        return None;
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(arg) else {
        return None;
    };
    // The compare must be inside `b` — a clone copies it, folding the
    // edge-supplied constant against the `iconst` operand. An outside
    // compare stays live in the clone: no fold, pure duplication.
    if func.layout.inst_block(i) != Some(b) {
        return None;
    }
    let InstructionData::IntCompare { args, .. } = func.dfg.insts[i] else {
        return None;
    };
    let iconst = |v: Value| {
        func.dfg
            .resolved_value_def(v)
            .inst()
            .is_some_and(|d| func.dfg.insts[d].opcode() == Opcode::Iconst)
    };
    args.iter().find_map(|&a| {
        let a = func.dfg.resolve_aliases(a);
        let other = if a == func.dfg.resolve_aliases(args[0]) {
            args[1]
        } else {
            args[0]
        };
        (iconst(other))
            .then(|| params.iter().position(|&p| p == a))
            .flatten()
    })
}

fn plain_store(op: Opcode) -> bool {
    matches!(op, Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32)
}

fn has_store(func: &Function, b: Block) -> bool {
    func.layout.block_insts(b).any(|i| plain_store(func.dfg.insts[i].opcode()))
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
        let store_ok =
            opts.store && brif && plain_store(op) && cond_param(func, b, opts.store_icmp).is_some();
        // `iconst` is free to duplicate (remat sinks or hoists them
        // anyway); don't charge it against the size bound — late in the
        // pipeline it is what makes a 4-op check tail look "too big".
        if op.is_call()
            || (op.can_store() && !store_ok)
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
                    tracing::debug!(target: "pliron::taildup", "taildup: cand {b} impure {i} {} (fn {})", func.dfg.display_inst(i), func.name)
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

/// A plan for repairing a candidate's escaping values: each block that
/// uses one — or merely forwards it toward a use (the cold unwind chain
/// between the clone's target and a panic block, e.g. the enum-field
/// asserts lol_html's `finish_tag_name` leaves under `is_end = false`'s
/// merge) — takes the values it must bind as fresh params, and every
/// edge into it supplies the pred's binding.
struct Repair {
    /// (block, values it gets params for) in supply order.
    steps: Vec<(Block, Vec<Value>)>,
    /// (pred terminator, edge index, param'd block) — args appended at apply.
    edges: Vec<(Inst, usize, Block)>,
    /// (use-block, escapee) — an `iconst` re-materialized at the
    /// use-block's top, rewriting uses of the value or any alias inside.
    remats: Vec<(Block, Value)>,
    /// (pred-block, escapee) supplied by the raw value: the def still
    /// dominates the pred's tail after clones thread past `r` (the pred
    /// lies outside `r`'s successor cone).
    direct: FxHashSet<(Block, Value)>,
    /// (pred-block, escapee) supplied by a constant re-materialized in
    /// the pred: the escapee's value is known in the pred's arm region
    /// (`icmp eq tag, -1`'s true arm pins the tag; the condition itself
    /// is 1/0), so no param forwarding is needed.
    consts: FxHashMap<(Block, Value), i64>,
    /// (use-block, escapee) bound to a constant inside the use-block
    /// itself (the block sits under a decided arm): an `iconst` minted
    /// at its top rewrites the uses — no param, no edge binding.
    use_consts: FxHashMap<(Block, Value), i64>,
}

/// `r`'s values escaping to use-blocks `D` are repairable when every
/// edge in can supply a binding. A pred supplies `r`'s own value (`r`'s
/// edge), its fresh param once param'd, or the raw value when the def
/// still dominates the pred's tail post-transform — i.e. the pred is
/// not reachable from `r`'s successors without re-passing `r` (clones
/// add edges only into that cone). A pred that can do none of those is
/// param'd too — a *transit* block — pushing the demand up to its own
/// preds until the chain bottoms out at `r`. Anything else — a back
/// edge in, a path the escapee never reaches — bails: the clone would
/// reach `D` without the binding. `iconst` escapees need no params:
/// they are cloned at the top of each use-block. The CFG and domtree
/// are rebuilt fresh: clones earlier in this run added edges the
/// caller's snapshots cannot see.
fn repair_plan(
    func: &Function,
    sites: &FxHashMap<Value, Vec<Block>>,
    r: Block,
) -> Option<Repair> {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let mut vals: Vec<Value> = func.dfg.block_params(r).to_vec();
    vals.extend(
        func.layout
            .block_insts(r)
            .flat_map(|i| func.dfg.inst_results(i).iter().copied().collect::<Vec<_>>()),
    );
    let iconst = |v: Value| {
        func.dfg
            .resolved_value_def(v)
            .inst()
            .is_some_and(|i| func.dfg.insts[i].opcode() == Opcode::Iconst)
    };
    let (mut remats, mut escapees) = (Vec::new(), Vec::new());
    for &v in &vals {
        let Some(bs) = sites.get(&v) else { continue };
        if iconst(v) {
            for &b in bs {
                if b != r {
                    remats.push((b, v));
                }
            }
        } else if bs.iter().any(|&b| b != r) {
            escapees.push(v);
        }
    }
    if pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
    {
        tracing::debug!(target: "pliron::taildup", "taildup: repair {r} escapees {escapees:?} remats {remats:?} (fn {})", func.name);
    }
    if escapees.len() > MAX_ESCAPE_VALS {
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
        {
            tracing::debug!(target: "pliron::taildup", "taildup: repair {r} bail escapees {escapees:?} (fn {})", func.name);
        }
        return None;
    }
    // Blocks forward-reachable from `r`'s successors: the new clone edges
    // all land inside this cone, so only inside it can `r`'s dominance
    // of an escapee's uses break. Outside it `r`-dominated stays dominated.
    let mut cone: FxHashSet<Block> = cfg
        .succ_iter(r)
        .filter(|&s| s != r)
        .collect();
    {
        let mut wk: Vec<Block> = cone.iter().copied().collect();
        while let Some(b) = wk.pop() {
            for s in cfg.succ_iter(b) {
                if s != r && cone.insert(s) {
                    wk.push(s);
                }
            }
            if cone.len() > MAX_CONE {
                if pliron_cfg::tracing_on()
                    && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
                {
                    tracing::debug!(target: "pliron::taildup", "taildup: repair {r} bail cone (fn {})", func.name);
                }
                return None;
            }
        }
    }
    // Arm constants: inside the region dominated by a `brif` successor
    // of `r`, the condition is decided — 1 under the true arm, 0 under
    // the false — and when the condition is `icmp eq param, iconst` the
    // parameter equals the constant under the true arm (the
    // `finish_tag_name` tag: `-1` everywhere under `block10`). A binding
    // there is a re-materialized `iconst`, not a forwarded param.
    let mut arm_consts: FxHashMap<Value, Vec<(Block, i64)>> = FxHashMap::default();
    if let Some(t) = func.layout.last_inst(r) {
        if let InstructionData::Brif { arg, .. } = func.dfg.insts[t] {
            let dests: Vec<Block> = func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .map(|bc| bc.block(&func.dfg.value_lists))
                .collect();
            if let [t_arm, e_arm] = dests[..] {
                let arg = func.dfg.resolve_aliases(arg);
                for (arm, c) in [(t_arm, 1i64), (e_arm, 0)] {
                    arm_consts.entry(arg).or_default().push((arm, c));
                }
                if let ValueDef::Result(i, _) = func.dfg.value_def(arg) {
                    if let InstructionData::IntCompare {
                        cond: cranelift_codegen::ir::condcodes::IntCC::Equal,
                        args,
                        ..
                    } = func.dfg.insts[i]
                    {
                        let params = func.dfg.block_params(r);
                        for (a, o) in [(args[0], args[1]), (args[1], args[0])] {
                            let a = func.dfg.resolve_aliases(a);
                            let c = func.dfg.resolved_value_def(o).inst().and_then(|d| {
                                if let InstructionData::UnaryImm { imm, .. } = func.dfg.insts[d] {
                                    Some(imm.bits())
                                } else {
                                    None
                                }
                            });
                            if params.contains(&a) {
                                if let Some(c) = c {
                                    arm_consts.entry(a).or_default().push((t_arm, c));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let arm_val = |v: Value, b: Block| -> Option<i64> {
        arm_consts.get(&v).and_then(|arms| {
            arms.iter()
                .find(|(arm, _)| domtree.block_dominates(*arm, b))
                .map(|&(_, c)| c)
        })
    };
    // `dbs`: blocks needing new params — use-blocks inside the cone.
    // Outside the cone no clone edge lands, so `r`-dominated uses stay
    // dominated and need no rebinding at all.
    let mut dbs: FxHashSet<Block> = escapees
        .iter()
        .flat_map(|v| sites[v].iter().copied())
        .filter(|&b| b != r && cone.contains(&b))
        .collect();
    // `needed[d]`: the escapees `d` must take params for — the ones it
    // uses (unless `d` sits under a decided arm, where `use_consts`
    // mints an `iconst` at `d`'s top and rewrites the uses) plus
    // whatever a needing successor forwards that `d` cannot itself
    // const-supply. A pred `bp` supplies `v` itself when `r`, already
    // param'd (`needed[bp]`), arm-constant (`consts`), or the raw
    // value's def dominates `bp`'s tail with `bp` outside the cone
    // (`direct`). Otherwise `bp` joins `dbs` as a transit block.
    let mut use_consts: FxHashMap<(Block, Value), i64> = FxHashMap::default();
    let mut needed: FxHashMap<Block, FxHashSet<Value>> = FxHashMap::default();
    for &d in &dbs {
        let mut ns = FxHashSet::default();
        for &v in &escapees {
            if !sites[&v].contains(&d) {
                continue;
            }
            if let Some(c) = arm_val(v, d) {
                use_consts.insert((d, v), c);
            } else {
                ns.insert(v);
            }
        }
        needed.insert(d, ns);
    }
    let mut direct: FxHashSet<(Block, Value)> = FxHashSet::default();
    let mut consts: FxHashMap<(Block, Value), i64> = FxHashMap::default();
    let supplyable = |bp: Block,
                      v: Value,
                      needed: &FxHashMap<Block, FxHashSet<Value>>,
                      direct: &mut FxHashSet<(Block, Value)>,
                      consts: &mut FxHashMap<(Block, Value), i64>| {
        if bp == r || needed.get(&bp).is_some_and(|ns| ns.contains(&v)) {
            return true;
        }
        if direct.contains(&(bp, v)) || consts.contains_key(&(bp, v)) {
            return true;
        }
        // A pred under a decided arm supplies the constant.
        if let Some(c) = arm_val(v, bp) {
            consts.insert((bp, v), c);
            return true;
        }
        if cone.contains(&bp) {
            return false;
        }
        // Unreachable preds never execute: bind the raw value (the
        // verifier skips unreachable blocks).
        if !domtree.is_reachable(bp) {
            direct.insert((bp, v));
            return true;
        }
        let ok = match func.dfg.resolved_value_def(v) {
            ValueDef::Param(b, _) => domtree.block_dominates(b, bp),
            ValueDef::Result(i, _) => func
                .layout
                .last_inst(bp)
                .is_some_and(|t| domtree.dominates(i, t, &func.layout)),
            _ => false,
        };
        if ok {
            direct.insert((bp, v));
        }
        ok
    };
    loop {
        let mut grew = false;
        for &d in &dbs.clone() {
            // A successor's need reaches `d` only when `d` cannot bind
            // the value itself — under a decided arm `d` supplies the
            // constant, so no param (and no further forwarding) is due.
            let extra: Vec<Value> = cfg
                .succ_iter(d)
                .filter(|s| dbs.contains(s))
                .flat_map(|s| needed[&s].iter().copied())
                .filter(|&v| arm_val(v, d).is_none())
                .collect();
            for v in extra {
                grew |= needed.get_mut(&d).unwrap().insert(v);
            }
            let vs: Vec<Value> = needed[&d].iter().copied().collect();
            for bp in cfg.pred_iter(d) {
                for &v in &vs {
                    if !supplyable(bp.block, v, &needed, &mut direct, &mut consts) {
                        if dbs.insert(bp.block) && dbs.len() > MAX_ESCAPE_BLOCKS {
                            if pliron_cfg::tracing_on()
                                && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
                            {
                                tracing::debug!(target: "pliron::taildup", "taildup: repair {r} bail blocks {dbs:?} escapees {escapees:?} (fn {})", func.name);
                            }
                            return None;
                        }
                        grew |= needed.entry(bp.block).or_default().insert(v);
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    // Order steps so supply blocks precede the blocks they feed:
    // process a block once every pred can supply each `needed` value.
    let mut bound: FxHashSet<(Block, Value)> =
        escapees.iter().map(|&v| (r, v)).collect();
    bound.extend(direct.iter().copied());
    bound.extend(consts.keys().copied());
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
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
            {
                tracing::debug!(target: "pliron::taildup", "taildup: repair {r} bail supply {dbs:?} (fn {})", func.name);
            }
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
    if pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
    {
        tracing::debug!(target: "pliron::taildup", "taildup: repair {r} plan steps {steps:?} direct {direct:?} consts {consts:?} (fn {})", func.name);
    }
    Some(Repair {
        steps,
        edges,
        remats,
        direct,
        consts,
        use_consts,
    })
}

/// Apply `plan`: re-materialize escapee constants, param each
/// use/transit block, rewrite its uses, then append the pred's binding
/// to every edge in. Runs before `r`'s clones so a clone copies the
/// repaired edge args mapped to its own analogs.
fn apply_repair(func: &mut Function, r: Block, plan: &Repair) {
    for &(d, v) in &plan.remats {
        let Some(src) = func.dfg.resolved_value_def(v).inst() else {
            continue;
        };
        let ni = func.dfg.clone_inst(src);
        func.set_srcloc(ni, func.srcloc(src));
        let nv = func.dfg.inst_results(ni)[0];
        if let Some(first) = func.layout.first_inst(d) {
            func.layout.insert_inst(ni, first);
        } else {
            func.layout.append_inst(ni, d);
        }
        let to: FxHashMap<Value, Value> = func
            .layout
            .block_insts(d)
            .flat_map(|i| func.dfg.inst_values(i))
            .filter(|&u| func.dfg.resolve_aliases(u) == v)
            .map(|u| (u, nv))
            .collect();
        if !to.is_empty() {
            for i in func.layout.block_insts(d).collect::<Vec<_>>() {
                if i != ni {
                    func.dfg
                        .map_inst_values(i, |u| to.get(&u).copied().unwrap_or(u));
                }
            }
        }
    }
    let mint = |func: &mut Function, b: Block, ty: Type, c: i64| -> Value {
        let ni = func.dfg.make_inst(InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm: c.into(),
        });
        func.dfg.make_inst_results(ni, ty);
        let nv = func.dfg.inst_results(ni)[0];
        if let Some(first) = func.layout.first_inst(b) {
            func.layout.insert_inst(ni, first);
        } else {
            func.layout.append_inst(ni, b);
        }
        nv
    };
    let mut supply: FxHashMap<(Block, Value), Value> = FxHashMap::default();
    let mut minted: FxHashMap<(Block, Value), Value> = FxHashMap::default();
    for &(d, ref vs) in &plan.steps {
        for &v in vs {
            let ty = func.dfg.value_type(v);
            let p = func.dfg.append_block_param(d, ty);
            supply.insert((d, v), p);
        }
        // Arm-constant uses: mint the `iconst` at `d`'s top and let the
        // use-rewrite below swap `v` for it; record it in `minted` too
        // so `d`'s outgoing edges reuse the same constant.
        for (&(b, v), &c) in &plan.use_consts {
            if b == d {
                let ty = func.dfg.value_type(v);
                let nv = mint(func, d, ty, c);
                supply.insert((d, v), nv);
                minted.insert((d, v), nv);
            }
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
        let args: Vec<Value> = vs
            .iter()
            .map(|&v| {
                if p == r || plan.direct.contains(&(p, v)) {
                    v
                } else if let Some(&c) = plan.consts.get(&(p, v)) {
                    *minted.entry((p, v)).or_insert_with(|| {
                        let ty = func.dfg.value_type(v);
                        let ni = func.dfg.make_inst(InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            imm: c.into(),
                        });
                        func.dfg.make_inst_results(ni, ty);
                        let nv = func.dfg.inst_results(ni)[0];
                        if let Some(first) = func.layout.first_inst(p) {
                            func.layout.insert_inst(ni, first);
                        } else {
                            func.layout.append_inst(ni, p);
                        }
                        nv
                    })
                } else {
                    supply[&(p, v)]
                }
            })
            .collect();
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[t]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di];
        for arg in args {
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
            store: pliron_cfg::env_is("PLIRON_TAILDUP_STORE", "1"),
            store_icmp: pliron_cfg::env_is("PLIRON_TAILDUP_STOREICMP", "1"),
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
    if pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
    {
        tracing::debug!(target: "pliron::taildup", "taildup: rets {rets:?} (fn {})", func.name);
    }
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
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
                && rets.contains(&r)
            {
                tracing::debug!(target: "pliron::taildup", "taildup: edge {p}->{r} di={di} latch={} (fn {})", latches[di], func.name);
            }
            let latch = latches[di];
            // When the second edge's latch clone will take the fallthrough
            // slot anyway, an edge-0 clone is pure duplication — skip it.
            if latch && di == 0 && latches.get(1) == Some(&true) {
                continue;
            }
            // Brif-edge clones only pay when an arg is a constant the cloned
            // body can fold — otherwise it's pure dispatch-chain duplication.
            let arg_const = dest.args(&func.dfg.value_lists).any(|a| {
                matches!(a, BlockArg::Value(v)
                    if func.dfg.resolved_value_def(v).inst()
                        .is_some_and(|i| func.dfg.insts[i].opcode() == Opcode::Iconst))
            });
            let folds = !has_store(func, r)
                || cond_param(func, r, opts.store_icmp).is_some_and(|k| {
                    matches!(dest.args(&func.dfg.value_lists).nth(k), Some(BlockArg::Value(v))
                        if func.dfg.resolved_value_def(v).inst()
                            .is_some_and(|i| func.dfg.insts[i].opcode() == Opcode::Iconst))
                });
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
                && rets.contains(&r)
                && (is_brif && !latch && !arg_const || !folds)
            {
                tracing::debug!(target: "pliron::taildup", "taildup: skip {p}->{r} edge {di}: no-fold arg_const={arg_const} folds={folds} (fn {})", func.name);
            }
            // A candidate with a store (`Opts::store`) is copied only along
            // an edge that passes a constant for its branch condition.
            if is_brif && !latch && !arg_const || !folds {
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
                if pliron_cfg::tracing_on()
                    && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
                {
                    tracing::debug!(target: "pliron::taildup", "taildup: skip {p}->{r} edge {di}: uses-r-defs (fn {})", func.name);
                }
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
                if pliron_cfg::tracing_on()
                    && tracing::enabled!(target: "pliron::taildup", tracing::Level::DEBUG)
                {
                    tracing::debug!(target: "pliron::taildup", "taildup: skip {p}->{r} edge {di}: arg-in-candidate (fn {})", func.name);
                }
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
                        // edge args (or a per-block remat): publish that
                        // into `uses`/`sites` so `local_values` fast-passes
                        // the remaining edges and the arm above sees only
                        // genuinely new sites.
                        let plan = repairs[&r].as_ref().unwrap();
                        let repaired_vs: Vec<Value> = plan
                            .steps
                            .iter()
                            .flat_map(|(_, vs)| vs.iter().copied())
                            .chain(plan.remats.iter().map(|&(_, v)| v))
                            .collect();
                        for v in repaired_vs {
                            uses.insert(v, Some(r));
                            if let Some(s) = sites.as_mut() {
                                s.insert(v, vec![r]);
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
    use cranelift_codegen::ir::{AbiParam, Signature, UserFuncName};
    use cranelift_codegen::ir::{Function, InstBuilder, MemFlagsData};
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
                store: false,
                store_icmp: false,
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

    /// `Opts::store_icmp`: a merge storing a flag then branching on
    /// `icmp tag_param, const` duplicates along edges that pass a
    /// constant for the tag — the clone's compare folds (lol_html's
    /// `finish_tag_name`: `s.is_end = false` sits in the `Option<Fb>`
    /// merge ahead of `if let Some(..)`). Without the opt the store
    /// keeps the block out of candidacy: the `brif` is not on a bare
    /// parameter.
    #[test]
    fn store_icmp_param_clone_folds() {
        let build = || {
            let mut sig = Signature::new(CallConv::Fast);
            sig.params.push(AbiParam::new(types::I64));
            sig.params.push(AbiParam::new(types::I8));
            sig.returns.push(AbiParam::new(types::I64));
            let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);

            let entry = func.dfg.make_block();
            let none_arm = func.dfg.make_block();
            let some_arm = func.dfg.make_block();
            let merge = func.dfg.make_block();
            let ret_a = func.dfg.make_block();
            let ret_b = func.dfg.make_block();
            for b in [entry, none_arm, some_arm, merge, ret_a, ret_b] {
                func.layout.append_block(b);
            }
            let v0 = func.dfg.append_block_param(entry, types::I64);
            let v9 = func.dfg.append_block_param(entry, types::I8);

            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            cur.ins().brif(v9, none_arm, &[], some_arm, &[]);

            // None edge: a constant for the tag parameter — a clone of
            // the merge folds `icmp -1, -1` and jumps straight to ret_a.
            let mut cur = FuncCursor::new(&mut func).at_bottom(none_arm);
            let w = cur.ins().iconst(types::I64, 255);
            let z = cur.ins().iconst(types::I64, 0);
            let m = cur.ins().iconst(types::I8, -1);
            cur.ins()
                .jump(merge, &[w.into(), z.into(), m.into()]);

            // Some edge: the tag arrives as a live value — no constant,
            // no clone.
            let mut cur = FuncCursor::new(&mut func).at_bottom(some_arm);
            cur.ins()
                .jump(merge, &[v0.into(), v0.into(), v9.into()]);

            let p0 = func.dfg.append_block_param(merge, types::I64);
            let p1 = func.dfg.append_block_param(merge, types::I64);
            let tag = func.dfg.append_block_param(merge, types::I8);
            let mut cur = FuncCursor::new(&mut func).at_bottom(merge);
            let zero = cur.ins().iconst(types::I8, 0);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), zero, v0, 0);
            let one = cur.ins().iconst(types::I8, -1);
            let c = cur.ins().icmp(IntCC::Equal, tag, one);
            cur.ins()
                .brif(c, ret_a, &[p0.into()], ret_b, &[p1.into()]);

            let r0 = func.dfg.append_block_param(ret_a, types::I64);
            let mut cur = FuncCursor::new(&mut func).at_bottom(ret_a);
            cur.ins().return_(&[r0]);
            let r1 = func.dfg.append_block_param(ret_b, types::I64);
            let mut cur = FuncCursor::new(&mut func).at_bottom(ret_b);
            cur.ins().return_(&[r1]);
            (func, none_arm, some_arm, merge)
        };

        // Without the opt the store pins the merge: nothing clones.
        let (mut func, ..) = build();
        let n = run_opts(
            &mut func,
            &FxHashMap::default(),
            Opts {
                brif: true,
                escape: true,
                edge: true,
                store: true,
                store_icmp: false,
            },
        );
        assert_eq!(n, 0, "store merge must stay put:\n{}", func.display());

        let (mut func, none_arm, some_arm, merge) = build();
        let n = run_opts(
            &mut func,
            &FxHashMap::default(),
            Opts {
                brif: true,
                escape: true,
                edge: true,
                store: true,
                store_icmp: true,
            },
        );
        assert_eq!(n, 1, "only the constant edge clones:\n{}", func.display());
        // The clone replaced none_arm's jump with the merge body: the
        // store plus `icmp -1, -1` (folded by the usual compare fold
        // later) and the branch itself.
        let t = func.layout.last_inst(none_arm).unwrap();
        assert!(
            matches!(func.dfg.insts[t], InstructionData::Brif { .. }),
            "none_arm must end in the cloned brif:\n{}",
            func.display()
        );
        // The variable edge still enters the merge.
        let t = func.layout.last_inst(some_arm).unwrap();
        let InstructionData::Jump { .. } = func.dfg.insts[t] else {
            panic!("some_arm's jump changed:\n{}", func.display());
        };
        assert!(
            func.layout.is_block_inserted(merge),
            "merge keeps its live edge:\n{}",
            func.display()
        );

        let flags = cranelift_codegen::settings::Flags::new(
            cranelift_codegen::settings::builder(),
        );
        cranelift_codegen::verify_function(&func, &flags).unwrap();
    }

    /// Escape repair through a *transit* chain: the merge's params feed
    /// a panic block reached only through intermediates that never use
    /// them (`finish_tag_name`'s cold enum asserts: tag/payload reaching
    /// `block25`/`block173` via `block13`'s unwind chain). The
    /// intermediates get params forwarding the binding; without that the
    /// repair bails and the merge never clones.
    #[test]
    fn escape_repair_params_transit_blocks() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I8));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);

        let entry = func.dfg.make_block();
        let none_arm = func.dfg.make_block();
        let some_arm = func.dfg.make_block();
        let merge = func.dfg.make_block();
        let arm_t = func.dfg.make_block();
        let transit = func.dfg.make_block();
        let panic_use = func.dfg.make_block();
        let arm_e = func.dfg.make_block();
        for b in [
            entry, none_arm, some_arm, merge, arm_t, transit, panic_use, arm_e,
        ] {
            func.layout.append_block(b);
        }
        let v0 = func.dfg.append_block_param(entry, types::I64);
        let v9 = func.dfg.append_block_param(entry, types::I8);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        cur.ins().brif(v9, none_arm, &[], some_arm, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(none_arm);
        let w = cur.ins().iconst(types::I64, 255);
        let z = cur.ins().iconst(types::I64, 0);
        let m = cur.ins().iconst(types::I8, -1);
        cur.ins().jump(merge, &[w.into(), z.into(), m.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(some_arm);
        cur.ins().jump(merge, &[v0.into(), v0.into(), v9.into()]);

        let p0 = func.dfg.append_block_param(merge, types::I64);
        let p1 = func.dfg.append_block_param(merge, types::I64);
        let tag = func.dfg.append_block_param(merge, types::I8);
        let mut cur = FuncCursor::new(&mut func).at_bottom(merge);
        let zero = cur.ins().iconst(types::I8, 0);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), zero, v0, 0);
        let one = cur.ins().iconst(types::I8, -1);
        let c = cur.ins().icmp(IntCC::Equal, tag, one);
        cur.ins().brif(c, arm_t, &[], arm_e, &[]);

        // `arm_t`'s cone: two intermediates that never touch `p0`, then
        // a cold use — the panic block reads the escapee in its body.
        let mut cur = FuncCursor::new(&mut func).at_bottom(arm_t);
        cur.ins().jump(transit, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(transit);
        cur.ins().jump(panic_use, &[]);
        let mut cur = FuncCursor::new(&mut func).at_bottom(panic_use);
        cur.ins()
            .store(MemFlagsData::new().with_notrap(), p0, v0, 8);
        cur.ins().return_(&[p0]);

        // `arm_e` uses `p1` in its body — a plain use-block on the other arm.
        let mut cur = FuncCursor::new(&mut func).at_bottom(arm_e);
        cur.ins().return_(&[p1]);

        let n = run_opts(
            &mut func,
            &FxHashMap::default(),
            Opts {
                brif: true,
                escape: true,
                edge: true,
                store: true,
                store_icmp: true,
            },
        );
        assert_eq!(n, 1, "the constant edge must clone:\n{}", func.display());

        // The escape was bound by param'ing the intermediates: `arm_t`
        // and `transit` each forward `p0`, `panic_use` reads its param.
        assert_eq!(
            func.dfg.num_block_params(arm_t),
            1,
            "arm_t forwards the escapee:\n{}",
            func.display()
        );
        assert_eq!(
            func.dfg.num_block_params(transit),
            1,
            "transit forwards the escapee:\n{}",
            func.display()
        );
        assert_eq!(
            func.dfg.num_block_params(panic_use),
            1,
            "panic_use takes the binding:\n{}",
            func.display()
        );

        let flags = cranelift_codegen::settings::Flags::new(
            cranelift_codegen::settings::builder(),
        );
        cranelift_codegen::verify_function(&func, &flags).unwrap();
    }
}

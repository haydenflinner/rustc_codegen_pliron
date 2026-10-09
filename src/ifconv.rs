//! If-conversion of select diamonds (LLVM SimplifyCFG's FoldTwoEntryPHI):
//! `brif c, T, E` where `T` and `E` both reach the same merge block `M` —
//! either directly or through a forwarder block carrying only pure
//! (speculatable) insts — collapses to cloned side computations + `select`s
//! in `b` plus a direct `jump M`.
//!
//! Values a side uses that aren't its own params are safe to reuse in `b`:
//! they dominate the side's terminator, and since `b` is a predecessor and
//! any path to `b` extends over the edge into the side, they dominate `b`'s
//! terminator too. Pure side insts (icmp, arithmetic — no loads, stores,
//! traps, calls) are speculated into `b`; the jump-thread fixpoint then
//! collapses the residue (dead side blocks, single-pred merge params).
//!
//! `PLIRON_IFCONV=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
    ValueDef, types,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

/// One branch arm resolved against a candidate merge point.
struct Arm {
    /// Merge target the arm's jump reaches (`side`'s jump dest), or the
    /// side block itself for a direct edge.
    m: Block,
    /// Values to feed `M`'s params, using pre-clone side-local results.
    args: Vec<Value>,
    /// The arm's block, `None` for a direct edge to `M`.
    side: Option<Block>,
    /// Pure insts in `side` (program order) to speculate into `b`.
    clone: Vec<Inst>,
    /// `side` param -> `b`'s edge arg.
    psub: Vec<(Value, Value)>,
}

/// `tgt` reached over `edge`: a pure-compute forwarder `jump M`, or `tgt`
/// itself when `edge` is the (already-final) argument list into it.
fn resolve_arm(func: &Function, tgt: Block, edge: &[BlockArg]) -> Option<Arm> {
    let insts: Vec<Inst> = func.layout.block_insts(tgt).collect();
    if let Some((&jmp, body)) = insts.split_last()
        && let InstructionData::Jump { destination, .. } = func.dfg.insts[jmp]
    {
        let m = destination.block(&func.dfg.value_lists);
        let params = func.dfg.block_params(tgt);
        if body.len() <= 4 && edge.len() == params.len() {
            // All body insts must be speculatable; operands may be side
            // params, earlier side results, or values dominating `tgt`
            // (which then dominate `b`'s terminator — see file docs).
            let mut psub = Vec::with_capacity(params.len());
            let mut ok = edge.iter().all(|a| matches!(a, BlockArg::Value(_)));
            for (j, &p) in params.iter().enumerate() {
                if !ok {
                    break;
                }
                if let BlockArg::Value(v) = edge[j] {
                    psub.push((p, func.dfg.resolve_aliases(v)));
                }
            }
            for &i in body {
                ok &= crate::jumpthread::pure_op(func, i);
            }
            if ok {
                let mut args = Vec::new();
                for a in destination.args(&func.dfg.value_lists) {
                    let BlockArg::Value(v) = a else {
                        return None;
                    };
                    args.push(func.dfg.resolve_aliases(v));
                }
                return Some(Arm {
                    m,
                    args,
                    side: Some(tgt),
                    clone: body.to_vec(),
                    psub,
                });
            }
        }
    }
    // Direct edge into the merge block.
    if edge.iter().all(|a| matches!(a, BlockArg::Value(_))) {
        return Some(Arm {
            m: tgt,
            args: edge
                .iter()
                .map(|a| match a {
                    BlockArg::Value(v) => func.dfg.resolve_aliases(*v),
                    _ => unreachable!(),
                })
                .collect(),
            side: None,
            clone: Vec::new(),
            psub: Vec::new(),
        });
    }
    None
}

/// `select` lowers on our targets only for ≤64-bit scalars (aarch64 can't
/// legalize an i128 select, and a scalar condition can't pick vectors).
fn select_ok(func: &Function, a: Value, b: Value) -> bool {
    let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
    let (ta, tb) = (func.dfg.value_type(a), func.dfg.value_type(b));
    ta == tb
        && ta.lane_count() == 1
        && (ta.is_int() && ta.bits() <= 64 || matches!(ta, types::F32 | types::F64))
}

/// Clone `arm`'s insts into `b` before `term`, remapping side params and
/// side-local results, then return the arm's final merge args.
fn emit_arm(
    func: &mut Function,
    term: Inst,
    arm: &Arm,
) -> Vec<Value> {
    let mut map: FxHashMap<Value, Value> = arm.psub.iter().copied().collect();
    {
        let mut pos = FuncCursor::new(func).at_inst(term);
        for &i in &arm.clone {
            let ni = pos.func.dfg.clone_inst(i);
            let vals: Vec<Value> = pos
                .func
                .dfg
                .inst_args(ni)
                .iter()
                .map(|&a| {
                    let a = pos.func.dfg.resolve_aliases(a);
                    map.get(&a).copied().unwrap_or(a)
                })
                .collect();
            pos.func.dfg.overwrite_inst_values(ni, vals.into_iter());
            pos.insert_inst(ni);
            let old: Vec<Value> = pos.func.dfg.inst_results(i).to_vec();
            let new: Vec<Value> = pos.func.dfg.inst_results(ni).to_vec();
            for (o, nr) in old.into_iter().zip(new) {
                map.insert(pos.func.dfg.resolve_aliases(o), nr);
            }
        }
    }
    arm.args
        .iter()
        .map(|&v| map.get(&v).copied().unwrap_or(v))
        .collect()
}

/// The `b -> m` edge skips whatever the arms routed through. A use inside
/// `m`'s dominated subtree of a value defined outside it (a removed-param
/// alias to an arm-local def is the live case) is only safe when the def
/// still dominates `term`. `m`'s own params are fine: `sargs` supplies them.
fn edge_ok(
    func: &Function,
    domtree: &DominatorTree,
    m: Block,
    mset: &FxHashSet<Block>,
    term: Inst,
) -> bool {
    mset.iter().all(|&x| {
        func.layout.block_insts(x).all(|i| {
            func.dfg.inst_values(i).all(|v| {
                match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
                    ValueDef::Param(d, _) => {
                        d == m || mset.contains(&d) || domtree.block_dominates(d, term_block(func, term))
                    }
                    ValueDef::Result(di, _) => {
                        mset.contains(&func.layout.inst_block(di).unwrap_or(m))
                            || domtree.dominates(di, term, &func.layout)
                    }
                    _ => true,
                }
            })
        })
    })
}

fn term_block(func: &Function, term: Inst) -> Block {
    func.layout.inst_block(term).unwrap()
}

/// `br_table` if-conversion: every target resolving (through pure
/// forwarders) to one merge `M` with per-arm args collapses to cloned side
/// computations plus `select(hit, arm_val, sel)` chains in `b`. The mask for
/// an arm is `or(icmp eq idx, k)` over its table indices — index sets are
/// disjoint, so select order doesn't matter. Keeps switch-idiom loops
/// (`matches!` filters) flat enough for loopvec.
fn conv_table(
    func: &mut Function,
    cfg: &mut ControlFlowGraph,
    domtree: &mut DominatorTree,
    mset_cache: &mut FxHashMap<Block, FxHashSet<Block>>,
    dirty: &mut bool,
    b: Block,
    term: Inst,
) -> bool {
    let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[term] else {
        return false;
    };
    let idx = func.dfg.resolve_aliases(arg);
    let ity0 = func.dfg.value_type(idx);
    if !ity0.is_int() || ity0.is_vector() {
        return false;
    }
    // `all_branches` lists the default first; entry i is index i+1.
    let dests: Vec<BlockCall> = func
        .dfg
        .jump_tables
        .get(table)
        .unwrap()
        .all_branches()
        .to_vec();
    let nidx = dests.len() - 1;
    // Compare at the pre-extension width: `extend(x) == k` for
    // `0 <= k < 2^(bits-1)` is bit-exact, and narrow compares vectorize.
    let (idx, ity) = match func.dfg.value_def(idx) {
        ValueDef::Result(i, _)
            if nidx < 128
                && let InstructionData::Unary {
                    opcode: Opcode::Uextend | Opcode::Sextend,
                    arg,
                } = func.dfg.insts[i] =>
        {
            (func.dfg.resolve_aliases(arg), func.dfg.value_type(func.dfg.resolve_aliases(arg)))
        }
        _ => (idx, ity0),
    };
    let mut arms: Vec<Arm> = Vec::new();
    let mut arm_of: Vec<usize> = Vec::with_capacity(dests.len());
    'dests: for bc in &dests {
        let tgt = bc.block(&func.dfg.value_lists);
        if tgt == b {
            return false;
        }
        let edge: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
        let Some(a) = resolve_arm(func, tgt, &edge) else {
            return false;
        };
        if a.m == b {
            return false;
        }
        for (ai, prev) in arms.iter().enumerate() {
            if prev.m == a.m && prev.side == a.side && prev.args == a.args {
                arm_of.push(ai);
                continue 'dests;
            }
        }
        arm_of.push(arms.len());
        arms.push(a);
    }
    let m = arms[0].m;
    if arms.iter().any(|a| a.m != m || a.side == Some(m)) {
        return false;
    }
    let np = func.dfg.num_block_params(m);
    if arms.iter().any(|a| a.args.len() != np) {
        return false;
    }
    // Bounds before mutating: few arms, few total covered indices.
    if arms.len() > 8 || nidx > 64 {
        return false;
    }
    let dflt = arm_of[0];
    let covered = |ai: usize| -> Vec<usize> {
        (0..nidx).filter(|&i| arm_of[1 + i] == ai).collect()
    };
    let total_hits: usize = (0..arms.len())
        .filter(|&ai| ai != dflt)
        .map(|ai| covered(ai).len())
        .sum();
    if total_hits > 12 {
        return false;
    }
    // Per-param select types: every differing override must be selectable
    // against the default arm's value.
    for j in 0..np {
        let dv = arms[dflt].args[j];
        for ai in 0..arms.len() {
            if ai == dflt || covered(ai).is_empty() {
                continue;
            }
            let v = arms[ai].args[j];
            if v != dv && !select_ok(func, v, dv) {
                return false;
            }
        }
    }
    if *dirty {
        cfg.compute(func);
        domtree.compute(func, &cfg);
        mset_cache.clear();
        *dirty = false;
    }
    // The synthesized values live in `b`; every path into `m` must come
    // from this table.
    let sides: FxHashSet<Block> = arms.iter().filter_map(|a| a.side).collect();
    let ok = cfg.pred_iter(m).all(|p| {
        let pb = func.layout.inst_block(p.inst).unwrap_or(m);
        pb == b || sides.contains(&pb)
    });
    if !ok {
        return false;
    }
    let mset = mset_cache.entry(m).or_insert_with(|| {
        func.layout
            .blocks()
            .filter(|&x| domtree.block_dominates(m, x))
            .collect()
    });
    if !edge_ok(func, domtree, m, mset, term) {
        return false;
    }
    // Arm args that aren't cloned side results or side params must dominate
    // `b`'s terminator — a value legal inside a side block need not be if
    // the side is reachable without `b`.
    for a in &arms {
        let Some(s) = a.side else { continue };
        let local: FxHashSet<Value> = func
            .dfg
            .block_params(s)
            .iter()
            .copied()
            .chain(
                func.layout
                    .block_insts(s)
                    .flat_map(|i| func.dfg.inst_results(i).iter().copied()),
            )
            .collect();
        for &v in &a.args {
            let v = func.dfg.resolve_aliases(v);
            if local.contains(&v) {
                continue;
            }
            let dom = match func.dfg.value_def(v) {
                ValueDef::Result(di, _) => domtree.dominates(di, term, &func.layout),
                ValueDef::Param(db, _) => domtree.block_dominates(db, b),
                _ => false,
            };
            if !dom {
                return false;
            }
        }
    }
    *dirty = true;
    let avals: Vec<Vec<Value>> = arms.iter().map(|a| emit_arm(func, term, a)).collect();
    let mut sargs: Vec<BlockArg> = Vec::with_capacity(np);
    for j in 0..np {
        let mut sel = avals[dflt][j];
        for ai in 0..arms.len() {
            if ai == dflt {
                continue;
            }
            let v = avals[ai][j];
            if v == sel {
                continue;
            }
            let s = covered(ai);
            if s.is_empty() {
                continue;
            }
            let mut pos = FuncCursor::new(func).at_inst(term);
            let mut hit = {
                let k = pos.ins().iconst(ity, s[0] as i64);
                pos.ins().icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, idx, k)
            };
            for &k in &s[1..] {
                let k = pos.ins().iconst(ity, k as i64);
                let c = pos.ins().icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, idx, k);
                hit = pos.ins().bor(hit, c);
            }
            sel = pos.ins().select(hit, v, sel);
        }
        sargs.push(BlockArg::Value(sel));
    }
    let dfg = &mut func.dfg;
    let bc = BlockCall::new(m, sargs.iter().copied(), &mut dfg.value_lists);
    dfg.insts[term] = InstructionData::Jump {
        opcode: Opcode::Jump,
        destination: bc,
    };
    if std::env::var_os("PLIRON_IFCONV_DEBUG").is_some() {
        eprintln!("ifconv {b} br_table -> jump {m} (arms {})", arms.len());
    }
    true
}

pub fn run(func: &mut Function) -> usize {
    let mut cfg = ControlFlowGraph::with_function(func);
    let mut domtree = DominatorTree::with_function(func, &cfg);
    let mut mset_cache: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    let mut dirty = false;
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg: cond, .. } = func.dfg.insts[term] else {
            if matches!(func.dfg.insts[term], InstructionData::BranchTable { .. })
                && conv_table(func, &mut cfg, &mut domtree, &mut mset_cache, &mut dirty, b, term)
            {
                n += 1;
            }
            continue;
        };
        let dests = func.dfg.insts[term]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .to_vec();
        let [t_bc, e_bc] = dests[..] else {
            continue;
        };
        let (t, e) = (
            t_bc.block(&func.dfg.value_lists),
            e_bc.block(&func.dfg.value_lists),
        );
        if t == b || e == b {
            continue;
        }
        let t_edge: Vec<BlockArg> = t_bc.args(&func.dfg.value_lists).collect();
        let e_edge: Vec<BlockArg> = e_bc.args(&func.dfg.value_lists).collect();
        let Some(ta) = resolve_arm(func, t, &t_edge) else {
            continue;
        };
        let Some(ea) = resolve_arm(func, e, &e_edge) else {
            continue;
        };
        if ta.m != ea.m || ta.m == b {
            continue;
        }
        // A side can't be its own merge (self-loop forwarder).
        if ta.m == t && ta.side == Some(t) || ta.m == e && ea.side == Some(e) {
            continue;
        }
        let m = ta.m;
        // A non-forwarder arm must target M itself (its edge args are the
        // merge args); both arms direct is `brif c, M, M`.
        if (ta.side.is_none() && t != m) || (ea.side.is_none() && e != m) {
            continue;
        }
        if ta.args.len() != ea.args.len() || ta.args.len() != func.dfg.num_block_params(m) {
            continue;
        }
        // Type-check before mutating (clones preserve types).
        if !ta
            .args
            .iter()
            .zip(&ea.args)
            .all(|(&x, &y)| x == y || select_ok(func, x, y))
        {
            continue;
        }
        if dirty {
            cfg.compute(func);
            domtree.compute(func, &cfg);
            mset_cache.clear();
            dirty = false;
        }
        let mset = mset_cache.entry(m).or_insert_with(|| {
            func.layout
                .blocks()
                .filter(|&x| domtree.block_dominates(m, x))
                .collect()
        });
        if !edge_ok(func, &domtree, m, mset, term) {
            continue;
        }
        dirty = true;
        let targs = emit_arm(func, term, &ta);
        let eargs = emit_arm(func, term, &ea);
        let cond = func.dfg.resolve_aliases(cond);
        let mut sargs: Vec<BlockArg> = Vec::with_capacity(targs.len());
        for (x, y) in targs.into_iter().zip(eargs) {
            if x == y {
                sargs.push(BlockArg::Value(x));
                continue;
            }
            let ty = func.dfg.value_type(x);
            let sel = func.dfg.make_inst(InstructionData::Ternary {
                opcode: Opcode::Select,
                args: [cond, x, y],
            });
            func.dfg.make_inst_results(sel, ty);
            func.layout.insert_inst(sel, term);
            sargs.push(BlockArg::Value(func.dfg.first_result(sel)));
        }
        let dfg = &mut func.dfg;
        let bc = BlockCall::new(m, sargs.iter().copied(), &mut dfg.value_lists);
        dfg.insts[term] = InstructionData::Jump {
            opcode: Opcode::Jump,
            destination: bc,
        };
        if std::env::var_os("PLIRON_IFCONV_DEBUG").is_some() {
            eprintln!("ifconv {b} -> jump {m} (arms {t:?} {e:?})");
        }
        n += 1;
    }
    n
}

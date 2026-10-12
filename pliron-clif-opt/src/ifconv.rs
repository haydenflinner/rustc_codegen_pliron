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
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
    ValueDef, types,
};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::{FxHashMap, FxHashSet};

/// One branch arm resolved against a candidate merge point.
struct Arm {
    /// Merge target the arm's final jump reaches (`side`'s jump dest), or
    /// the target itself for a direct edge.
    m: Block,
    /// Values to feed `M`'s params, using pre-clone side-local results.
    args: Vec<Value>,
    /// The block that edges into `M` — the last forwarder hop; `None` for
    /// a direct `b -> M` edge.
    side: Option<Block>,
    /// Every forwarder hop traversed, in order (empty for a direct arm).
    chain: Vec<Block>,
    /// Pure insts across the whole chain (program order) to speculate
    /// into `b`.
    clone: Vec<Inst>,
    /// Chain param -> incoming edge arg (per hop, in order).
    psub: Vec<(Value, Value)>,
    /// `chain` went beyond one hop — gated by `flat_loop`: folding a
    /// chain inside a loop is only profitable when the loop ends up
    /// branch-free (vectorizer-bound); inside a still-branchy loop it
    /// turns predictable branches into always-paid selects.
    chained: bool,
}

/// Speculation caps for a pure arm body. The narrow cap is the
/// historical bound; the wide cap exists for nested range-check
/// predicate diamonds (`is_ascii_alphanumeric` &co clone ~3 compares
/// per level) and is only profitable when the diamond merges a
/// *bool* — see `bool_merge` below.
const SPEC_NARROW: usize = 4;
const SPEC_WIDE: usize = 8;

/// `iconst` value bits, if `v` is one.
fn iconst_bits(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    if let ValueDef::Result(i, _) = func.dfg.value_def(v)
        && let InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } = func.dfg.insts[i]
    {
        return Some(imm.bits());
    }
    None
}

/// A value carrying a 0/1 (mod-2) result: integer/float compares, bool
/// constants, and bitwise/select/extend trees over them — the
/// `filter(pred)` predicate shape loopvec vectorizes. `nonconst` is set
/// when a real compare (not just constants) sits in the tree: the UTF-8
/// `Option<char>` dispatch also merges a 0/1 discriminant alongside its
/// pointer/char args, and speculating its decode arm to feed selects
/// loses to the (predictable) width branch.
fn bool_tree(func: &Function, v: Value, depth: usize, nonconst: &mut bool) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if depth == 0 {
        return false;
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return false;
    };
    match func.dfg.insts[i] {
        InstructionData::IntCompare { .. } | InstructionData::FloatCompare { .. } => {
            *nonconst = true;
            true
        }
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => matches!(imm.bits(), 0 | 1),
        InstructionData::Binary {
            opcode: Opcode::Band | Opcode::Bor | Opcode::Bxor,
            args,
        } => {
            args.iter().all(|&a| bool_tree(func, a, depth - 1, nonconst))
                // `band x, 1` is a bool for any `x`.
                || func.dfg.insts[i].opcode() == Opcode::Band
                    && args.iter().any(|&a| iconst_bits(func, a) == Some(1))
        }
        InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } => args[1..]
            .iter()
            .all(|&a| bool_tree(func, a, depth - 1, nonconst)),
        InstructionData::Unary {
            opcode: Opcode::Ireduce | Opcode::Uextend | Opcode::Sextend,
            arg,
        } => bool_tree(func, arg, depth - 1, nonconst),
        _ => false,
    }
}

/// Does the diamond's merge carry a *differing* bool param — at least
/// one arm's arg a real compare/bool tree? Then speculating a wide arm
/// plausibly feeds a count/conditional reduction the vectorizer wants
/// flat; otherwise (data merges like the UTF-8 dispatch) a >`SPEC_NARROW`
/// arm would convert a predictable branch into always-paid work.
fn bool_merge(func: &Function, arms: &[&Arm]) -> bool {
    let Some(first) = arms.first() else {
        return false;
    };
    for j in 0..first.args.len() {
        let mut nc = false;
        let mut ok = true;
        let mut diff = false;
        for a in arms {
            if !bool_tree(func, a.args[j], 8, &mut nc) {
                ok = false;
                break;
            }
            diff |= a.args[j] != first.args[j];
        }
        if ok && diff && nc {
            return true;
        }
    }
    false
}

/// `tgt` reached over `edge`: a chain of pure-compute forwarders ending
/// in `jump M` (a single-pred flag-fold leaves several hop blocks per
/// arm), or `tgt` itself when `edge` is the (already-final) argument
/// list into it. Each hop's pure body joins `clone` and its params join
/// `psub`; intermediate results remap through `emit_arm`'s value map.
fn resolve_arm(func: &Function, tgt: Block, edge: &[BlockArg]) -> Option<Arm> {
    let mut cur = tgt;
    let mut cur_edge: Vec<BlockArg> = edge.to_vec();
    let mut chain = Vec::new();
    let mut clone = Vec::new();
    let mut psub = Vec::new();
    while chain.len() < 8 {
        let insts: Vec<Inst> = func.layout.block_insts(cur).collect();
        let Some((&jmp, body)) = insts.split_last() else {
            break;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[jmp] else {
            break;
        };
        let params = func.dfg.block_params(cur);
        // The inst cap bounds total speculation across the chain; arms
        // deeper than SPEC_NARROW additionally need `bool_merge` at the
        // conversion site.
        if cur_edge.len() != params.len()
            || clone.len() + body.len() > SPEC_WIDE
            || !cur_edge.iter().all(|a| matches!(a, BlockArg::Value(_)))
            || !body.iter().all(|&i| crate::jumpthread::pure_op(func, i))
        {
            break;
        }
        for (j, &p) in params.iter().enumerate() {
            let BlockArg::Value(v) = cur_edge[j] else {
                unreachable!()
            };
            psub.push((p, func.dfg.resolve_aliases(v)));
        }
        clone.extend_from_slice(body);
        chain.push(cur);
        cur_edge = destination.args(&func.dfg.value_lists).collect();
        cur = destination.block(&func.dfg.value_lists);
    }
    if chain.is_empty() {
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
                chain,
                clone,
                psub,
                chained: false,
            });
        }
        return None;
    }
    let map: FxHashMap<Value, Value> = psub.iter().copied().collect();
    let mut args = Vec::new();
    for a in &cur_edge {
        let BlockArg::Value(v) = a else {
            return None;
        };
        // Resolve through the chain's param substitutions: the final edge
        // args are the last hop's own params, which are identical for every
        // arm sharing that hop. Dedup must see what each arm actually feeds
        // `m` — the value injected at its first hop.
        args.push(thru(&map, func.dfg.resolve_aliases(*v)));
    }
    Some(Arm {
        m: cur,
        args,
        side: chain.last().copied(),
        chained: chain.len() > 1,
        chain,
        clone,
        psub,
    })
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

/// Chase `v` through the clone/psub map: a later hop's param can map to
/// an earlier hop's result, which itself renames when cloned.
fn thru(map: &FxHashMap<Value, Value>, mut v: Value) -> Value {
    for _ in 0..16 {
        match map.get(&v) {
            Some(&w) if w != v => v = w,
            _ => return v,
        }
    }
    v
}

/// Clone `arm`'s insts into `b` before `term`, remapping side params and
/// side-local results, then return the arm's final merge args.
fn emit_arm(
    func: &mut Function,
    term: Inst,
    arm: &Arm,
    derived: &mut FxHashMap<Value, Value>,
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
                    thru(&map, a)
                })
                .collect();
            pos.func.dfg.overwrite_inst_values(ni, vals.into_iter());
            pos.insert_inst(ni);
            let old: Vec<Value> = pos.func.dfg.inst_results(i).to_vec();
            let new: Vec<Value> = pos.func.dfg.inst_results(ni).to_vec();
            for (o, nr) in old.into_iter().zip(new) {
                let o = pos.func.dfg.resolve_aliases(o);
                map.insert(o, nr);
                // A clone of an `inbounds` offset is still inbounds on the
                // renamed base — keep the fact so `NonNull` can see it.
                // `derived` may hold a stale alias tombstone (`Alias` whose
                // dest is the reserved value) — `resolve_aliases` on it
                // indexes the reserved slot, so leave it unpropagated.
                if let Some(&b) = derived.get(&o)
                    && pos
                        .func
                        .dfg
                        .value_alias_dest_for_serialization(b)
                        .is_none_or(|d| d != Value::reserved_value())
                {
                    let b = map
                        .get(&pos.func.dfg.resolve_aliases(b))
                        .copied()
                        .unwrap_or(b);
                    derived.insert(nr, b);
                }
            }
        }
    }
    arm.args.iter().map(|&v| thru(&map, v)).collect()
}

/// The `b -> m` edge skips whatever the arms routed through. A use inside
/// `m`'s dominated subtree of a value defined outside it (a removed-param
/// alias to an arm-local def is the live case) is only safe when the def
/// still dominates `term`. `m`'s own params are fine: `sargs` supplies them.
///
/// A def inside a bypassed `chain` block never qualifies on dominance: the
/// use isn't remapped — `emit_arm` only rewrites `b`'s edge — and the hop
/// keeps binding it, now over a *smaller* edge set. A hop param is a phi
/// over the hop's incoming edges, so dropping `b`'s edge changes the value
/// the use observes even when the hop still dominates it (the loop-header
/// flag case: `rustc_lexer::eat_hexadecimal_digits` lost `has_digits`).
fn edge_ok(
    func: &Function,
    domtree: &DominatorTree,
    m: Block,
    mset: &FxHashSet<Block>,
    chains: &FxHashSet<Block>,
    term: Inst,
) -> bool {
    mset.iter().all(|&x| {
        func.layout.block_insts(x).all(|i| {
            func.dfg
                .inst_values(i)
                .all(|v| match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
                    ValueDef::Param(d, _) => {
                        !chains.contains(&d)
                            && (d == m
                                || mset.contains(&d)
                                || domtree.block_dominates(d, term_block(func, term)))
                    }
                    ValueDef::Result(di, _) => {
                        let db = func.layout.inst_block(di).unwrap_or(m);
                        !chains.contains(&db)
                            && (mset.contains(&db) || domtree.dominates(di, term, &func.layout))
                    }
                    _ => true,
                })
        })
    })
}

fn term_block(func: &Function, term: Inst) -> Block {
    func.layout.inst_block(term).unwrap()
}

/// Blocks reachable from the entry over the current CFG (orphans left by
/// earlier folds aren't).
fn reachable(func: &Function, cfg: &ControlFlowGraph) -> FxHashSet<Block> {
    let mut r = FxHashSet::default();
    let mut st = vec![func.layout.entry_block().unwrap()];
    while let Some(b) = st.pop() {
        if r.insert(b) {
            st.extend(cfg.succ_iter(b));
        }
    }
    r
}

/// Would converting `b`'s diamond leave the enclosing loop branch-free?
/// That's the shape loopvec can take — a flat count/conditional-update
/// kernel. Inside a loop that keeps other interior branches (the UTF-8
/// width decode) speculation only buys always-paid selects. Blocks with
/// no reachable path (dead dispatch arms like `RangeInclusive`'s
/// exhausted-flag branch) don't count.
fn flat_loop(func: &Function, la: &LoopAnalysis, reach: &FxHashSet<Block>, b: Block) -> bool {
    let Some(lp) = la.innermost_loop(b) else {
        return true;
    };
    func.layout.blocks().all(|x| {
        if x == b || !reach.contains(&x) || !la.is_in_loop(x, lp) {
            return true;
        }
        func.layout.block_insts(x).all(|i| {
            let InstructionData::Brif { .. } = func.dfg.insts[i] else {
                return true;
            };
            // An interior brif keeps both destinations in the loop.
            !func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .all(|bc| la.is_in_loop(bc.block(&func.dfg.value_lists), lp))
        })
    })
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
    la: &mut LoopAnalysis,
    reach: &mut FxHashSet<Block>,
    mset_cache: &mut FxHashMap<Block, FxHashSet<Block>>,
    dirty: &mut bool,
    b: Block,
    term: Inst,
    derived: &mut FxHashMap<Value, Value>,
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
            (
                func.dfg.resolve_aliases(arg),
                func.dfg.value_type(func.dfg.resolve_aliases(arg)),
            )
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
    let covered =
        |ai: usize| -> Vec<usize> { (0..nidx).filter(|&i| arm_of[1 + i] == ai).collect() };
    let total_hits: usize = (0..arms.len())
        .filter(|&ai| ai != dflt)
        .map(|ai| covered(ai).len())
        .sum();
    if total_hits > 12 {
        return false;
    }
    // Wide arms are only profitable for bool merges (see `bool_merge`);
    // a data dispatch like the UTF-8 width table keeps its predictable
    // branches. Chained arms are gated by `flat_loop` below instead.
    if arms.iter().all(|a| !a.chained)
        && arms.iter().any(|a| a.clone.len() > SPEC_NARROW)
        && !bool_merge(func, &arms.iter().collect::<Vec<_>>())
    {
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
    // Multi-hop arms only pay off when the fold leaves its loop
    // branch-free (see `flat_loop`).
    if arms.iter().any(|a| a.chained) {
        la.compute(func, cfg, domtree);
        *reach = reachable(func, cfg);
        if !flat_loop(func, la, reach, b) {
            return false;
        }
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
    let chains: FxHashSet<Block> = arms.iter().flat_map(|a| a.chain.iter().copied()).collect();
    let mset = mset_cache.entry(m).or_insert_with(|| {
        func.layout
            .blocks()
            .filter(|&x| domtree.block_dominates(m, x))
            .collect()
    });
    if !edge_ok(func, domtree, m, mset, &chains, term) {
        return false;
    }
    // Arm args that aren't cloned chain results or chain params must
    // dominate `b`'s terminator — a value legal inside a hop block need
    // not be if the hop is reachable without `b`.
    for a in &arms {
        if a.side.is_none() {
            continue;
        }
        let local: FxHashSet<Value> = a
            .chain
            .iter()
            .flat_map(|&s| {
                func.dfg.block_params(s).iter().copied().chain(
                    func.layout
                        .block_insts(s)
                        .flat_map(|i| func.dfg.inst_results(i).iter().copied())
                        .collect::<Vec<_>>(),
                )
            })
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
    let avals: Vec<Vec<Value>> = arms
        .iter()
        .map(|a| emit_arm(func, term, a, derived))
        .collect();
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
                pos.ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, idx, k)
            };
            for &k in &s[1..] {
                let k = pos.ins().iconst(ity, k as i64);
                let c = pos
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, idx, k);
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
    if crate::env_set("PLIRON_IFCONV_DEBUG") {
        eprintln!("ifconv {b} br_table -> jump {m} (arms {})", arms.len());
    }
    true
}

pub fn run(func: &mut Function, derived: &mut FxHashMap<Value, Value>) -> usize {
    let mut cfg = ControlFlowGraph::with_function(func);
    let mut domtree = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    let mut reach: FxHashSet<Block> = FxHashSet::default();
    let mut mset_cache: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    let mut dirty = false;
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg: cond, .. } = func.dfg.insts[term] else {
            if matches!(func.dfg.insts[term], InstructionData::BranchTable { .. })
                && conv_table(
                    func,
                    &mut cfg,
                    &mut domtree,
                    &mut la,
                    &mut reach,
                    &mut mset_cache,
                    &mut dirty,
                    b,
                    term,
                    derived,
                )
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
        // The merge can't sit inside an arm's forwarder chain (a self
        // loop routed through hops).
        if ta.chain.contains(&ta.m) || ea.chain.contains(&ea.m) {
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
        // A >`SPEC_NARROW` arm pays its speculation every iteration; only
        // fold it when the merge carries a real predicate bool (the
        // `filter(pred)` diamonds loopvec wants flat). The UTF-8 width
        // dispatch merges const flags + pointers instead — its branches
        // are predictable and must stay branches (x64: ~2x chars hit).
        // A multi-hop arm is instead gated by `flat_loop`: folding is
        // only profitable when the loop ends up branch-free (the
        // count-if accumulator merges a select, not a bool tree).
        if !ta.chained
            && !ea.chained
            && (ta.clone.len() > SPEC_NARROW || ea.clone.len() > SPEC_NARROW)
            && !bool_merge(func, &[&ta, &ea])
        {
            continue;
        }
        if dirty {
            cfg.compute(func);
            domtree.compute(func, &cfg);
            mset_cache.clear();
            dirty = false;
        }
        // Multi-hop arms only pay off when the fold leaves its loop
        // branch-free (see `flat_loop`).
        if ta.chained || ea.chained {
            la.compute(func, &cfg, &domtree);
            reach = reachable(func, &cfg);
            if !flat_loop(func, &la, &reach, b) {
                continue;
            }
        }
        let chains: FxHashSet<Block> = ta.chain.iter().chain(ea.chain.iter()).copied().collect();
        let mset = mset_cache.entry(m).or_insert_with(|| {
            func.layout
                .blocks()
                .filter(|&x| domtree.block_dominates(m, x))
                .collect()
        });
        if !edge_ok(func, &domtree, m, mset, &chains, term) {
            continue;
        }
        dirty = true;
        let targs = emit_arm(func, term, &ta, derived);
        let eargs = emit_arm(func, term, &ea, derived);
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
        if crate::env_set("PLIRON_IFCONV_DEBUG") {
            eprintln!("ifconv {b} -> jump {m} (arms {t:?} {e:?})");
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{JumpTableData, MemFlagsData};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    /// Evaluate `v` (a pure iconst/icmp-eq/bor/select tree over `b`'s first
    /// param) with the param bound to `idx`.
    fn eval(func: &Function, v: Value, idx: i64, b: Block) -> i64 {
        let v = func.dfg.resolve_aliases(v);
        match func.dfg.value_def(v) {
            ValueDef::Param(d, _) => {
                assert_eq!(d, b);
                idx
            }
            ValueDef::Result(i, _) => match func.dfg.insts[i] {
                InstructionData::UnaryImm { imm, .. } => imm.bits(),
                InstructionData::IntCompare {
                    opcode: Opcode::Icmp,
                    args,
                    cond,
                } => {
                    let (x, y) = (eval(func, args[0], idx, b), eval(func, args[1], idx, b));
                    i64::from(match cond {
                        IntCC::Equal => x == y,
                        _ => panic!("unexpected icmp {cond}"),
                    })
                }
                InstructionData::Binary {
                    opcode: Opcode::Bor,
                    args,
                } => {
                    i64::from(eval(func, args[0], idx, b) != 0 || eval(func, args[1], idx, b) != 0)
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    if eval(func, args[0], idx, b) != 0 {
                        eval(func, args[1], idx, b)
                    } else {
                        eval(func, args[2], idx, b)
                    }
                }
                _ => panic!("unexpected {}", func.dfg.display_inst(i)),
            },
            _ => panic!("bad value def"),
        }
    }

    /// `br_table` whose arms are pure forwarders through a shared last hop:
    /// each arm injects a different constant at its first hop, but the final
    /// edge into the merge is `hop`'s param — identical for every arm. Arm
    /// dedup must compare the resolved (per-arm) values, not the raw edge
    /// args, or every member of the table returns the first arm's constant.
    /// (`rustc_hir_analysis::param_default_policy`: every `ItemKind` member
    /// returned `Forbidden`.)
    #[test]
    fn table_arms_dedup_on_resolved_args() {
        let mut func = Function::new();
        let mut builder_ctx = FunctionBuilderContext::new();
        let (b, merge) = {
            let mut fb = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let b = fb.create_block();
            let dflt = fb.create_block();
            let a0 = fb.create_block();
            let a1 = fb.create_block();
            let a2 = fb.create_block();
            let hop = fb.create_block();
            let merge = fb.create_block();
            fb.append_block_param(b, types::I32);
            fb.append_block_param(hop, types::I8);
            fb.append_block_param(merge, types::I8);

            fb.switch_to_block(b);
            let idx = fb.block_params(b)[0];
            let pool = &mut fb.func.dfg.value_lists;
            let dbc = BlockCall::new(dflt, std::iter::empty(), pool);
            let entries: Vec<BlockCall> = [a0, a1, a2]
                .into_iter()
                .map(|t| BlockCall::new(t, std::iter::empty(), pool))
                .collect();
            let jt = fb.func.create_jump_table(JumpTableData::new(dbc, &entries));
            fb.ins().br_table(idx, jt);

            for (blk, k) in [(a0, 2i64), (a1, 1), (a2, 0)] {
                fb.switch_to_block(blk);
                let v = fb.ins().iconst(types::I8, k);
                fb.ins().jump(hop, &[BlockArg::Value(v)]);
            }
            fb.switch_to_block(dflt);
            let v = fb.ins().iconst(types::I8, -1);
            fb.ins().jump(merge, &[BlockArg::Value(v)]);
            fb.switch_to_block(hop);
            let p = fb.block_params(hop)[0];
            fb.ins().jump(merge, &[BlockArg::Value(p)]);
            fb.switch_to_block(merge);
            let r = fb.block_params(merge)[0];
            fb.ins().return_(&[r]);
            fb.seal_all_blocks();
            (b, merge)
        };

        let mut derived = FxHashMap::default();
        assert_eq!(run(&mut func, &mut derived), 1);
        let term = func.layout.last_inst(b).unwrap();
        let InstructionData::Jump { destination, .. } = &func.dfg.insts[term] else {
            panic!("table not folded: {}", func.display());
        };
        assert_eq!(destination.block(&func.dfg.value_lists), merge);
        let Some(BlockArg::Value(sel)) = destination.args(&func.dfg.value_lists).next() else {
            panic!("no merge arg");
        };
        for (i, want) in [(0, 2), (1, 1), (2, 0), (3, -1), (9, -1)] {
            assert_eq!(eval(&func, sel, i, b) as i8, want, "table index {i}");
        }
    }

    /// Mirrors `rustc_lexer::eat_hexadecimal_digits` (sec6): `hop` is the
    /// digit-loop header merge that `ifconv` first folds into a pure
    /// forwarder to `merge`. `hop`'s params and results still have uses
    /// below `merge` (the `brif cnd` in `merge`, the `has`/`base` args in
    /// `blk411`'s edge into `hdr`). `latch` — the digit-classify backedge
    /// that injects `has=1` — sits inside `hop`'s loop, so `hop` dominates
    /// it and the plain dominance clause in `edge_ok` accepts the escaped
    /// defs. Folding `latch`'s arms past `hop` bypasses it: `has` becomes
    /// a phi over only `entry`'s `0` edge, and every hex literal reported
    /// `no valid digits`.
    #[test]
    fn arm_chain_local_escape_blocks_conversion() {
        let mut func = Function::new();
        let mut builder_ctx = FunctionBuilderContext::new();
        let (latch, hop_p) = {
            let mut fb = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let entry = fb.create_block();
            let hop = fb.create_block();
            let arm_t = fb.create_block();
            let arm_e = fb.create_block();
            let merge = fb.create_block();
            let blk411 = fb.create_block();
            let blk410 = fb.create_block();
            let blk415 = fb.create_block();
            let blk413 = fb.create_block();
            let hdr = fb.create_block();
            let latch = fb.create_block();
            let ret = fb.create_block();
            for blk in [entry, latch] {
                fb.append_block_param(blk, types::I8);
            }
            for _ in 0..4 {
                fb.append_block_param(hop, types::I8);
            }
            for _ in 0..4 {
                fb.append_block_param(merge, types::I8);
            }
            for _ in 0..4 {
                fb.append_block_param(hdr, types::I8);
            }
            fb.append_block_param(ret, types::I8);

            // entry -> hop with has=0 (first char), or straight out.
            // Constants live in entry so every later block may use them.
            fb.switch_to_block(entry);
            let c = fb.block_params(entry)[0];
            let z = fb.ins().iconst(types::I8, 0);
            let one = fb.ins().iconst(types::I8, 1);
            let s = fb.ins().iconst(types::I8, 16);
            fb.ins().brif(
                c,
                hop,
                &[z, z, z, z].map(BlockArg::Value),
                ret,
                &[BlockArg::Value(z)],
            );

            // hop: the loop-header diamond — its `cnd` escapes into
            // merge's own brif below. Its own fold (to a forwarder) is
            // legitimate: latch's backedge keeps it reachable.
            fb.switch_to_block(hop);
            let hp = fb.block_params(hop).to_vec();
            let cnd = fb.ins().icmp(IntCC::Equal, hp[2], hp[3]);
            fb.ins().brif(cnd, arm_t, &[], arm_e, &[]);

            fb.switch_to_block(arm_t);
            fb.ins()
                .jump(merge, &[z, hp[2], hp[2], hp[3]].map(BlockArg::Value));
            fb.switch_to_block(arm_e);
            fb.ins()
                .jump(merge, &[hp[2], hp[3], hp[2], hp[3]].map(BlockArg::Value));

            fb.switch_to_block(merge);
            let mp = fb.block_params(merge).to_vec();
            // Escaped use: cnd (a hop local) steers merge's own branch.
            fb.ins().brif(cnd, blk411, &[], blk410, &[]);

            // Escaped uses: hop's params forwarded to hdr via merge kids.
            fb.switch_to_block(blk411);
            fb.ins()
                .jump(hdr, &[z, z, hp[0], hp[1]].map(BlockArg::Value));
            fb.switch_to_block(blk410);
            let v = fb
                .ins()
                .uload8(types::I8, MemFlagsData::new().with_notrap(), mp[0], 0);
            let f = fb.ins().icmp_imm_u(IntCC::UnsignedLessThan, v, 128);
            fb.ins().brif(f, blk415, &[], blk413, &[]);
            fb.switch_to_block(blk415);
            fb.ins()
                .jump(hdr, &[one, v, hp[0], hp[1]].map(BlockArg::Value));
            fb.switch_to_block(blk413);
            fb.ins()
                .jump(hdr, &[z, v, hp[0], hp[1]].map(BlockArg::Value));

            // hdr loops back through the digit latch or exits. latch is
            // dominated by hop — the shape that defeats the plain
            // dominance clause.
            fb.switch_to_block(hdr);
            let dp = fb.block_params(hdr).to_vec();
            let live = fb.ins().icmp_imm_u(IntCC::Equal, dp[2], 0);
            fb.ins().brif(
                live,
                latch,
                &[BlockArg::Value(dp[0])],
                ret,
                &[BlockArg::Value(dp[2])],
            );

            // Digit latch: inject has=1, base=16 into the shared header.
            fb.switch_to_block(latch);
            let c = fb.block_params(latch)[0];
            let args = [one, s, z, z].map(BlockArg::Value);
            fb.ins().brif(c, hop, &args, hop, &args);

            fb.switch_to_block(ret);
            let r = fb.block_params(ret)[0];
            fb.ins().return_(&[r]);
            fb.seal_all_blocks();
            (latch, hp[0])
        };

        let mut derived = FxHashMap::default();
        let _ = run(&mut func, &mut derived);
        // hop's own diamond fold is legitimate; the latch's fold must be
        // rejected — its arms resolve through `hop` while `hop`'s params
        // and `cnd` still feed blocks below `merge`.
        let term = func.layout.last_inst(latch).unwrap();
        assert!(
            matches!(func.dfg.insts[term], InstructionData::Brif { .. }),
            "hop bypassed despite escaped param {hop_p}: {}",
            func.display()
        );
    }
}

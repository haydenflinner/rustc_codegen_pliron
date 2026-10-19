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
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
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
        let Some(jmp) = func.layout.last_inst(cur) else {
            break;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[jmp] else {
            break;
        };
        // All but the terminator — iterated rather than collected since
        // this runs per brif arm per round.
        let body = || func.layout.block_insts(cur).filter(|&i| i != jmp);
        let params = func.dfg.block_params(cur);
        // The inst cap bounds total speculation across the chain; arms
        // deeper than SPEC_NARROW additionally need `bool_merge` at the
        // conversion site. One fused pass: inst count for the cap, purity,
        // and the `clone` collection — provisionally pushed, truncated if
        // the hop is rejected.
        let base = clone.len();
        let mut ok = cur_edge.len() == params.len()
            && cur_edge.iter().all(|a| matches!(a, BlockArg::Value(_)));
        if ok {
            for i in body() {
                if clone.len() >= SPEC_WIDE || !crate::jumpthread::pure_op(func, i) {
                    ok = false;
                    break;
                }
                clone.push(i);
            }
        }
        if !ok {
            clone.truncate(base);
            break;
        }
        for (j, &p) in params.iter().enumerate() {
            let BlockArg::Value(v) = cur_edge[j] else {
                unreachable!()
            };
            psub.push((p, func.dfg.resolve_aliases(v)));
        }
        chain.push(cur);
        cur_edge.clear();
        cur_edge.extend(destination.args(&func.dfg.value_lists));
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
                crate::jumpthread::prop_derived(&pos.func.dfg, derived, o, nr, &map);
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
    // Early-exit matters: most candidates reject on a use within the
    // first few insts, so scanning beats any precomputed def set.
    mset.iter().all(|&x| {
        func.layout.block_insts(x).all(|i| {
            func.dfg
                .inst_values(i)
                .all(|v| match func.dfg.resolved_value_def(v) {
                    ValueDef::Param(d, _) => {
                        !chains.contains(&d)
                            && (d == m
                                || mset.contains(&d)
                                || domtree.block_dominates(d, term_block(func, term)))
                    }
                    ValueDef::Result(di, _) => {
                        let db = func.layout.inst_block(di).unwrap_or(m);
                        !chains.contains(&db)
                            && (mset.contains(&db) || domtree.try_dominates(di, term, &func.layout))
                    }
                    _ => true,
                })
        })
    })
}

fn term_block(func: &Function, term: Inst) -> Block {
    func.layout.inst_block(term).unwrap()
}

/// `m`'s dominated subtree, capped. `edge_ok` pays an inst scan over the
/// whole region per candidate, and a region this large isn't a profitable
/// conversion anyway — the empty set marks "too big, skip the candidate".
/// (`m` always dominates itself, so a legal set is never empty.)
const MAX_MSET: usize = 1024;

/// idom children + memoized dominated-subtree sets. Subtree enumeration
/// costs O(|subtree|) per query instead of an all-blocks dominance scan.
/// `recompute` must run whenever the domtree is rebuilt — the children
/// come from `idom`, so a stale domtree means stale children.
struct Msets {
    children: FxHashMap<Block, Vec<Block>>,
    sets: FxHashMap<Block, FxHashSet<Block>>,
}

impl Msets {
    fn new(func: &Function, domtree: &DominatorTree) -> Self {
        let mut m = Self {
            children: FxHashMap::default(),
            sets: FxHashMap::default(),
        };
        m.recompute(func, domtree);
        m
    }

    fn recompute(&mut self, func: &Function, domtree: &DominatorTree) {
        self.children.clear();
        for b in func.layout.blocks() {
            if let Some(p) = domtree.idom(b) {
                self.children.entry(p).or_default().push(b);
            }
        }
        self.sets.clear();
    }

    fn get(&mut self, m: Block) -> &FxHashSet<Block> {
        if !self.sets.contains_key(&m) {
            let mut s = FxHashSet::default();
            let mut st = vec![m];
            while let Some(x) = st.pop() {
                if s.insert(x) {
                    if s.len() > MAX_MSET {
                        s.clear();
                        break;
                    }
                    if let Some(cs) = self.children.get(&x) {
                        st.extend_from_slice(cs);
                    }
                }
            }
            self.sets.insert(m, s);
        }
        &self.sets[&m]
    }
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
///
/// `interior[lp]` is the set of reachable blocks in `lp` whose brif
/// keeps both edges inside `lp`, built in one pass: `flat_loop(b)` then
/// asks whether that set is empty apart from `b` itself. The set is a
/// pure function of the CFG, so `Flat` refreshes it only when a fold
/// changed edges (`dirty`), not per candidate.
struct Flat {
    ok: bool,
    la: LoopAnalysis,
    interior: FxHashMap<Loop, FxHashSet<Block>>,
}

impl Flat {
    fn ensure(&mut self, func: &Function, cfg: &ControlFlowGraph, domtree: &DominatorTree) {
        if self.ok {
            return;
        }
        self.la.compute(func, cfg, domtree);
        let reach = reachable(func, cfg);
        self.interior.clear();
        for x in func.layout.blocks() {
            if !reach.contains(&x) {
                continue;
            }
            let Some(t) = func.layout.last_inst(x) else {
                continue;
            };
            let InstructionData::Brif { .. } = func.dfg.insts[t] else {
                continue;
            };
            let Some(mut lp) = self.la.innermost_loop(x) else {
                continue;
            };
            let dests = func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            // Check each ancestor independently: a brif that leaves the
            // inner loop can still keep both edges inside an outer one, so
            // failing `lp` must not skip `lp`'s parents.
            loop {
                if dests
                    .iter()
                    .all(|bc| self.la.is_in_loop(bc.block(&func.dfg.value_lists), lp))
                {
                    self.interior.entry(lp).or_default().insert(x);
                }
                let Some(p) = self.la.loop_parent(lp) else {
                    break;
                };
                lp = p;
            }
        }
        self.ok = true;
    }

    fn flat(&self, b: Block) -> bool {
        let Some(lp) = self.la.innermost_loop(b) else {
            return true;
        };
        self.interior
            .get(&lp)
            .is_none_or(|s| s.iter().all(|&x| x == b))
    }
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
    flat: &mut Flat,
    msets: &mut Msets,
    dirty: &mut bool,
    preds: &mut (
        FxHashMap<Block, FxHashSet<Block>>,
        FxHashMap<Block, FxHashSet<Block>>,
    ),
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
    // Cheap bounds first: a giant dispatch table (16k arms) would burn
    // a `resolve_arm` per entry only to fail `nidx > 64` afterwards.
    let all = func.dfg.jump_tables.get(table).unwrap().all_branches();
    let nidx = all.len() - 1;
    if nidx > 64 {
        return false;
    }
    let dests: Vec<BlockCall> = all.to_vec();
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
        // The `arms.len() > 8` bound below would reject this anyway —
        // stop resolving further arms once it's already exceeded.
        if arms.len() >= 8
            && !arms
                .iter()
                .any(|prev| prev.m == a.m && prev.side == a.side && prev.args == a.args)
        {
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
    // Multi-hop arms only pay off when the fold leaves its loop
    // branch-free (see `flat_loop`).
    if arms.iter().any(|a| a.chained) {
        if *dirty {
            cfg.compute(func);
            domtree.compute(func, &cfg);
            msets.recompute(func, domtree);
            flat.ok = false;
            *dirty = false;
        }
        flat.ensure(func, cfg, domtree);
        if !flat.flat(b) {
            return false;
        }
    }
    // The synthesized values live in `b`; every path into `m` must come
    // from this table. Folds already applied this pass make `cfg`'s pred
    // list stale: a folded block now jumps straight to `m` (new pred) and
    // drops its arm-target edges (lost preds). `preds` tracks both deltas
    // exactly, so no per-fold CFG recompute is needed here.
    let sides: FxHashSet<Block> = arms.iter().filter_map(|a| a.side).collect();
    let ok = {
        let mut fresh: FxHashSet<Block> = cfg
            .pred_iter(m)
            .map(|p| func.layout.inst_block(p.inst).unwrap_or(m))
            .collect();
        if let Some(ls) = preds.1.get(&m) {
            for &l in ls {
                fresh.remove(&l);
            }
        }
        if let Some(gs) = preds.0.get(&m) {
            fresh.extend(gs.iter().copied());
        }
        fresh.iter().all(|&pb| pb == b || sides.contains(&pb))
    };
    if !ok {
        return false;
    }
    let chains: FxHashSet<Block> = arms.iter().flat_map(|a| a.chain.iter().copied()).collect();
    let mset = msets.get(m);
    let t0 = EDGE_PROF.with(|c| c.get()).then(std::time::Instant::now);
    let ok = !mset.is_empty() && edge_ok(func, domtree, m, mset, &chains, term);
    if let Some(t0) = t0 {
        EDGE_NS.with(|c| c.set(c.get() + t0.elapsed().as_nanos()));
    }
    if !ok {
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
                ValueDef::Result(di, _) => domtree.try_dominates(di, term, &func.layout),
                ValueDef::Param(db, _) => domtree.block_dominates(db, b),
                _ => false,
            };
            if !dom {
                return false;
            }
        }
    }
    *dirty = true;
    for d in &dests {
        let db = d.block(&func.dfg.value_lists);
        if db != m {
            preds.1.entry(db).or_default().insert(b);
        }
    }
    preds.0.entry(m).or_default().insert(b);
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
    if pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::ifconv", tracing::Level::DEBUG)
    {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::ifconv", "ifconv {b} br_table -> jump {m} (arms {})", arms.len())
        };
    }
    true
}

thread_local! {
    static EDGE_NS: std::cell::Cell<u128> = const { std::cell::Cell::new(0) };
    // `edge_ok` runs per candidate arm — its per-call `Instant::now` only
    // ever feeds the `EDGE_NS` total printed at the end of `run`, so the
    // clock reads are gated on `pliron::ifconv` INFO being live.
    static EDGE_PROF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn run(
    func: &mut Function,
    derived: &mut FxHashMap<Value, Value>,
    cd: Option<(&mut ControlFlowGraph, &mut DominatorTree)>,
) -> usize {
    EDGE_NS.set(0);
    EDGE_PROF.set(
        pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::ifconv", tracing::Level::INFO),
    );
    // The caller's cached cfg/dt may be reused: edge-args and values may
    // have changed since it was built, but block adjacency has not.
    let (mut cfg_l, mut dt_l);
    let (cfg, domtree) = match cd {
        Some((c, d)) => (c, d),
        None => {
            cfg_l = ControlFlowGraph::with_function(func);
            dt_l = DominatorTree::with_function(func, &cfg_l);
            (&mut cfg_l, &mut dt_l)
        }
    };
    let mut flat = Flat {
        ok: false,
        la: LoopAnalysis::new(),
        interior: FxHashMap::default(),
    };
    let mut msets = Msets::new(func, domtree);
    let mut dirty = false;
    // Exact pred deltas for applied folds — (gained direct preds, lost
    // preds) per block — so checks that need fresh pred sets don't pay a
    // full CFG recompute per conversion. Dominance can only strengthen
    // under edge removal, so the stale `domtree` stays a conservative
    // under-approximation for the `dominates`-required checks.
    let mut preds: (
        FxHashMap<Block, FxHashSet<Block>>,
        FxHashMap<Block, FxHashSet<Block>>,
    ) = (FxHashMap::default(), FxHashMap::default());
    let mut n = 0;
    // A fold can expose a convertible diamond in a block already
    // scanned, so rescan to fixpoint (bounded). The setup —
    // cfg/domtree/msets/`preds` deltas — carries across rounds; `dirty`
    // reads still trigger the incremental recompute. The pred deltas
    // are idempotent against a recomputed cfg, so a recompute inside a
    // round leaves later rounds correct.
    // Folds only rewrite terminators and emit into the scanned block —
    // no block is ever added or removed, so the scan list is stable
    // across rounds.
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut scans = 0usize;
    // Per-brif edge args — reused across blocks and rounds instead of two
    // fresh Vecs per candidate.
    let mut t_edge: Vec<BlockArg> = Vec::new();
    let mut e_edge: Vec<BlockArg> = Vec::new();
    'outer: for _ in 0..8 {
        let mut rn = 0;
        for b in blocks.iter().copied() {
            scans += 1;
            let Some(term) = func.layout.last_inst(b) else {
                continue;
            };
            let InstructionData::Brif { arg: cond, .. } = func.dfg.insts[term] else {
                if matches!(func.dfg.insts[term], InstructionData::BranchTable { .. })
                    && conv_table(
                        func,
                        cfg,
                        domtree,
                        &mut flat,
                        &mut msets,
                        &mut dirty,
                        &mut preds,
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
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
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
            t_edge.clear();
            t_edge.extend(t_bc.args(&func.dfg.value_lists));
            e_edge.clear();
            e_edge.extend(e_bc.args(&func.dfg.value_lists));
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
            // Multi-hop arms only pay off when the fold leaves its loop
            // branch-free (see `flat_loop`).
            if ta.chained || ea.chained {
                if dirty {
                    cfg.compute(func);
                    domtree.compute(func, cfg);
                    msets.recompute(func, domtree);
                    flat.ok = false;
                    dirty = false;
                }
                flat.ensure(func, cfg, domtree);
                if !flat.flat(b) {
                    continue;
                }
            }
            let chains: FxHashSet<Block> =
                ta.chain.iter().chain(ea.chain.iter()).copied().collect();
            let mset = msets.get(m);
            let t0 = EDGE_PROF.with(|c| c.get()).then(std::time::Instant::now);
            let ok = !mset.is_empty() && edge_ok(func, domtree, m, mset, &chains, term);
            if let Some(t0) = t0 {
                EDGE_NS.with(|c| c.set(c.get() + t0.elapsed().as_nanos()));
            }
            if !ok {
                continue;
            }
            dirty = true;
            for d in [t, e] {
                if d != m {
                    preds.1.entry(d).or_default().insert(b);
                }
            }
            preds.0.entry(m).or_default().insert(b);
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
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::ifconv", tracing::Level::DEBUG)
            {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::ifconv", "ifconv {b} -> jump {m} (arms {t:?} {e:?})")
                };
            }
            rn += 1;
        }
        n += rn;
        if rn == 0 {
            break 'outer;
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::ifconv", tracing::Level::INFO)
    {
        EDGE_NS.with(|c| tracing::info!(target: "pliron::ifconv", "ifconv edge_ok {:.1}ms scans={scans} conv={n}", c.get() as f64 / 1e6));
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

    /// Nested loops: the inner header's brif exits the inner loop but both
    /// edges stay inside the outer one. `flat` must see that brif as
    /// interior to the *outer* loop — checking only the innermost loop (or
    /// stopping at the first ancestor a brif escapes) under-reports, and an
    /// illegal outer-loop conversion then emits uses past their defs
    /// (rustc_errors `Chars::try_fold` verify failure, stage2).
    #[test]
    fn flat_counts_brif_interior_to_ancestor_loop() {
        let mut func = Function::new();
        let mut builder_ctx = FunctionBuilderContext::new();
        let (outer, inner, outer_mid) = {
            let mut fb = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let entry = fb.create_block();
            let outer = fb.create_block();
            let inner = fb.create_block();
            let inner_body = fb.create_block();
            let outer_mid = fb.create_block();
            let exit = fb.create_block();

            fb.switch_to_block(entry);
            fb.ins().jump(outer, &[]);

            // Outer header: enter the inner loop or leave.
            fb.switch_to_block(outer);
            let c = fb.ins().iconst(types::I8, 0);
            fb.ins().brif(c, inner, &[], exit, &[]);

            // Inner header: stay in the inner loop or break to the outer —
            // both destinations remain inside the outer loop.
            fb.switch_to_block(inner);
            let c = fb.ins().iconst(types::I8, 0);
            fb.ins().brif(c, inner_body, &[], outer_mid, &[]);

            fb.switch_to_block(inner_body);
            fb.ins().jump(inner, &[]);
            fb.switch_to_block(outer_mid);
            fb.ins().jump(outer, &[]);
            fb.switch_to_block(exit);
            fb.ins().return_(&[]);
            fb.seal_all_blocks();
            (outer, inner, outer_mid)
        };

        let cfg = ControlFlowGraph::with_function(&func);
        let domtree = DominatorTree::with_function(&func, &cfg);
        let mut flat = Flat {
            ok: false,
            la: LoopAnalysis::new(),
            interior: FxHashMap::default(),
        };
        flat.ensure(&func, &cfg, &domtree);
        // The inner header's brif is interior to the outer loop, so no
        // other block in that loop may be if-converted...
        assert!(!flat.flat(outer));
        assert!(!flat.flat(outer_mid));
        // ...except the interior block itself.
        assert!(flat.flat(inner));
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
        assert_eq!(run(&mut func, &mut derived, None), 1);
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
        let _ = run(&mut func, &mut derived, None);
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

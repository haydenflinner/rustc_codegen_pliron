//! Localification: a simple form of register allocation that picks
//! locations for SSA values in Wasm locals.
//!
//! Performance notes: liveness sets and live-range maps are keyed by
//! `Value`, a dense index space, so all per-value state lives in flat
//! arrays. The liveness fixpoint additionally runs over a compacted
//! index space of only the cross-block values (values with a use
//! outside their defining block), and we track cross-block liveness
//! with bitsets indexed by this index space.

use crate::backend::bitset::Bitset;
use crate::backend::cross_block_ids::{CrossBlockId, CrossBlockValues};
use crate::backend::dense_live_set::DenseLiveSet;
use crate::backend::treeify::Trees;
use crate::cfg::CFGInfo;
use crate::entity::{EntityRef, EntityVec, PerEntity};
use crate::ir::{Block, BlockTarget, FunctionBody, Local, Terminator, Type, Value, ValueDef};
use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};
use smallvec::{smallvec, SmallVec};
use std::ops::Range;

#[derive(Clone, Debug, Default)]
pub struct Localifier {
    pub values: PerEntity<Value, SmallVec<[Local; 2]>>,
    pub locals: EntityVec<Local, Type>,
}

impl Localifier {
    pub fn compute(body: &FunctionBody, cfg: &CFGInfo, trees: &Trees) -> Self {
        Context::new(body, cfg, trees).compute()
    }
}

struct Context<'a> {
    body: &'a FunctionBody,
    cfg: &'a CFGInfo,
    trees: &'a Trees,
    results: Localifier,

    /// Two-way map between `Value` and `CrossBlockId` for the compacted
    /// inter-block value index space.
    cross_blocks: CrossBlockValues,

    /// Precise liveness for each block: cross-block values live at the
    /// end, as a bitset over compact ids.
    block_end_live: PerEntity<Block, Bitset>,

    /// Liveranges for each Value, in an arbitrary index space
    /// (concretely, the span of first to last instruction visit step
    /// index in an RPO walk over the function body). `usize::MAX`
    /// start means "no range recorded".
    ranges: Vec<Range<usize>>,
    /// Number of points.
    points: usize,

    /// PLIRON: coalescing candidates for each value. For an edge
    /// transfer arg `v` feeding target-block param `p`, each is
    /// recorded as a preferred-local candidate for the other. In
    /// `allocate`, a value whose range starts first tries to take a
    /// candidate's local out of the freelist; this is sound because a
    /// local only reaches the freelist once its owner's range has
    /// ended, so overlapping live ranges can never be merged. When the
    /// preference fires, the transfer's `local.get L; local.set L`
    /// degenerates to a no-op.
    preferred: HashMap<Value, SmallVec<[Value; 2]>>,
    /// PLIRON: `arg` -> `param` steals: `arg`'s def sits in the same
    /// block as the edge passing it to `param`, `param`'s range
    /// outlives `arg`'s, and no use of `param` follows `arg`'s def in
    /// that block (terminator args included). `arg` then takes
    /// `param`'s local outright — `param`'s remaining uses are only
    /// reachable through edges that rewrite the local, so the early
    /// write is unobservable — instead of waiting for `param`'s local
    /// to reach the freelist. This is what lets a loop's `i+1`
    /// overwrite `i`'s slot before the backedge, LLVM-style.
    steal: HashMap<Value, Value>,
}

trait Visitor {
    fn visit_use(&mut self, _: Value) {}
    fn visit_def(&mut self, _: Value) {}
    fn post_inst(&mut self, _: Value) {}
    fn pre_inst(&mut self, _: Value) {}
    fn post_term(&mut self) {}
    fn pre_term(&mut self) {}
    fn post_params(&mut self) {}
    fn pre_params(&mut self) {}
}

fn is_tree_or_remat(value: Value, trees: &Trees) -> bool {
    trees.owner.contains_key(&value) || trees.remat.contains(&value)
}

struct BlockVisitor<'a, V: Visitor> {
    body: &'a FunctionBody,
    trees: &'a Trees,
    visitor: V,
}
impl<'a, V: Visitor> BlockVisitor<'a, V> {
    fn new(body: &'a FunctionBody, trees: &'a Trees, visitor: V) -> Self {
        log::trace!(
            "localify: running on:\n{}",
            body.display_verbose("| ", None)
        );
        Self {
            body,
            trees,
            visitor,
        }
    }
    fn visit_block(&mut self, block: Block) {
        self.visitor.post_term();
        self.body.blocks[block].terminator.visit_uses(|u| {
            self.visit_use(u);
        });
        self.visitor.pre_term();

        for &inst in self.body.blocks[block].insts.iter().rev() {
            if is_tree_or_remat(inst, self.trees) {
                continue;
            }
            self.visitor.post_inst(inst);
            self.visit_inst(inst, /* root = */ true);
            self.visitor.pre_inst(inst);
        }

        self.visitor.post_params();
        for &(_, param) in &self.body.blocks[block].params {
            self.visitor.visit_def(param);
        }
        self.visitor.pre_params();
    }
    fn visit_inst(&mut self, value: Value, root: bool) {
        // If this is an instruction...
        if let ValueDef::Operator(_, args, _) = &self.body.values[value] {
            // If root, we need to process the def.
            if root {
                self.visitor.visit_def(value);
            }
            // Handle uses.
            for &arg in &self.body.arg_pool[*args] {
                self.visit_use(arg);
            }
        }
    }
    fn visit_use(&mut self, value: Value) {
        let value = self.body.resolve_alias(value);
        if let ValueDef::PickOutput(value, _, _) = self.body.values[value] {
            self.visit_use(value);
            return;
        }
        if is_tree_or_remat(value, self.trees) {
            // If this is a treeified or rematerialized value, then
            // don't process the use, but process the instruction
            // directly here. (Lowering rematerializes remat values at
            // each use and never lowers their defs, so a remat use
            // must not keep a local alive: its def is never visited
            // and it would otherwise be tracked as live everywhere,
            // holding a dead local for the whole function.)
            self.visit_inst(value, /* root = */ false);
        } else {
            // Otherwise, this is a proper use.
            self.visitor.visit_use(value);
        }
    }
}

impl<'a> Context<'a> {
    fn new(body: &'a FunctionBody, cfg: &'a CFGInfo, trees: &'a Trees) -> Self {
        let mut results = Localifier::default();

        // Create locals for function args.
        for &(ty, value) in &body.blocks[body.entry].params {
            let param_local = results.locals.push(ty);
            results.values[value] = smallvec![param_local];
        }

        Self {
            body,
            cfg,
            trees,
            results,
            cross_blocks: CrossBlockValues::default(),
            block_end_live: PerEntity::default(),
            ranges: vec![usize::MAX..usize::MAX; body.values.len()],
            points: 0,
            preferred: HashMap::default(),
            steal: HashMap::default(),
        }
    }

    /// Assign compact ids to the values with a (proper) use outside
    /// the block where their def is VISITED; only those can appear in
    /// a liveness set. A def the block walk skips (treeified or remat
    /// insts) never cancels its uses, so such values stay tracked
    /// everywhere, matching the walk's behavior.
    fn find_cross_block_values(&mut self) {
        self.cross_blocks = CrossBlockValues::build(self.body, self.trees);
    }

    fn compute_liveness(&mut self) {
        struct LivenessVisitor<'b> {
            cross_blocks: &'b CrossBlockValues,
            live: DenseLiveSet,
        }
        impl<'b> Visitor for LivenessVisitor<'b> {
            fn visit_use(&mut self, value: Value) {
                if let Some(id) = self.cross_blocks.get(value) {
                    self.live.set_live(id.as_u32());
                }
            }
            fn visit_def(&mut self, value: Value) {
                if let Some(id) = self.cross_blocks.get(value) {
                    self.live.set_dead(id.as_u32());
                }
            }
        }

        let mut visitor = BlockVisitor::new(
            self.body,
            self.trees,
            LivenessVisitor {
                cross_blocks: &self.cross_blocks,
                live: DenseLiveSet::new(self.cross_blocks.len()),
            },
        );
        let mut workqueue: Vec<Block> = self.cfg.rpo.values().cloned().collect();
        let mut workqueue_set = vec![true; self.body.blocks.len()];
        while let Some(block) = workqueue.pop() {
            workqueue_set[block.index()] = false;
            visitor.visitor.live.next_epoch();
            for id in self.block_end_live[block].iter() {
                visitor.visitor.live.set_live(id);
            }
            visitor.visit_block(block);

            // Insert the live-in ids straight into each pred's
            // live-out bitset.
            let live = &visitor.visitor.live;
            for &pred in &self.body.blocks[block].preds {
                let pred_live = &mut self.block_end_live[pred];
                let mut changed = false;
                for &id in live.touched() {
                    if live.is_live(id) && pred_live.insert(id) {
                        changed = true;
                    }
                }
                if changed && !workqueue_set[pred.index()] {
                    workqueue_set[pred.index()] = true;
                    workqueue.push(pred);
                }
            }
        }
    }

    fn find_ranges(&mut self) {
        let mut point = 0;

        struct LiveRangeVisitor<'b> {
            point: &'b mut usize,
            /// Live values in the current block walk; the range start
            /// (visit point of the last use seen) rides in `start`.
            live: DenseLiveSet,
            start: &'b mut [usize],
            ranges: &'b mut [Range<usize>],
        }
        impl<'b> LiveRangeVisitor<'b> {
            fn record_def(&mut self, value: Value, range: Range<usize>) {
                let existing = &mut self.ranges[value.index()];
                if existing.start == usize::MAX {
                    *existing = range;
                } else {
                    existing.start = std::cmp::min(existing.start, range.start);
                    existing.end = std::cmp::max(existing.end, range.end);
                }
            }
        }
        impl<'b> Visitor for LiveRangeVisitor<'b> {
            fn pre_params(&mut self) {
                *self.point += 1;
            }
            fn pre_inst(&mut self, _: Value) {
                *self.point += 1;
            }
            fn pre_term(&mut self) {
                *self.point += 1;
            }
            fn visit_use(&mut self, value: Value) {
                if self.live.set_live(value.index() as u32) {
                    self.start[value.index()] = *self.point;
                }
            }
            fn visit_def(&mut self, value: Value) {
                let range = if self.live.set_dead(value.index() as u32) {
                    self.start[value.index()]..(*self.point + 1)
                } else {
                    *self.point..(*self.point + 1)
                };
                self.record_def(value, range);
            }
        }

        let mut start = vec![0usize; self.body.values.len()];
        let mut live = DenseLiveSet::new(self.body.values.len());
        for &block in self.cfg.rpo.values().rev() {
            live.next_epoch();
            let visitor = LiveRangeVisitor {
                live,
                start: &mut start,
                point: &mut point,
                ranges: &mut self.ranges,
            };
            let mut visitor = BlockVisitor::new(&self.body, self.trees, visitor);
            // Live-outs to succ blocks: in this block-local
            // handling, model them as uses as the end of the block.
            for id in self.block_end_live[block].iter() {
                let livein = self.cross_blocks.value(CrossBlockId::new(id));
                let livein = self.body.resolve_alias(livein);
                visitor.visitor.visit_use(livein);
            }
            // Visit all insts.
            visitor.visit_block(block);
            // Live-ins from pred blocks: anything still live has a
            // virtual def at top of block.
            let still_live: Vec<Value> = visitor
                .visitor
                .live
                .touched()
                .iter()
                .copied()
                .filter(|&i| visitor.visitor.live.is_live(i))
                .map(|i| Value::new(i as usize))
                .collect();
            for v in still_live {
                visitor.visitor.visit_def(v);
            }
            live = visitor.visitor.live;
        }

        self.points = point + 1;
    }

    /// PLIRON: build the coalescing-candidate map. For each edge
    /// transfer arg `v` -> target param `p`, record `p` as a preferred
    /// partner of `v` and vice versa. `v` is resolved exactly as
    /// `visit_use` resolves uses (alias chains, `PickOutput` unwrap),
    /// so the candidate is the value that actually owns a live range.
    fn compute_preferred(&mut self) {
        // PLIRON: off switch (`PLIRON_WCOAL=0` reverts to stock
        // freelist order).
        if std::env::var_os("PLIRON_WCOAL").is_some_and(|v| v == "0") {
            return;
        }
        // Edge args live on the *predecessor's* terminator, so walk
        // every block's targets rather than `preds`.
        let body = self.body;
        for block in body.blocks.iter() {
            let target_args = |preferred: &mut HashMap<Value, SmallVec<[Value; 2]>>,
                                   t: &BlockTarget| {
                let params = &body.blocks[t.block].params;
                for (i, &arg) in t.args.iter().enumerate() {
                    if i >= params.len() {
                        break;
                    }
                    let mut v = body.resolve_alias(arg);
                    if let ValueDef::PickOutput(v2, _, _) = body.values[v] {
                        v = v2;
                    }
                    let p = params[i].1;
                    if v != p {
                        preferred.entry(v).or_default().push(p);
                        preferred.entry(p).or_default().push(v);
                    }
                }
            };
            match &body.blocks[block].terminator {
                Terminator::Br { target } => target_args(&mut self.preferred, target),
                Terminator::CondBr {
                    if_true, if_false, ..
                } => {
                    target_args(&mut self.preferred, if_true);
                    target_args(&mut self.preferred, if_false);
                }
                Terminator::Select {
                    targets, default, ..
                } => {
                    for t in targets {
                        target_args(&mut self.preferred, t);
                    }
                    target_args(&mut self.preferred, default);
                }
                _ => {}
            }
        }
    }

    /// PLIRON: fill `steal` — see the field's comment for the rule.
    /// Needs `ranges` for the outlives check, so runs after
    /// `find_ranges` (and after `compute_preferred` so a steal can be
    /// preferred over the weaker freelist hint in `allocate`).
    fn compute_steal(&mut self) {
        // PLIRON: shares `PLIRON_WCOAL`'s off switch.
        if std::env::var_os("PLIRON_WCOAL").is_some_and(|v| v == "0") {
            return;
        }
        let body = self.body;
        let trees = self.trees;
        let mut defpos: HashMap<Value, usize> = HashMap::default();
        for block in body.blocks.iter() {
            defpos.clear();
            for (i, &iv) in body.blocks[block].insts.iter().enumerate() {
                defpos.insert(iv, i);
            }
            // Does `p` have a use in this block after inst `pos`?
            // Terminator args (any edge) count as uses at the end.
            // The use point of a treeified inst's args is the root
            // that consumes it, and `p` can sit inside a treeified
            // arg's subtree, so positions and args are resolved
            // through `trees`.
            let root_pos = |iv: Value, k: usize| -> usize {
                if !trees.owner.contains_key(&iv) {
                    return k;
                }
                let mut r = iv;
                for _ in 0..64 {
                    match trees.owner.get(&r) {
                        Some(&va) => r = va.root_value(),
                        None => break,
                    }
                }
                *defpos.get(&r).unwrap_or(&k)
            };
            let contains_p = |u: Value, p: Value| -> bool {
                let mut stack = vec![u];
                for _ in 0..128 {
                    let Some(u) = stack.pop() else {
                        return false;
                    };
                    let u = body.resolve_alias(u);
                    let u = match &body.values[u] {
                        ValueDef::PickOutput(v2, _, _) => *v2,
                        _ => u,
                    };
                    if u == p {
                        return true;
                    }
                    if trees.owner.contains_key(&u) || trees.remat.contains(&u) {
                        if let ValueDef::Operator(_, args, _) = &body.values[u] {
                            stack.extend(body.arg_pool[*args].iter().copied());
                        }
                    }
                }
                // Depth budget exhausted: bail to "used".
                true
            };
            let used_after = |pos: usize, p: Value| -> bool {
                for (k, &iv) in body.blocks[block].insts.iter().enumerate() {
                    if root_pos(iv, k) <= pos {
                        continue;
                    }
                    if let ValueDef::Operator(_, args, _) = &body.values[iv] {
                        for &u in &body.arg_pool[*args] {
                            if contains_p(u, p) {
                                return true;
                            }
                        }
                    }
                }
                let mut hit = false;
                body.blocks[block].terminator.visit_uses(|u| {
                    if hit {
                        return;
                    }
                    if contains_p(u, p) {
                        hit = true;
                    }
                });
                hit
            };
            let ranges = &self.ranges;
            // PLIRON: hoisted field refs — the closure below must not
            // capture `self` (edition-2018 whole-`self` capture would
            // fight the `&mut self.steal` argument).
            let bel = &self.block_end_live[block];
            let cbs = &self.cross_blocks;
            let check = |t: &BlockTarget, steal: &mut HashMap<Value, Value>| {
                let params = &body.blocks[t.block].params;
                for (i, &arg) in t.args.iter().enumerate() {
                    if i >= params.len() {
                        break;
                    }
                    let mut a = body.resolve_alias(arg);
                    if let ValueDef::PickOutput(v2, _, _) = body.values[a] {
                        a = v2;
                    }
                    let p = params[i].1;
                    if a == p {
                        continue;
                    }
                    let Some(&pos) = defpos.get(&a) else {
                        continue;
                    };
                    // The steal aliases `a`'s uses into `p`'s local
                    // while `p`'s range still owns it; `p` must outlive
                    // `a` or the local could be recycled mid-flight.
                    let ar = &ranges[a.index()];
                    let pr = &ranges[p.index()];
                    if ar.start == usize::MAX
                        || pr.start == usize::MAX
                        || ar.end > pr.end
                    {
                        continue;
                    }
                    if used_after(pos, p) {
                        continue;
                    }
                    // PLIRON: `a`'s write lands before the branch, so
                    // it dominates every outgoing edge — but only the
                    // edge to `t.block` rewrites `p`'s local. If `p` is
                    // live-out of this block, some sibling edge can
                    // still observe `p`'s old value after the clobber,
                    // so the steal is unsound. (`t.block` itself can't
                    // be the witness: `p` is its param, i.e. redefined
                    // at entry, so it is never live-in there.)
                    let mut pr = p;
                    if let ValueDef::PickOutput(v2, _, _) = body.values[pr] {
                        pr = v2;
                    }
                    pr = body.resolve_alias(pr);
                    let p_liveout = bel.iter().any(|id| {
                        let mut lv = cbs.value(CrossBlockId::new(id));
                        if let ValueDef::PickOutput(v2, _, _) = body.values[lv] {
                            lv = v2;
                        }
                        body.resolve_alias(lv) == pr
                    });
                    if p_liveout {
                        continue;
                    }
                    steal.insert(a, p);
                }
            };
            match &body.blocks[block].terminator {
                Terminator::Br { target } => check(target, &mut self.steal),
                Terminator::CondBr {
                    if_true, if_false, ..
                } => {
                    check(if_true, &mut self.steal);
                    check(if_false, &mut self.steal);
                }
                Terminator::Select {
                    targets, default, ..
                } => {
                    for t in targets {
                        check(t, &mut self.steal);
                    }
                    check(default, &mut self.steal);
                }
                _ => {}
            }
        }
    }

    fn allocate(&mut self) {
        // Sort values by ranges' starting points, then value to break ties.
        let mut ranges: Vec<(Value, std::ops::Range<usize>)> = self
            .ranges
            .iter()
            .enumerate()
            .filter(|(_, r)| r.start != usize::MAX)
            .map(|(i, r)| (Value::new(i), r.clone()))
            .collect();
        ranges.sort_unstable_by_key(|(val, range)| (range.start, *val));

        // Keep a list of expiring Locals by expiry point.
        let mut expiring: HashMap<usize, SmallVec<[(Type, Local); 8]>> = HashMap::default();

        // Iterate over allocation space, processing range starts (at
        // which point we allocate) and ends (at which point we add to
        // the freelist).
        let mut range_idx = 0;
        let mut freelist: HashMap<Type, Vec<Local>> = HashMap::default();
        // PLIRON: current range-owner of each local, for the steal
        // check — a local may only be stolen while the candidate
        // param's own range still owns it (never reallocated, never
        // freed).
        let mut owner: HashMap<Local, Value> = HashMap::default();

        for i in 0..self.points {
            // Process ends. (Ends are exclusive, so we do them
            // first; another range can grab the local at the same
            // point index in this same iteration.)
            if let Some(expiring) = expiring.remove(&i) {
                for (ty, local) in expiring {
                    log::trace!(" -> expiring {} of type {} back to freelist", local, ty);
                    owner.remove(&local);
                    freelist.entry(ty).or_insert_with(|| vec![]).push(local);
                }
            }

            // Process starts.
            while range_idx < ranges.len() && ranges[range_idx].1.start == i {
                let (value, range) = ranges[range_idx].clone();
                range_idx += 1;
                log::trace!(
                    "localify: processing range for {}: {}..{}",
                    value,
                    range.start,
                    range.end
                );

                // If the value is an arg on block0, ignore; these
                // already have fixed locations.
                if let &ValueDef::BlockParam(b, _, _) = &self.body.values[value] {
                    if b == self.body.entry {
                        continue;
                    }
                }

                // Try getting a local from the freelist; if not,
                // allocate a new one.
                let mut allocs = smallvec![];
                let expiring = expiring.entry(range.end).or_insert_with(|| smallvec![]);
                // PLIRON: try a coalescing candidate's local first; a
                // local on the freelist is provably dead here, so this
                // can only merge non-overlapping ranges.
                let cands = self.preferred.get(&value).cloned();
                // PLIRON: a steal candidate beats the freelist hint —
                // it turns the edge transfer into a self-copy instead
                // of merely picking a nicer free local. Only applies
                // to single-result values (the local list must match
                // the param's one local) and only while `p`'s range
                // still owns the local; ownership stays with `p`, so
                // nothing is pushed to `expiring` for the stolen slot.
                let tys = self.body.values[value].tys(&self.body.type_pool);
                let steal = if tys.len() == 1 {
                    self.steal.get(&value).and_then(|&p| {
                        let vs = &self.results.values[p];
                        if vs.len() == 1 && owner.get(&vs[0]) == Some(&p) {
                            Some(vs[0])
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };
                for &ty in tys {
                    let mut local = steal.and_then(|l| (self.results.locals[l] == ty).then_some(l));
                    if local.is_none() {
                        local = cands.as_ref().and_then(|cands| {
                            cands.iter().find_map(|&cand| {
                                if self.results.values[cand].len() != 1 {
                                    return None;
                                }
                                let l = self.results.values[cand][0];
                                freelist.get_mut(&ty).and_then(|free| {
                                    free.iter()
                                        .position(|&x| x == l)
                                        .map(|pos| free.swap_remove(pos))
                                })
                            })
                        });
                    }
                    if local.is_none() {
                        local = Some(
                            freelist
                                .get_mut(&ty)
                                .and_then(|v| v.pop())
                                .unwrap_or_else(|| {
                                    log::trace!(" -> allocating new local of type {}", ty);
                                    self.results.locals.push(ty)
                                }),
                        );
                    }
                    let local = local.unwrap();
                    log::trace!(" -> got local {} of type {}", local, ty);
                    let stolen = steal == Some(local);
                    if !stolen {
                        owner.insert(local, value);
                    }
                    allocs.push(local);
                    if !stolen {
                        expiring.push((ty, local));
                    }
                }
                self.results.values[value] = allocs;
            }
        }
    }

    fn compute(mut self) -> Localifier {
        self.find_cross_block_values();
        self.compute_liveness();
        self.find_ranges();
        self.compute_preferred();
        self.compute_steal();
        self.allocate();
        self.coalesce_holes();
        self.results
    }

    /// PLIRON: merge locals that the linearized-range freelist kept
    /// apart because of lifetime holes. `find_ranges` assigns each
    /// value a single interval in reverse-RPO point space, so a value
    /// live in B_early and B_late looks live through every block
    /// between them in that order — even when precise
    /// `block_end_live` says it is dead there. Another value whose
    /// true range sits only in a hole then gets a fresh local.
    ///
    /// Walk each block with SSA liveness (seeded from
    /// `block_end_live`) to build a local-interference graph, then
    /// greedily recolor same-type locals. Entry-param locals keep
    /// their indices. `PLIRON_WCOAL_HOLES=1` enables; `PLIRON_WCOAL=0`
    /// disables together with preferred/steal.
    fn coalesce_holes(&mut self) {
        if std::env::var_os("PLIRON_WCOAL").is_some_and(|v| v == "0") {
            return;
        }
        if !std::env::var_os("PLIRON_WCOAL_HOLES").is_some_and(|v| v == "1") {
            return;
        }
        let n = self.results.locals.len();
        if n <= 1 {
            return;
        }
        let n_params = self.body.blocks[self.body.entry].params.len();
        let mut inter = vec![Bitset::default(); n];

        for &block in self.cfg.rpo.values() {
            let mut state = HoleLive::default();
            for id in self.block_end_live[block].iter() {
                let v = self
                    .body
                    .resolve_alias(self.cross_blocks.value(CrossBlockId::new(id)));
                state.add_val(&mut inter, &self.results, v);
            }
            let mut term_uses = Vec::new();
            self.body.blocks[block]
                .terminator
                .visit_uses(|u| term_uses.push(u));
            for u in term_uses {
                for_each_local_value(self.body, self.trees, u, |v| {
                    state.add_val(&mut inter, &self.results, v);
                });
            }
            for &inst in self.body.blocks[block].insts.iter().rev() {
                if is_tree_or_remat(inst, self.trees) {
                    continue;
                }
                state.mark_against_live(&mut inter, &self.results, inst);
                state.del_val(&self.results, inst);
                if let ValueDef::Operator(_, args, _) = &self.body.values[inst] {
                    for &arg in &self.body.arg_pool[*args] {
                        for_each_local_value(self.body, self.trees, arg, |v| {
                            state.add_val(&mut inter, &self.results, v);
                        });
                    }
                }
            }
            for &(_, param) in self.body.blocks[block].params.iter().rev() {
                state.mark_against_live(&mut inter, &self.results, param);
                state.del_val(&self.results, param);
            }
        }

        let mut mapping: Vec<Local> = (0..n).map(Local::new).collect();
        for i in 0..n {
            if i < n_params {
                mapping[i] = Local::new(i);
                continue;
            }
            let ty = self.results.locals[Local::new(i)];
            let mut blocked = Bitset::default();
            for j in inter[i].iter() {
                blocked.insert(mapping[j as usize].index() as u32);
            }
            let mut chosen = Local::new(i);
            for k in 0..n {
                if self.results.locals[Local::new(k)] != ty {
                    continue;
                }
                if blocked.contains(k as u32) {
                    continue;
                }
                chosen = Local::new(k);
                break;
            }
            mapping[i] = chosen;
        }

        if mapping.iter().enumerate().all(|(i, &m)| m.index() == i) {
            return;
        }

        let mut new_of_rep: Vec<Option<Local>> = vec![None; n];
        let mut new_locals: EntityVec<Local, Type> = EntityVec::default();
        for i in 0..n_params {
            new_of_rep[i] = Some(new_locals.push(self.results.locals[Local::new(i)]));
        }
        for i in 0..n {
            let rep = mapping[i].index();
            if new_of_rep[rep].is_some() {
                continue;
            }
            if !(0..n).any(|j| mapping[j].index() == rep) {
                continue;
            }
            new_of_rep[rep] = Some(new_locals.push(self.results.locals[Local::new(rep)]));
        }

        for v in self.body.values.iter() {
            for l in self.results.values[v].iter_mut() {
                let rep = mapping[l.index()];
                *l = new_of_rep[rep.index()].expect("rep used by a value");
            }
        }
        log::trace!(
            "localify: coalesce_holes {} -> {} locals",
            n,
            new_locals.len()
        );
        self.results.locals = new_locals;
    }
}

/// Live-local refcounts for one block's interference walk.
#[derive(Default)]
struct HoleLive {
    vals: HashSet<Value>,
    local_refs: HashMap<Local, u32>,
    locals: HashSet<Local>,
}

impl HoleLive {
    fn mark_pair(inter: &mut [Bitset], a: Local, b: Local) {
        if a != b {
            inter[a.index()].insert(b.index() as u32);
            inter[b.index()].insert(a.index() as u32);
        }
    }

    fn add_local(&mut self, inter: &mut [Bitset], l: Local) {
        let c = self.local_refs.entry(l).or_insert(0);
        *c += 1;
        if *c == 1 {
            for &o in &self.locals {
                Self::mark_pair(inter, l, o);
            }
            self.locals.insert(l);
        }
    }

    fn del_local(&mut self, l: Local) {
        if let Some(c) = self.local_refs.get_mut(&l) {
            *c -= 1;
            if *c == 0 {
                self.local_refs.remove(&l);
                self.locals.remove(&l);
            }
        }
    }

    fn add_val(&mut self, inter: &mut [Bitset], results: &Localifier, v: Value) {
        if !self.vals.insert(v) {
            return;
        }
        for &l in &results.values[v] {
            self.add_local(inter, l);
        }
    }

    fn del_val(&mut self, results: &Localifier, v: Value) {
        if !self.vals.remove(&v) {
            return;
        }
        for &l in &results.values[v] {
            self.del_local(l);
        }
    }

    fn mark_against_live(&self, inter: &mut [Bitset], results: &Localifier, v: Value) {
        for &l in &results.values[v] {
            for &o in &self.locals {
                Self::mark_pair(inter, l, o);
            }
        }
    }
}

/// Invoke `f` for every non-treeified Value under `value` (tree/remat
/// ops are expanded to their operands, matching `BlockVisitor`).
fn for_each_local_value(
    body: &FunctionBody,
    trees: &Trees,
    value: Value,
    mut f: impl FnMut(Value),
) {
    fn walk(body: &FunctionBody, trees: &Trees, value: Value, f: &mut dyn FnMut(Value)) {
        let value = body.resolve_alias(value);
        if let ValueDef::PickOutput(v, _, _) = body.values[value] {
            walk(body, trees, v, f);
            return;
        }
        if is_tree_or_remat(value, trees) {
            if let ValueDef::Operator(_, args, _) = &body.values[value] {
                for &arg in &body.arg_pool[*args] {
                    walk(body, trees, arg, f);
                }
            }
            return;
        }
        f(value);
    }
    walk(body, trees, value, &mut f);
}

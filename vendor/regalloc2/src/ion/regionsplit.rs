//! PLIRON `PLIRON_RA2_REGION=1` (opt-in): region splitting by min-cut
//! spill placement, after LLVM's `RAGreedy::tryRegionSplit` /
//! `SpillPlacement`.
//!
//! regalloc2 splits a bundle that does not fit at its first conflict, and
//! when the pieces conflict again (a long value live across several
//! calls, or through a loop nest that wants every register) it falls back
//! to minimal bundles: a reload at every use, with the spill bundle's
//! second-chance register often a different one, so every use also costs
//! a register move. LLVM instead decides, per block and for one physical
//! register at a time, where the value lives in that register and where
//! on the stack, minimizing the frequency-weighted cost of the moves on
//! the edges between the two and of the reloads in the stack blocks.
//!
//! Here the problem is solved exactly: the blocks the bundle is live in
//! are the nodes of a graph, labelled R (in the register) or S (on the
//! stack). A block where the register is not free over the bundle's part
//! of it must be S. An S block costs its uses (each a reload or a store
//! at the block's frequency, capped: a block-local piece reloads once per
//! stretch); an edge the bundle is live across costs its frequency when
//! its ends differ. The labelling with the least total cost is a minimum
//! s-t cut (the pairwise costs are symmetric, so the energy is
//! submodular), found with Dinic's max-flow under an operation budget.
//! The few registers with the largest possible gain are each tried; the
//! cheapest cut wins if it beats putting the whole bundle on the stack.
//!
//! The split itself only cuts ranges at block boundaries: the R blocks
//! become one bundle (with holes) that is assigned the register at once;
//! each S block with register uses keeps a block-local piece from its
//! first to its last use, which is requeued (it may still find another
//! register, or splits locally); the rest goes to the spill bundle. The
//! moves between the pieces come from the normal edge and range move
//! insertion (`moves.rs`). Under `PLIRON_RA2_REGION=2` an S component of
//! several blocks (S blocks joined by edges the bundle is live across)
//! with uses is requeued as one bundle instead, so a loop the chosen
//! register is busy in can still take another register whole.
//!
//! Termination and cost: the register part remembers its parent's block
//! count (`LiveBundle::region_blocks`) and is region-split again, after an
//! eviction, only while it has fewer blocks (LLVM's `RS_Split2` guard);
//! at most `PLIRON_RA2_REGION_MAX` (4) region splits per spillset, bundles
//! live in more than `PLIRON_RA2_REGION_MAXB` (2048) blocks are left to the
//! ordinary split, `PLIRON_RA2_REGION_K` (8) registers are cut per try, and
//! each max-flow has an operation budget linear in the graph.

use super::{
    AllocRegResult, Env, LiveBundleIndex, LiveBundleVec, LiveRangeFlag, LiveRangeKey,
    LiveRangeList, LiveRangeListEntry, PRegIndex, RegTraversalIter,
};
use crate::{
    ion::data_structures::CodeRange, Block, Function, OperandConstraint, PReg, ProgPoint,
};
use alloc::vec;
use alloc::vec::Vec;

/// Per block of the bundle.
struct BlockInfo {
    block: u32,
    /// Cost if the block is on the stack (S).
    cost_s: f32,
    /// Number of uses/defs that need a register.
    reg_uses: u32,
    live_in: bool,
    live_out: bool,
    /// Pieces `[lo, hi)` in `pieces`.
    lo: u32,
    hi: u32,
}

/// Dinic max-flow on a small graph with an operation budget.
struct Flow {
    head: Vec<u32>,
    next: Vec<u32>,
    to: Vec<u32>,
    cap: Vec<i64>,
    level: Vec<i32>,
    it: Vec<u32>,
}

const NONE: u32 = u32::MAX;

impl Flow {
    fn new(n: usize) -> Self {
        Flow {
            head: vec![NONE; n],
            next: Vec::new(),
            to: Vec::new(),
            cap: Vec::new(),
            level: vec![0; n],
            it: vec![NONE; n],
        }
    }

    fn add(&mut self, u: usize, v: usize, c_uv: i64, c_vu: i64) {
        let a = self.to.len() as u32;
        self.to.push(v as u32);
        self.cap.push(c_uv);
        self.next.push(self.head[u]);
        self.head[u] = a;
        self.to.push(u as u32);
        self.cap.push(c_vu);
        self.next.push(self.head[v]);
        self.head[v] = a + 1;
    }

    fn bfs(&mut self, s: usize, ops: &mut usize) -> Vec<u32> {
        for l in self.level.iter_mut() {
            *l = -1;
        }
        let mut q = vec![s as u32];
        self.level[s] = 0;
        let mut i = 0;
        while i < q.len() {
            let u = q[i] as usize;
            i += 1;
            let mut a = self.head[u];
            while a != NONE {
                *ops += 1;
                let v = self.to[a as usize] as usize;
                if self.cap[a as usize] > 0 && self.level[v] < 0 {
                    self.level[v] = self.level[u] + 1;
                    q.push(v as u32);
                }
                a = self.next[a as usize];
            }
        }
        q
    }

    /// Max flow, or `None` when the budget runs out.
    fn max_flow(&mut self, s: usize, t: usize, budget: usize) -> Option<i64> {
        let mut flow = 0i64;
        let mut ops = 0usize;
        let mut path: Vec<u32> = Vec::new();
        loop {
            self.bfs(s, &mut ops);
            if self.level[t] < 0 {
                return Some(flow);
            }
            if ops > budget {
                return None;
            }
            self.it.copy_from_slice(&self.head);
            path.clear();
            let mut u = s;
            loop {
                if u == t {
                    let mut f = i64::MAX;
                    for &a in &path {
                        f = f.min(self.cap[a as usize]);
                    }
                    for &a in &path {
                        self.cap[a as usize] -= f;
                        self.cap[(a ^ 1) as usize] += f;
                    }
                    flow += f;
                    path.clear();
                    u = s;
                    continue;
                }
                let mut advanced = false;
                while self.it[u] != NONE {
                    ops += 1;
                    let a = self.it[u] as usize;
                    let v = self.to[a] as usize;
                    if self.cap[a] > 0 && self.level[v] == self.level[u] + 1 {
                        path.push(a as u32);
                        u = v;
                        advanced = true;
                        break;
                    }
                    self.it[u] = self.next[a];
                }
                if ops > budget {
                    return None;
                }
                if !advanced {
                    if u == s {
                        break;
                    }
                    // Dead end: retreat and skip the arc that led here.
                    self.level[u] = -1;
                    let a = path.pop().unwrap() as usize;
                    u = self.to[a ^ 1] as usize;
                    self.it[u] = self.next[a];
                }
            }
        }
    }

    /// Nodes reachable from `s` in the residual graph (the source side
    /// of a minimum cut).
    fn source_side(&mut self, s: usize) -> Vec<bool> {
        let mut ops = 0;
        self.bfs(s, &mut ops);
        self.level.iter().map(|&l| l >= 0).collect()
    }
}

fn to_cap(x: f32) -> i64 {
    (x * 4.0).round().max(0.0) as i64
}

impl<'a, F: Function> Env<'a, F> {
    /// Try the region split on `bundle` (see the module comment). Returns
    /// false, with nothing changed, when it does not apply or does not
    /// pay.
    pub(crate) fn try_region_split(&mut self, bundle: LiveBundleIndex, hint: PReg) -> bool {
        let spillset = self.ctx.bundles[bundle].spillset;
        if self.ctx.spillsets[spillset].region_count >= crate::region_max()
            || self.ctx.bundles[bundle].limit.is_some()
            || self.minimal_bundle(bundle)
            || self.ctx.bundles[bundle].ranges.is_empty()
        {
            return false;
        }
        let max_blocks = crate::region_max_blocks();
        let cap = crate::region_use_cap();

        // Pieces: the bundle's ranges cut at block boundaries, in order.
        let mut pieces: Vec<(u32, ProgPoint, ProgPoint)> = Vec::new();
        let mut infos: Vec<BlockInfo> = Vec::new();
        {
            let cfg = &self.ctx.cfginfo;
            for entry in &self.ctx.bundles[bundle].ranges {
                let (from, to) = (entry.range.from, entry.range.to);
                if from >= to {
                    continue;
                }
                let b0 = cfg.insn_block[from.inst().index()].index();
                let b1 = cfg.insn_block[to.prev().inst().index()].index();
                if pieces.len() + (b1 - b0 + 1) > 4 * max_blocks {
                    return false;
                }
                let uses = &self.ctx.ranges[entry.index].uses;
                let mut ui = 0;
                for b in b0..=b1 {
                    let pf = core::cmp::max(from, cfg.block_entry[b]);
                    let pt = core::cmp::min(to, cfg.block_exit[b].next());
                    if pf >= pt {
                        continue;
                    }
                    let fresh = infos.last().map_or(true, |i| i.block as usize != b);
                    if fresh {
                        if infos.len() >= max_blocks {
                            return false;
                        }
                        infos.push(BlockInfo {
                            block: b as u32,
                            cost_s: 0.0,
                            reg_uses: 0,
                            live_in: false,
                            live_out: false,
                            lo: pieces.len() as u32,
                            hi: pieces.len() as u32,
                        });
                    }
                    let info = infos.last_mut().unwrap();
                    pieces.push((b as u32, pf, pt));
                    info.hi = pieces.len() as u32;
                    info.live_in |= pf == cfg.block_entry[b];
                    info.live_out |= pt == cfg.block_exit[b].next();
                    while ui < uses.len() && uses[ui].pos < pt {
                        if uses[ui].operand.constraint() != OperandConstraint::Any {
                            info.reg_uses += 1;
                        }
                        ui += 1;
                    }
                }
            }
            let prev = self.ctx.bundles[bundle].region_blocks as usize;
            if infos.len() < 2 || (prev != 0 && infos.len() >= prev) {
                return false;
            }
            for info in infos.iter_mut() {
                let n = info.reg_uses.min(cap) as f32;
                info.cost_s = cfg.hot_bonus[info.block as usize] * n;
            }
        }
        let find = |infos: &[BlockInfo], b: u32| infos.binary_search_by_key(&b, |i| i.block).ok();

        // Edges the bundle is live across: (i, j, cost).
        let mut edges: Vec<(u32, u32, f32)> = Vec::new();
        for (i, info) in infos.iter().enumerate() {
            if !info.live_out {
                continue;
            }
            let hb = self.ctx.cfginfo.hot_bonus[info.block as usize];
            for &s in self.func.block_succs(Block::new(info.block as usize)) {
                if let Some(j) = find(&infos, s.index() as u32) {
                    if infos[j].live_in && j != i {
                        let w = hb.min(self.ctx.cfginfo.hot_bonus[s.index()]);
                        edges.push((i as u32, j as u32, w));
                    }
                }
            }
        }

        // Which blocks each register is free in, and the most it can gain.
        let class = self.ctx.spillsets[spillset].class;
        let scan_offset = self.ctx.ranges[self.ctx.bundles[bundle].ranges[0].index]
            .range
            .from
            .inst()
            .index()
            + bundle.index();
        let mut cands: Vec<(f32, PReg, Vec<bool>)> = Vec::new();
        let total_s: f32 = infos.iter().map(|i| i.cost_s).sum();
        for preg in RegTraversalIter::new(self.env, class, None, hint.as_valid(), scan_offset, None)
        {
            let btree = &self.ctx.pregs[preg.index()].allocations.btree;
            let mut free = vec![true; infos.len()];
            let mut gain = 0.0f32;
            let mut any_busy = false;
            for (k, info) in infos.iter().enumerate() {
                for p in info.lo..info.hi {
                    let (_, from, to) = pieces[p as usize];
                    let key = LiveRangeKey::from_range(&CodeRange { from, to });
                    if let Some((k2, _)) = btree.range(key..).next() {
                        if k2.to_range().from < to {
                            free[k] = false;
                            break;
                        }
                    }
                }
                if free[k] {
                    gain += info.cost_s;
                } else {
                    any_busy = true;
                }
            }
            explain!(
                "    region-cand {:?}: gain {gain} free {}/{} busy {any_busy}",
                preg,
                free.iter().filter(|&&f| f).count(),
                free.len()
            );
            if !any_busy || !(gain > 0.0) {
                // Free everywhere: the probe would have taken it.
                continue;
            }
            cands.push((gain, preg, free));
        }
        if cands.is_empty() {
            return false;
        }
        // Largest possible gain first; among equal gains (free in every use
        // block), the register free in the most blocks: fewer blocks forced
        // to the stack, fewer transitions. Without the tie-break the
        // caller-saved registers, probed first, filled `K` and a callee-saved
        // one free across a loop was never cut (blake3's
        // `compress_chunks_parallel` reloaded two values on a loop exit).
        // `PLIRON_RA2_REGION_TIE=0`: traversal order.
        if crate::region_tie() {
            cands.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(core::cmp::Ordering::Equal)
                    .then_with(|| {
                        let fa = a.2.iter().filter(|&&f| f).count();
                        let fb = b.2.iter().filter(|&&f| f).count();
                        fb.cmp(&fa)
                    })
            });
        } else {
            cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(core::cmp::Ordering::Equal));
        }
        cands.truncate(crate::region_k());

        let n = infos.len();
        let budget = 64 * (n + edges.len()) + 4096;
        let mut best: Option<(f32, PReg, Vec<bool>)> = None;
        for (gain, preg, free) in cands {
            let (s, t) = (n, n + 1);
            let mut fl = Flow::new(n + 2);
            let mut forced = 0.0f32;
            for (k, info) in infos.iter().enumerate() {
                if free[k] {
                    let c = to_cap(info.cost_s);
                    if c > 0 {
                        fl.add(s, k, c, 0);
                    }
                } else {
                    forced += info.cost_s;
                }
            }
            for &(i, j, w) in &edges {
                let (i, j) = (i as usize, j as usize);
                let c = to_cap(w);
                match (free[i], free[j]) {
                    (true, true) => fl.add(i, j, c, c),
                    (true, false) => fl.add(i, t, c, 0),
                    (false, true) => fl.add(j, t, c, 0),
                    (false, false) => {}
                }
            }
            let Some(f) = fl.max_flow(s, t, budget) else {
                explain!("  region {:?}: budget out on {:?}", bundle, preg);
                continue;
            };
            let side = fl.source_side(s);
            let in_r: Vec<bool> = (0..n).map(|k| free[k] && side[k]).collect();
            let cost = f as f32 / 4.0 + forced;
            explain!(
                "  region {:?} {:?}: gain<= {gain} cost {cost} (all-stack {total_s}), {} of {n} blocks in reg",
                bundle,
                preg,
                in_r.iter().filter(|&&r| r).count()
            );
            if !in_r.iter().zip(infos.iter()).any(|(&r, i)| r && i.reg_uses > 0) {
                continue;
            }
            if best.as_ref().map_or(true, |b| cost < b.0) {
                best = Some((cost, preg, in_r));
            }
        }
        let Some((cost, preg, in_r)) = best else {
            return false;
        };
        if !(cost < total_s * crate::region_ratio()) {
            explain!("  region {:?}: best {cost} not below {} x all-stack {total_s}", bundle, crate::region_ratio());
            return false;
        }
        // Stack components: S blocks joined by edges the bundle is live
        // across (mode 2). A component of several blocks with uses is
        // requeued as one bundle, so it can still take another register
        // as a whole (a loop the chosen register is busy in).
        let n = infos.len();
        let mut comp: Vec<u32> = (0..n as u32).collect();
        if crate::region_mode() >= 2 {
            fn root(c: &mut [u32], mut x: u32) -> u32 {
                while c[x as usize] != x {
                    let p = c[c[x as usize] as usize];
                    c[x as usize] = p;
                    x = p;
                }
                x
            }
            for &(i, j, _) in &edges {
                if !in_r[i as usize] && !in_r[j as usize] {
                    let (a, b) = (root(&mut comp, i), root(&mut comp, j));
                    if a != b {
                        comp[a.max(b) as usize] = a.min(b);
                    }
                }
            }
            for k in 0..n as u32 {
                comp[k as usize] = root(&mut comp, k);
            }
        }
        self.apply_region_split(bundle, preg, &infos, &in_r, &comp);
        true
    }

    fn apply_region_split(
        &mut self,
        bundle: LiveBundleIndex,
        preg: PReg,
        infos: &[BlockInfo],
        in_r: &[bool],
        comp: &[u32],
    ) {
        let spillset = self.ctx.bundles[bundle].spillset;
        self.ctx.spillsets[spillset].region_count += 1;
        self.ctx.bundles[bundle].region_blocks = infos.len() as u32;
        self.ctx.output.stats.splits += 1;
        let label = |b: usize| -> Option<bool> {
            infos
                .binary_search_by_key(&(b as u32), |i| i.block)
                .ok()
                .map(|k| in_r[k])
        };

        // Cut every range at each block boundary where the label changes,
        // and at every block boundary inside S stretches (block-local
        // pieces).
        let empty = LiveRangeList::new_in(self.ctx.bump());
        let old = core::mem::replace(&mut self.ctx.bundles[bundle].ranges, empty);
        let mut reg_part = LiveRangeList::new_in(self.ctx.bump());
        let mut stack_part: Vec<LiveRangeListEntry> = Vec::new();
        for entry in old.iter().cloned() {
            let mut cur = entry;
            loop {
                let (b0, b1, lab) = {
                    let cfg = &self.ctx.cfginfo;
                    let b0 = cfg.insn_block[cur.range.from.inst().index()].index();
                    let b1 = cfg.insn_block[cur.range.to.prev().inst().index()].index();
                    (b0, b1, label(b0).unwrap_or(false))
                };
                // The first block after b0 where the piece must end.
                let mut cut = None;
                if !lab {
                    if b1 > b0 {
                        cut = Some(b0 + 1);
                    }
                } else {
                    for b in (b0 + 1)..=b1 {
                        if label(b) != Some(true) {
                            cut = Some(b);
                            break;
                        }
                    }
                }
                match cut {
                    None => {
                        if lab {
                            reg_part.push(cur);
                        } else {
                            stack_part.push(cur);
                        }
                        break;
                    }
                    Some(b) => {
                        let at = self.ctx.cfginfo.block_entry[b];
                        let tail = self.cold_split_range(cur.index, at);
                        cur.range.to = at;
                        if lab {
                            reg_part.push(cur);
                        } else {
                            stack_part.push(cur);
                        }
                        cur = LiveRangeListEntry {
                            range: self.ctx.ranges[tail].range,
                            index: tail,
                        };
                    }
                }
            }
        }

        // The stack part: block-local pieces from the first to the last use
        // that needs a register, the rest in the spill bundle.
        let spill = self
            .get_or_create_spill_bundle(bundle, /* create_if_absent = */ true)
            .unwrap();
        let mut locals: Vec<LiveBundleIndex> = Vec::new();
        // Per component root: blocks, register uses, and its bundle.
        let n = infos.len();
        let mut csize = vec![0u32; n];
        let mut cuses = vec![0u32; n];
        for k in 0..n {
            if !in_r[k] {
                csize[comp[k] as usize] += 1;
                cuses[comp[k] as usize] += infos[k].reg_uses;
            }
        }
        let mut cbundle: Vec<LiveBundleIndex> = vec![LiveBundleIndex::invalid(); n];
        let block_of = |this: &Self, e: &LiveRangeListEntry| -> usize {
            let b = this.ctx.cfginfo.insn_block[e.range.from.inst().index()].index() as u32;
            infos.binary_search_by_key(&b, |i| i.block).unwrap()
        };
        for entry in stack_part {
            let k = block_of(self, &entry);
            let c = comp[k] as usize;
            if csize[c] > 1 && cuses[c] > 0 {
                if cbundle[c].is_invalid() {
                    let piece = self.ctx.bundles.add(self.ctx.scratch_bump.clone());
                    self.ctx.bundles[piece].spillset = spillset;
                    self.ctx.bundles[piece].cold_split = self.ctx.bundles[bundle].cold_split;
                    self.ctx.bundles[piece].region_blocks = infos.len() as u32;
                    cbundle[c] = piece;
                    locals.push(piece);
                }
                let piece = cbundle[c];
                self.ctx.ranges[entry.index].bundle = piece;
                self.ctx.bundles[piece].ranges.push(entry);
                continue;
            }
            let lr = entry.index;
            let uses = &self.ctx.ranges[lr].uses;
            let first = uses
                .iter()
                .find(|u| u.operand.constraint() != OperandConstraint::Any)
                .map(|u| u.pos);
            let last = uses
                .iter()
                .rev()
                .find(|u| u.operand.constraint() != OperandConstraint::Any)
                .map(|u| u.pos);
            let (Some(first), Some(last)) = (first, last) else {
                self.ctx.ranges[lr].bundle = spill;
                self.ctx.bundles[spill].ranges.push(entry);
                continue;
            };
            let mut cur = entry;
            // Leading gap (unless the range starts at its def).
            let lead_to = ProgPoint::before(first.inst());
            if !self.ctx.ranges[lr].has_flag(LiveRangeFlag::StartsAtDef)
                && lead_to > cur.range.from
                && !self.ctx.ranges[lr]
                    .uses
                    .iter()
                    .any(|u| u.pos < lead_to)
            {
                let tail = self.cold_split_range(cur.index, lead_to);
                cur.range.to = lead_to;
                self.ctx.ranges[cur.index].bundle = spill;
                self.ctx.bundles[spill].ranges.push(cur);
                cur = LiveRangeListEntry {
                    range: self.ctx.ranges[tail].range,
                    index: tail,
                };
            }
            // Trailing gap.
            let trail_from = ProgPoint::before(last.inst().next());
            let mut trail = None;
            if trail_from < cur.range.to
                && trail_from > cur.range.from
                && !self.ctx.ranges[cur.index]
                    .uses
                    .iter()
                    .any(|u| u.pos >= trail_from)
            {
                let tail = self.cold_split_range(cur.index, trail_from);
                cur.range.to = trail_from;
                trail = Some(LiveRangeListEntry {
                    range: self.ctx.ranges[tail].range,
                    index: tail,
                });
            }
            let piece = self.ctx.bundles.add(self.ctx.scratch_bump.clone());
            self.ctx.bundles[piece].spillset = spillset;
            self.ctx.bundles[piece].cold_split = self.ctx.bundles[bundle].cold_split;
            self.ctx.ranges[cur.index].bundle = piece;
            self.ctx.bundles[piece].ranges.push(cur);
            locals.push(piece);
            if let Some(t) = trail {
                self.ctx.ranges[t.index].bundle = spill;
                self.ctx.bundles[spill].ranges.push(t);
            }
        }

        // The register part: assign it at once (it is free there).
        for entry in &reg_part {
            self.ctx.ranges[entry.index].bundle = bundle;
        }
        self.ctx.bundles[bundle].ranges = reg_part;
        explain!(
            "  region split -> {:?}: reg part {}, {} local piece(s)",
            preg,
            self.explain_bundle(bundle),
            locals.len()
        );
        self.recompute_bundle_properties(bundle);
        let mut scratch = LiveBundleVec::new();
        match self.try_to_allocate_bundle_to_reg(
            bundle,
            PRegIndex::new(preg.index()),
            None,
            &mut scratch,
        ) {
            AllocRegResult::Allocated(_) => {
                self.ctx.spillsets[spillset].hint = preg;
            }
            _ => {
                debug_assert!(false, "region split: reg part did not fit {:?}", preg);
                let prio = self.ctx.bundles[bundle].prio;
                self.ctx
                    .allocation_queue
                    .insert(bundle, prio as usize, preg);
            }
        }
        for piece in locals {
            self.recompute_bundle_properties(piece);
            explain!("    local piece {}", self.explain_bundle(piece));
            let prio = self.ctx.bundles[piece].prio;
            self.ctx
                .allocation_queue
                .insert(piece, prio as usize, preg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Flow;

    /// The classic six-node network (CLRS 26.1): max flow 23, and the
    /// source side of the minimum cut is {s, v1, v2, v4}.
    #[test]
    fn dinic_clrs() {
        let (s, v1, v2, v3, v4, t) = (0, 1, 2, 3, 4, 5);
        let mut f = Flow::new(6);
        f.add(s, v1, 16, 0);
        f.add(s, v2, 13, 0);
        f.add(v2, v1, 4, 0);
        f.add(v1, v3, 12, 0);
        f.add(v3, v2, 9, 0);
        f.add(v2, v4, 14, 0);
        f.add(v4, v3, 7, 0);
        f.add(v3, t, 20, 0);
        f.add(v4, t, 4, 0);
        assert_eq!(f.max_flow(s, t, usize::MAX), Some(23));
        let side = f.source_side(s);
        assert_eq!(side, [true, true, true, false, true, false]);
    }

    /// The region-split energy: a chain of blocks a-b-c-d with the
    /// register busy in c (c is tied to the sink). Uses in a and d cost 10
    /// each on the stack, b's costs 1, every edge costs 1: the cut keeps
    /// a, b in the register, puts c on the stack and d back in the
    /// register (cost 2).
    #[test]
    fn chain_cut() {
        let (a, b, d, s, t) = (0, 1, 2, 3, 4);
        let mut f = Flow::new(5);
        f.add(s, a, 10, 0);
        f.add(s, d, 10, 0);
        f.add(s, b, 1, 0); // one cheap use in b breaks the tie
        f.add(a, b, 1, 1);
        f.add(b, t, 1, 0); // edge b-c, c forced to the stack
        f.add(d, t, 1, 0); // edge c-d
        assert_eq!(f.max_flow(s, t, usize::MAX), Some(2));
        let side = f.source_side(s);
        assert!(side[a] && side[b] && side[d]);
    }

    /// An exhausted budget is reported, not a wrong flow.
    #[test]
    fn budget() {
        let mut f = Flow::new(3);
        f.add(0, 1, 5, 0);
        f.add(1, 2, 5, 0);
        assert_eq!(f.max_flow(0, 2, 0), None);
    }
}

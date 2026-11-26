/*
 * This file was initially derived from the files
 * `js/src/jit/BacktrackingAllocator.h` and
 * `js/src/jit/BacktrackingAllocator.cpp` in Mozilla Firefox, and was
 * originally licensed under the Mozilla Public License 2.0. We
 * subsequently relicensed it to Apache-2.0 WITH LLVM-exception (see
 * https://github.com/bytecodealliance/regalloc2/issues/7).
 *
 * Since the initial port, the design has been substantially evolved
 * and optimized.
 */

//! Bundle merging.

use crate::ion::data_structures::{
    BlockparamOut, CodeRange, Env, LiveBundleIndex, LiveRangeKey, LiveRangeList,
    LiveRangeListEntry, SpillSet, SpillSlotIndex, VRegIndex,
};
use crate::ion::requirement::Requirement;
use crate::{Function, FxHashSet, Inst, OperandConstraint, OperandKind, PReg, ProgPoint};
use alloc::format;
use core::convert::TryFrom;

impl<'a, F: Function> Env<'a, F> {
    fn merge_bundle_properties(&mut self, from: LiveBundleIndex, to: LiveBundleIndex) {
        if self.bundles[from].cached_fixed() {
            self.bundles[to].set_cached_fixed();
        }
        if self.bundles[from].cached_fixed_def() {
            self.bundles[to].set_cached_fixed_def();
        }
        if self.bundles[from].cached_stack() {
            self.bundles[to].set_cached_stack();
        }
        if let Some(theirs) = self.bundles[from].limit {
            match self.bundles[to].limit {
                Some(ours) => self.bundles[to].limit = Some(ours.min(theirs)),
                None => self.bundles[to].limit = Some(theirs),
            }
        }
    }

    pub fn merge_bundles(&mut self, from: LiveBundleIndex, to: LiveBundleIndex) -> bool {
        if from == to {
            // Merge bundle into self -- trivial merge.
            return true;
        }
        trace!(
            "merging from bundle{} to bundle{}",
            from.index(),
            to.index()
        );

        // Both bundles must deal with the same RegClass.
        let from_rc = self.spillsets[self.bundles[from].spillset].class;
        let to_rc = self.spillsets[self.bundles[to].spillset].class;
        if from_rc != to_rc {
            trace!(" -> mismatching reg classes");
            self.ctx.last_merge_fail = 6;
            return false;
        }

        // If either bundle is already assigned (due to a pinned vreg), don't merge.
        if self.bundles[from].allocation.is_some() || self.bundles[to].allocation.is_some() {
            trace!("one of the bundles is already assigned (pinned)");
            self.ctx.last_merge_fail = 6;
            return false;
        }

        #[cfg(debug_assertions)]
        {
            // Sanity check: both bundles should contain only ranges with appropriate VReg classes.
            for entry in &self.bundles[from].ranges {
                let vreg = self.ranges[entry.index].vreg;
                debug_assert_eq!(from_rc, self.vreg(vreg).class());
            }
            for entry in &self.bundles[to].ranges {
                let vreg = self.ranges[entry.index].vreg;
                debug_assert_eq!(to_rc, self.vreg(vreg).class());
            }
        }

        // If a bundle has a fixed-reg def then we need to be careful to not
        // extend the bundle to include another use in the same instruction.
        // This could result in a minimal bundle that is impossible to split.
        //
        // This can only happen with an early use and a late def, so we round
        // the start of each range containing a fixed def up to the start of
        // its instruction to detect overlaps.
        let adjust_range_start = |bundle_idx, range: CodeRange| {
            if self.bundles[bundle_idx].cached_fixed_def() {
                ProgPoint::before(range.from.inst())
            } else {
                range.from
            }
        };

        // Check for overlap in LiveRanges and for conflicting
        // requirements.
        let ranges_from = &self.bundles[from].ranges[..];
        let ranges_to = &self.bundles[to].ranges[..];
        let mut idx_from = 0;
        let mut idx_to = 0;
        let mut range_count = 0;
        while idx_from < ranges_from.len() && idx_to < ranges_to.len() {
            range_count += 1;
            if range_count > 200 {
                trace!(
                    "reached merge complexity (range_count = {}); exiting",
                    range_count
                );
                // Limit merge complexity.
                self.ctx.last_merge_fail = 6;
                return false;
            }

            if adjust_range_start(from, ranges_from[idx_from].range) >= ranges_to[idx_to].range.to {
                idx_to += 1;
            } else if adjust_range_start(to, ranges_to[idx_to].range)
                >= ranges_from[idx_from].range.to
            {
                idx_from += 1;
            } else {
                // Overlap -- cannot merge.
                trace!(
                    " -> overlap between {:?} and {:?}, exiting",
                    ranges_from[idx_from].index,
                    ranges_to[idx_to].index
                );
                {
                    let (rf, rt) = (ranges_from[idx_from], ranges_to[idx_to]);
                    explain!(
                        "  merge overlap: v{} {:?} (b{}..b{}) vs v{} {:?} (b{}..b{})",
                        self.ranges[rf.index].vreg.index(),
                        rf.range,
                        self.ctx.cfginfo.insn_block[rf.range.from.inst().index()].index(),
                        self.ctx.cfginfo.insn_block[rf.range.to.inst().index()].index(),
                        self.ranges[rt.index].vreg.index(),
                        rt.range,
                        self.ctx.cfginfo.insn_block[rt.range.from.inst().index()].index(),
                        self.ctx.cfginfo.insn_block[rt.range.to.inst().index()].index(),
                    );
                }
                self.ctx.last_merge_fail = 4;
                return false;
            }
        }

        // Check for a requirements conflict.
        if self.bundles[from].cached_stack()
            || self.bundles[from].cached_fixed()
            || self.bundles[from].limit.is_some()
            || self.bundles[to].cached_stack()
            || self.bundles[to].cached_fixed()
            || self.bundles[to].limit.is_some()
        {
            if self.merge_bundle_requirements(from, to).is_err() {
                trace!(" -> conflicting requirements; aborting merge");
                self.ctx.last_merge_fail = 5;
                return false;
            }
        }

        trace!(" -> committing to merge");

        // If we reach here, then the bundles do not overlap -- merge
        // them!  We do this with a merge-sort-like scan over both
        // lists, building a new range list and replacing the list on
        // `to` when we're done.
        if ranges_from.is_empty() {
            // `from` bundle is empty -- trivial merge.
            trace!(" -> from bundle{} is empty; trivial merge", from.index());
            return true;
        }
        if ranges_to.is_empty() {
            // `to` bundle is empty -- just move the list over from
            // `from` and set `bundle` up-link on all ranges.
            trace!(" -> to bundle{} is empty; trivial merge", to.index());
            let empty_vec = LiveRangeList::new_in(self.ctx.bump());
            let list = core::mem::replace(&mut self.bundles[from].ranges, empty_vec);
            for entry in &list {
                self.ranges[entry.index].bundle = to;

                if self.annotations_enabled {
                    self.annotate(
                        entry.range.from,
                        format!(
                            " MERGE range{} v{} from bundle{} to bundle{}",
                            entry.index.index(),
                            self.ranges[entry.index].vreg.index(),
                            from.index(),
                            to.index(),
                        ),
                    );
                }
            }
            self.bundles[to].ranges = list;
            self.merge_bundle_properties(from, to);
            return true;
        }

        trace!(
            "merging: ranges_from = {:?} ranges_to = {:?}",
            ranges_from,
            ranges_to
        );

        let empty_vec = LiveRangeList::new_in(self.ctx.bump());
        let mut from_list = core::mem::replace(&mut self.bundles[from].ranges, empty_vec);
        for entry in &from_list {
            self.ranges[entry.index].bundle = to;
        }

        if from_list.len() == 1 {
            // Optimize for the common case where `from_list` contains a single
            // item. Using a binary search to find the insertion point and then
            // calling `insert` is more efficient than re-sorting the entire
            // list, specially after the changes in sorting algorithms introduced
            // in rustc 1.81.
            // See: https://github.com/bytecodealliance/regalloc2/issues/203
            let single_entry = from_list.pop().unwrap();
            let pos = self.bundles[to]
                .ranges
                .binary_search_by_key(&single_entry.range.from, |entry| entry.range.from)
                .unwrap_or_else(|pos| pos);
            self.bundles[to].ranges.insert(pos, single_entry);
        } else {
            // Two non-empty lists of LiveRanges: concatenate and sort. This is
            // faster than a mergesort-like merge into a new list, empirically.
            self.bundles[to].ranges.extend_from_slice(&from_list[..]);
            self.bundles[to]
                .ranges
                .sort_unstable_by_key(|entry| entry.range.from);
        }

        if self.annotations_enabled {
            trace!("merging: merged = {:?}", self.bundles[to].ranges);
            let mut last_range = None;
            for i in 0..self.bundles[to].ranges.len() {
                let entry = self.bundles[to].ranges[i];
                if last_range.is_some() {
                    debug_assert!(last_range.unwrap() < entry.range);
                }
                last_range = Some(entry.range);

                if self.ranges[entry.index].bundle == from {
                    self.annotate(
                        entry.range.from,
                        format!(
                            " MERGE range{} v{} from bundle{} to bundle{}",
                            entry.index.index(),
                            self.ranges[entry.index].vreg.index(),
                            from.index(),
                            to.index(),
                        ),
                    );
                }

                trace!(
                    " -> merged result for bundle{}: range{}",
                    to.index(),
                    entry.index.index(),
                );
            }
        }

        if self.bundles[from].spillset != self.bundles[to].spillset {
            // Widen the range for the target spillset to include the one being merged in.
            let from_range = self.spillsets[self.bundles[from].spillset].range;
            let to_range = &mut self.ctx.spillsets[self.ctx.bundles[to].spillset].range;
            *to_range = to_range.join(from_range);
        }

        self.merge_bundle_properties(from, to);

        true
    }

    /// PLIRON (`PLIRON_RA2_NOPINMERGE`): would merging `from` into `to`
    /// pin an unconstrained side onto a preg it can't actually keep?
    /// True when exactly one side has a `FixedReg`/`FixedStack`
    /// constraint, the merged requirement is a fixed preg, and some
    /// range of the unconstrained side overlaps a fixed reservation
    /// (clobber) on that preg — pinning there forces a split where an
    /// edge move would do.
    fn pin_spread_hurts(&mut self, from: LiveBundleIndex, to: LiveBundleIndex) -> bool {
        let f = self.bundles[from].cached_fixed() || self.bundles[from].cached_stack();
        let t = self.bundles[to].cached_fixed() || self.bundles[to].cached_stack();
        if f == t {
            return false;
        }
        let preg = match self.merge_bundle_requirements(from, to) {
            Ok(Requirement::FixedReg(p)) | Ok(Requirement::FixedStack(p)) => p,
            _ => return false,
        };
        let side = if f { to } else { from };
        for entry in &self.bundles[side].ranges {
            let r = entry.range;
            let probe = LiveRangeKey::from_range(&CodeRange {
                from: r.from,
                to: r.to,
            });
            for (key, lr) in self.ctx.pregs[preg.index()]
                .allocations
                .btree
                .range(probe..)
            {
                if key.to_range().from >= r.to {
                    break;
                }
                if !lr.is_valid() {
                    explain!(
                        "  nopinmerge: refuse {:?}->{:?}; range {:?} crosses fixed use of {:?}",
                        from,
                        to,
                        r,
                        preg
                    );
                    return true;
                }
            }
        }
        false
    }

    /// PLIRON (`PLIRON_RA2_EVICT`): the block-frequency weight of a
    /// blockparam edge move — the move lands where
    /// `moves.rs::choose_move_location` would put it: at the end of
    /// `from_block` when `to_block` has several predecessors, at the
    /// head of `to_block` (an edge stub) otherwise.
    fn bp_edge_weight(&self, from_block: crate::Block, to_block: crate::Block) -> f32 {
        let to_preds = self.func.block_preds(to_block).len()
            + usize::from(self.func.entry_block() == to_block);
        let blk = if to_preds > 1 { from_block } else { to_block };
        self.ctx
            .cfginfo
            .hot_bonus
            .get(blk.index())
            .copied()
            .unwrap_or(1.0)
            .max(1.0)
    }

    /// PLIRON (`PLIRON_RA2_EVICT`): recompute a bundle's
    /// fixed/fixed-def/stack cached flags and `limit` from its current
    /// ranges' uses — the merge-time equivalents of what
    /// `merge_vreg_bundles` sets at bundle creation, needed after ranges
    /// leave a bundle outside `merge_bundles`.
    fn set_bundle_flags(&mut self, bundle: LiveBundleIndex) {
        let mut fixed = false;
        let mut fixed_def = false;
        let mut stack = false;
        'scan: for entry in &self.bundles[bundle].ranges {
            for u in &self.ranges[entry.index].uses {
                use OperandConstraint::*;
                match u.operand.constraint() {
                    FixedReg(_) => {
                        fixed = true;
                        if u.operand.kind() == OperandKind::Def {
                            fixed_def = true;
                        }
                    }
                    Stack => {
                        stack = true;
                    }
                    Any | Reg | Reuse(_) | Limit(_) => {}
                }
                if fixed && stack && fixed_def {
                    break 'scan;
                }
            }
        }
        self.bundles[bundle].set_cached_flags(fixed, fixed_def, stack);
        self.bundles[bundle].limit = self.compute_bundle_limit(bundle);
    }

    /// PLIRON (`PLIRON_RA2_EVICT`): `merge_bundles(from_b, to_b)` just
    /// failed on a live-range overlap (`last_merge_fail == 4`). When
    /// every overlapping to-bundle vreg is a *third party* — not the
    /// param `to_vreg` itself (the `bp-ovl-3rd` census shape), not a
    /// block parameter (evicting one cascades to all of its own
    /// in-edges), not reuse-tied (its bundle is what makes the tied
    /// operands coincide) — and this edge is at least
    /// `evict_ratio()` times hotter than the arg edges the conflicts'
    /// moves would land back on, split the conflicting vregs out of
    /// `to_b` into one fresh bundle with a fresh spillset and retry the
    /// merge. Returns true when the retry merged.
    ///
    /// The conflicting vregs are all mutually non-overlapping (they
    /// were co-resident in `to_b`), so they keep their shared
    /// allocation by sharing the new bundle: this undoes only their
    /// join into `to_b`, not their merges with each other.
    fn bp_merge_evict(
        &mut self,
        arg_i: usize,
        to_vreg: VRegIndex,
        from_b: LiveBundleIndex,
        to_b: LiveBundleIndex,
        arg_cost: &[f32],
        reuse_tied: &[bool],
    ) -> bool {
        let stats = crate::movestats_on();
        let incoming = {
            let o = self.blockparam_outs[arg_i];
            self.bp_edge_weight(o.from_block, o.to_block)
        };
        // The census-comparable weight: the block frequency where the
        // edge move would land, as `tally_move` weights it.
        let mw = if stats {
            let o = self.blockparam_outs[arg_i];
            let to_preds = self.func.block_preds(o.to_block).len()
                + usize::from(self.func.entry_block() == o.to_block);
            let blk = if to_preds > 1 { o.from_block } else { o.to_block };
            match self.func.block_hotness(blk) {
                Some(f) => (f as f64).min(512.0),
                None => {
                    8f64.powi(self.ctx.cfginfo.approx_loop_depth[blk.index()].min(3) as i32)
                }
            }
        } else {
            0.0
        };
        macro_rules! why {
            ($r:expr) => {
                if stats {
                    crate::evictwhy_add($r, mw as f32);
                }
            };
        }
        if stats {
            crate::evictwhy_add(0, mw as f32);
        }
        // Collect every to-bundle vreg whose ranges overlap a
        // from-bundle range: the same scan `merge_bundles` did
        // (including the fixed-def range-start adjustment), but
        // gathering all conflicts rather than stopping at the first.
        let adjust_range_start = |bundle_idx, range: CodeRange| {
            if self.bundles[bundle_idx].cached_fixed_def() {
                ProgPoint::before(range.from.inst())
            } else {
                range.from
            }
        };
        let cap = crate::evict_cap();
        let mut conf: crate::Vec<VRegIndex> = crate::Vec::new();
        {
            let ranges_from = &self.bundles[from_b].ranges[..];
            let ranges_to = &self.bundles[to_b].ranges[..];
            let (mut fi, mut ti, mut steps) = (0usize, 0usize, 0usize);
            while fi < ranges_from.len() && ti < ranges_to.len() {
                steps += 1;
                if steps > 400 {
                    why!(1);
                    return false;
                }
                let (rf, rt) = (ranges_from[fi], ranges_to[ti]);
                if adjust_range_start(from_b, rf.range) >= rt.range.to {
                    ti += 1;
                } else if adjust_range_start(to_b, rt.range) >= rf.range.to {
                    fi += 1;
                } else {
                    let v = self.ranges[rt.index].vreg;
                    if !conf.contains(&v) {
                        if conf.len() >= cap {
                            why!(2);
                            return false;
                        }
                        conf.push(v);
                    }
                    if rf.range.to <= rt.range.to {
                        fi += 1;
                    } else {
                        ti += 1;
                    }
                }
            }
        }
        if conf.is_empty() {
            // Overlap was reported but the scan found no conflicting
            // vreg: the fixed-def start adjustment differed. Count it
            // with the other scan anomaly.
            why!(1);
            return false;
        }
        if conf.contains(&to_vreg) {
            why!(3);
            return false;
        }
        // PLIRON (`PLIRON_RA2_EVICTDBG`): eviction candidate frequency
        // landscape — `span` = the hottest block the conflicting
        // vreg's `to_b` ranges cover (the EVICT_SPAN test's value),
        // `usehot` = its hottest use anywhere.
        let evictdbg = crate::evictdbg_on();
        if evictdbg {
            for &v in &conf {
                let (mut hot, mut hotb) = (0.0f32, usize::MAX);
                let (mut uhot, mut ub, mut nu) = (0.0f32, usize::MAX, 0usize);
                for r in &self.vregs[v].ranges {
                    if self.ranges[r.index].bundle == to_b {
                        let lo =
                            self.ctx.cfginfo.insn_block[r.range.from.inst().index()].index();
                        let hi =
                            self.ctx.cfginfo.insn_block[r.range.to.prev().inst().index()].index();
                        for b in lo..=hi {
                            let h = self.ctx.cfginfo.hot_bonus.get(b).copied().unwrap_or(1.0);
                            if h > hot {
                                hot = h;
                                hotb = b;
                            }
                        }
                    }
                    for u in &self.ranges[r.index].uses {
                        let b = self.ctx.cfginfo.insn_block[u.pos.inst().index()].index();
                        let h = self.ctx.cfginfo.hot_bonus.get(b).copied().unwrap_or(1.0);
                        if h > uhot {
                            uhot = h;
                            ub = b;
                        }
                        nu += 1;
                    }
                }
                std::eprintln!(
                    "ra2ev: cand v{}: acost {} incoming {} span b{} w{} uses {} hottest b{} w{}",
                    v.index(),
                    arg_cost[v.index()],
                    incoming,
                    hotb,
                    hot,
                    nu,
                    ub,
                    uhot,
                );
            }
        }
        // Cost of evicting a vreg: it sits in its own bundle again, so
        // every merged edge that carried it in gets its edge move back.
        // `ecost[v]` is the sum of the edge weights that touch `v` as
        // arg or param — an over-count of the restored moves (it
        // charges edges that never merged), hence conservative.
        let evict_params = crate::evict_mode() & 2 != 0;
        let span_lim = crate::evict_span();
        let mut cost = 0.0f32;
        for &v in &conf {
            if self.vregs[v].blockparam.is_valid() && !evict_params {
                why!(4);
                return false;
            }
            if reuse_tied[v.index()] {
                why!(5);
                return false;
            }
            let c = arg_cost[v.index()];
            if !(c > 0.0) {
                why!(6);
                return false;
            }
            // PLIRON (`PLIRON_RA2_EVICT_SPAN`): a squatter whose
            // `to_b` ranges reach a block more than EVICT_SPAN times
            // hotter than this edge may not be evicted: alone in the
            // fresh bundle it must hold a register through that block,
            // and one forced spill there costs more than the edge move
            // the merge removes (BrotliCreateHqZopfli: a 913-weight
            // edge evicted a squatter spanning a 1.6M-weight loop,
            // which went minimal and paid a reload per iteration).
            // Only `to_b` ranges count — the eviction moves those
            // alone; a vreg's ranges in other bundles are unaffected.
            // Not memoized: the to_b partition changes as merges and
            // evictions move ranges between bundles.
            if span_lim > 0.0 {
                let mut h = 0.0f32;
                for r in &self.vregs[v].ranges {
                    if self.ranges[r.index].bundle != to_b {
                        continue;
                    }
                    let lo =
                        self.ctx.cfginfo.insn_block[r.range.from.inst().index()].index();
                    let hi =
                        self.ctx.cfginfo.insn_block[r.range.to.prev().inst().index()].index();
                    for b in lo..=hi {
                        let hb = self.ctx.cfginfo.hot_bonus.get(b).copied().unwrap_or(1.0);
                        if hb > h {
                            h = hb;
                        }
                    }
                }
                if h > span_lim * incoming {
                    why!(10);
                    return false;
                }
            }
            cost += c;
        }
        if !(incoming >= crate::evict_ratio() * cost) {
            // PLIRON (EVICT bit 2): the per-vreg sum over-counts — it
            // charges every edge touching a conflicting vreg, merged or
            // not. Recompute exactly: only edges that were merged into
            // `to_b` and are split by this eviction restore a move
            // (arg in `conf` xor param in `conf`, both bundles `to_b`).
            if crate::evict_mode() & 4 == 0 {
                why!(7);
                return false;
            }
            let inset = |v: VRegIndex| conf.contains(&v);
            let mut xcost = 0.0f32;
            for o in 0..self.blockparam_outs.len() {
                let out = self.blockparam_outs[o];
                let fb = self.ranges[self.vregs[out.from_vreg].ranges[0].index].bundle;
                if fb != to_b {
                    continue;
                }
                let tb = self.ranges[self.vregs[out.to_vreg].ranges[0].index].bundle;
                if tb == to_b && inset(out.from_vreg) != inset(out.to_vreg) {
                    xcost += self.bp_edge_weight(out.from_block, out.to_block);
                }
            }
            cost = xcost;
            if !(incoming >= crate::evict_ratio() * cost) {
                why!(7);
                return false;
            }
        }
        // The overlap is provably gone once the conflicting vregs leave;
        // check the merged requirement on the post-eviction bundle too,
        // so a requirement refusal doesn't leave the evictions wasted.
        let skip: FxHashSet<VRegIndex> = conf.iter().copied().collect();
        let ok = match (
            self.compute_requirement(from_b),
            self.compute_requirement_skip(to_b, &skip),
        ) {
            (Ok(a), Ok(b)) => a.merge(b).is_ok(),
            _ => false,
        };
        if !ok {
            why!(8);
            return false;
        }
        why!(9);

        // Evict: move the conflicting vregs' ranges out of `to_b` into a
        // fresh bundle with a fresh spillset.
        let bump = self.ctx.bump();
        let nb = self.ctx.bundles.add(bump);
        let ranges: &crate::ion::data_structures::LiveRanges = &self.ctx.ranges;
        let mut moved: crate::Vec<LiveRangeListEntry> = crate::Vec::new();
        self.ctx.bundles[to_b].ranges.retain(|e| {
            if skip.contains(&ranges[e.index].vreg) {
                moved.push(*e);
                false
            } else {
                true
            }
        });
        debug_assert!(!moved.is_empty());
        for e in &moved {
            self.ctx.ranges[e.index].bundle = nb;
        }
        let mut extent = moved[0].range;
        for e in &moved[1..] {
            extent = extent.join(e.range);
        }
        self.ctx.bundles[nb].ranges.extend_from_slice(&moved);
        let class = self.spillsets[self.bundles[to_b].spillset].class;
        let ssidx = self.spillsets.push(SpillSet {
            slot: SpillSlotIndex::invalid(),
            required: false,
            class,
            hint: PReg::invalid(),
            spill_bundle: LiveBundleIndex::invalid(),
            splits: 0,
            cold_spill: false,
            region_count: 0,
            use_cluster_done: false,
            range: extent,
        });
        self.bundles[nb].spillset = ssidx;
        self.set_bundle_flags(nb);
        explain!(
            "  bp-evict: {:?} out of bundle{} into bundle{} (edge block{} -> v{})",
            conf,
            to_b.index(),
            nb.index(),
            self.blockparam_outs[arg_i].from_block.index(),
            to_vreg.index()
        );
        if evictdbg {
            std::eprintln!(
                "ra2ev: EVICT {:?} edge block{} -> block{} v{} incoming {}",
                conf,
                self.blockparam_outs[arg_i].from_block.index(),
                self.blockparam_outs[arg_i].to_block.index(),
                to_vreg.index(),
                incoming,
            );
        }

        // `to_b`'s cached flags, limit and spillset extent may have been
        // widened by the evicted vregs; recompute over what remains.
        self.set_bundle_flags(to_b);
        let mut ext: Option<CodeRange> = None;
        for e in &self.bundles[to_b].ranges {
            ext = Some(match ext {
                None => e.range,
                Some(x) => x.join(e.range),
            });
        }
        if let Some(ext) = ext {
            let ss = self.ctx.bundles[to_b].spillset;
            self.ctx.spillsets[ss].range = ext;
        }

        use core::sync::atomic::Ordering::Relaxed;
        crate::EVICTSTATS[1].fetch_add(conf.len() as u64, Relaxed);
        if self.merge_bundles(from_b, to_b) {
            crate::EVICTSTATS[0].fetch_add(1, Relaxed);
            true
        } else {
            crate::EVICTSTATS[2].fetch_add(1, Relaxed);
            false
        }
    }

    pub fn merge_vreg_bundles(&mut self) {
        // Create a bundle for every vreg, initially.
        trace!("merge_vreg_bundles: creating vreg bundles");
        for vreg in 0..self.vregs.len() {
            let vreg = VRegIndex::new(vreg);
            if self.vregs[vreg].ranges.is_empty() {
                continue;
            }

            let bundle = self.ctx.bundles.add(self.ctx.scratch_bump.clone());
            let mut range = self.vregs[vreg].ranges.first().unwrap().range;

            self.bundles[bundle].ranges = self.vregs[vreg].ranges.clone();
            trace!("vreg v{} gets bundle{}", vreg.index(), bundle.index());
            for entry in &self.ctx.bundles[bundle].ranges {
                trace!(
                    " -> with LR range{}: {:?}",
                    entry.index.index(),
                    entry.range
                );
                range = range.join(entry.range);
                self.ctx.ranges[entry.index].bundle = bundle;
            }

            let mut fixed = false;
            let mut fixed_def = false;
            let mut stack = false;
            let mut limit: Option<u8> = None;
            for entry in &self.bundles[bundle].ranges {
                for u in &self.ranges[entry.index].uses {
                    use OperandConstraint::*;
                    match u.operand.constraint() {
                        FixedReg(_) => {
                            fixed = true;
                            if u.operand.kind() == OperandKind::Def {
                                fixed_def = true;
                            }
                        }
                        Stack => stack = true,
                        Limit(current) => {
                            let current = u8::try_from(current)
                                .expect("the current limit is too large to fit in a u8");
                            match limit {
                                Some(prev) => limit = Some(prev.min(current)),
                                None => limit = Some(current),
                            }
                        }
                        Any | Reg | Reuse(_) => {
                            continue;
                        }
                    }
                    if fixed && stack && fixed_def {
                        break;
                    }
                }
            }
            if fixed {
                self.bundles[bundle].set_cached_fixed();
            }
            if fixed_def {
                self.bundles[bundle].set_cached_fixed_def();
            }
            if stack {
                self.bundles[bundle].set_cached_stack();
            }
            self.bundles[bundle].limit = limit;

            // Create a spillslot for this bundle.
            let reg = self.vreg(vreg);
            let ssidx = self.spillsets.push(SpillSet {
                slot: SpillSlotIndex::invalid(),
                required: false,
                class: reg.class(),
                hint: PReg::invalid(),
                spill_bundle: LiveBundleIndex::invalid(),
                splits: 0,
                cold_spill: false,
                region_count: 0,
                use_cluster_done: false,
                range,
            });
            self.bundles[bundle].spillset = ssidx;
        }

        // PLIRON (`PLIRON_RA2_EVICT`): vregs tied by a Reuse operand are
        // never evicted from a merge (their shared bundle is what makes
        // the tied operands coincide); mark both the reusing def and the
        // reused input.
        let evict = crate::evict_on();
        let mut reuse_tied: crate::Vec<bool> = crate::Vec::new();
        if evict {
            reuse_tied.resize(self.vregs.len(), false);
        }

        for inst in 0..self.func.num_insts() {
            let inst = Inst::new(inst);

            // Attempt to merge Reuse-constraint operand outputs with the
            // corresponding inputs.
            for op in self.func.inst_operands(inst) {
                if let OperandConstraint::Reuse(reuse_idx) = op.constraint() {
                    let src_vreg = op.vreg();
                    let dst_vreg = self.func.inst_operands(inst)[reuse_idx].vreg();
                    if evict {
                        reuse_tied[src_vreg.vreg()] = true;
                        reuse_tied[dst_vreg.vreg()] = true;
                    }

                    trace!(
                        "trying to merge reused-input def: src {} to dst {}",
                        src_vreg,
                        dst_vreg
                    );
                    let src_bundle = self.ranges[self.vregs[src_vreg].ranges[0].index].bundle;
                    debug_assert!(src_bundle.is_valid());
                    let dest_bundle = self.ranges[self.vregs[dst_vreg].ranges[0].index].bundle;
                    debug_assert!(dest_bundle.is_valid());
                    self.merge_bundles(/* from */ dest_bundle, /* to */ src_bundle);
                }
            }
        }

        // Attempt to merge blockparams with their inputs.
        // PLIRON: `PLIRON_RA2_MERGEDEPTH=1` (default off): merge the
        // blockparam edges of the deepest loops first, so a loop-carried
        // value shares the param's register and the move lands on the
        // preheader edge instead of the back edge.
        static MD: ::std::sync::OnceLock<u8> = ::std::sync::OnceLock::new();
        let mut order: crate::Vec<usize> = (0..self.blockparam_outs.len()).collect();
        let md = *MD.get_or_init(|| {
            ::std::env::var("PLIRON_RA2_MERGEDEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        });
        if md > 0 {
            let depth = &self.ctx.cfginfo.approx_loop_depth;
            let span = |v: VRegIndex| {
                let r = &self.vregs[v].ranges;
                match (r.first(), r.last()) {
                    (Some(a), Some(b)) => b.range.to.inst().index().saturating_sub(a.range.from.inst().index()),
                    _ => 0,
                }
            };
            // Modes 4 and 5: by the edge's block frequency (`hot_bonus`,
            // quantized to a factor of 2) instead of the loop depth; 4
            // breaks ties shortest range first.
            let hot = &self.ctx.cfginfo.hot_bonus;
            order.sort_by_key(|&i| {
                let o = &self.blockparam_outs[i];
                let d = ::core::cmp::Reverse(depth[o.from_block.index()]);
                match md {
                    1 => (d, 0i64),
                    2 => (d, span(o.from_vreg) as i64),
                    4 | 5 => {
                        let h = hot.get(o.from_block.index()).copied().unwrap_or(1.0).max(1.0);
                        let q = ::core::cmp::Reverse(h.log2() as u32);
                        (q, if md == 4 { span(o.from_vreg) as i64 } else { 0 })
                    }
                    6 => (::core::cmp::Reverse(0), span(o.from_vreg) as i64),
                    _ => (d, -(span(o.from_vreg) as i64)),
                }
            });
        }
        // PLIRON (`PLIRON_RA2_EVICT`): each vreg's total edge weight —
        // the moves that reappear on the edges touching it when it is
        // evicted from a merged bundle. An edge charges both ends: an
        // evicted arg pays its arg edges, an evicted param its in-edges
        // (mode 2); both in `conf` double-count, which is conservative.
        let mut arg_cost: crate::Vec<f32> = crate::Vec::new();
        if evict {
            arg_cost.resize(self.vregs.len(), 0.0f32);
            for o in 0..self.blockparam_outs.len() {
                let out = self.blockparam_outs[o];
                let h = self.bp_edge_weight(out.from_block, out.to_block);
                arg_cost[out.from_vreg.index()] += h;
                if crate::evict_mode() & 2 != 0 {
                    arg_cost[out.to_vreg.index()] += h;
                }
            }
        }

        for i in order {
            let BlockparamOut {
                from_vreg, to_vreg, to_block, ..
            } = self.blockparam_outs[i];
            if self.func.no_param_merge(to_block) {
                continue;
            }
            trace!(
                "trying to merge blockparam v{} with input v{}",
                to_vreg.index(),
                from_vreg.index()
            );
            let to_bundle = self.ranges[self.vregs[to_vreg].ranges[0].index].bundle;
            debug_assert!(to_bundle.is_valid());
            let from_bundle = self.ranges[self.vregs[from_vreg].ranges[0].index].bundle;
            debug_assert!(from_bundle.is_valid());
            trace!(
                " -> from bundle{} to bundle{}",
                from_bundle.index(),
                to_bundle.index()
            );
            // PLIRON: `PLIRON_RA2_NOPINMERGE=1` — don't let one side's
            // `FixedReg`/`FixedStack` constraint pin an unconstrained
            // neighbor whose ranges cross a clobber of that preg. The
            // merged bundle would require the preg everywhere it is
            // live and split+fill at every call clobber; refusing the
            // merge keeps the edge move, which is cheaper. (A pinned
            // try_call-ret vreg merging into a long param chain is the
            // motivating case — `BrotliCreateHqZopfli...`.)
            let stats = crate::movestats_on();
            if crate::nopinmerge_on() && self.pin_spread_hurts(from_bundle, to_bundle) {
                if stats {
                    let key = crate::ion::data_structures::u64_key(
                        self.blockparam_outs[i].from_block.raw_u32(),
                        to_vreg.raw_u32(),
                    );
                    self.ctx.bp_merge_kind.insert(key, 5);
                }
                continue;
            }
            explain!(
                "bpmerge v{} -> v{} (block{} -> block{})",
                from_vreg.index(),
                to_vreg.index(),
                self.blockparam_outs[i].from_block.index(),
                to_block.index()
            );
            if !self.merge_bundles(from_bundle, to_bundle) {
                // PLIRON (`PLIRON_RA2_EVICT`): on a pure third-party
                // overlap, evict the conflicting vregs out of the
                // to-bundle and retry.
                if evict
                    && self.ctx.last_merge_fail == 4
                    && self.bp_merge_evict(
                        i,
                        to_vreg,
                        from_bundle,
                        to_bundle,
                        &arg_cost,
                        &reuse_tied,
                    )
                {
                    continue;
                }
                if stats {
                let key = crate::ion::data_structures::u64_key(
                    self.blockparam_outs[i].from_block.raw_u32(),
                    to_vreg.raw_u32(),
                );
                let mut k = self.ctx.last_merge_fail;
                if k == 4 {
                    // PLIRON census: which to-bundle vreg's range the
                    // input's ranges overlap, and where the input is
                    // defined relative to the edge.
                    let fb = self.blockparam_outs[i].from_block;
                    let def_block = self.vregs[from_vreg]
                        .ranges
                        .first()
                        .map(|r| self.ctx.cfginfo.insn_block[r.range.from.inst().index()]);
                    let edge_pred = match self.func.block_preds(fb) {
                        &[p] if self.func.block_succs(fb).len() == 1 => Some(p),
                        _ => None,
                    };
                    let mut self_ov = false;
                    let mut third_ov = false;
                    {
                        let ranges_from = &self.bundles[from_bundle].ranges[..];
                        let ranges_to = &self.bundles[to_bundle].ranges[..];
                        let mut fi = 0;
                        let mut ti = 0;
                        let mut steps = 0;
                        while fi < ranges_from.len() && ti < ranges_to.len() && steps < 400 {
                            steps += 1;
                            if ranges_from[fi].range.from >= ranges_to[ti].range.to {
                                ti += 1;
                            } else if ranges_to[ti].range.from >= ranges_from[fi].range.to {
                                fi += 1;
                            } else {
                                if self.ranges[ranges_to[ti].index].vreg == to_vreg {
                                    self_ov = true;
                                } else {
                                    third_ov = true;
                                }
                                if self_ov && third_ov {
                                    break;
                                }
                                if ranges_from[fi].range.to <= ranges_to[ti].range.to {
                                    fi += 1;
                                } else {
                                    ti += 1;
                                }
                            }
                        }
                    }
                    let in_fb = def_block == Some(fb);
                    let in_pred = edge_pred.is_some() && def_block == edge_pred;
                    k = if third_ov && !self_ov {
                        11 // conflict only with another vreg already in the bundle
                    } else if third_ov {
                        12 // conflict with the param's own ranges and another's
                    } else if in_fb && self.func.block_succs(fb).len() >= 2 {
                        10 // input defined in the branching pred, on a direct edge
                    } else if in_fb {
                        14 // input defined in the jump-pred/stub itself
                    } else if in_pred && self.func.block_succs(edge_pred.unwrap()).len() >= 2 {
                        13 // input defined in the multi-succ pred of a stub: sinkable
                    } else if in_pred {
                        15 // input defined in a single-succ pred of a stub
                    } else {
                        4 // input defined further upstream
                    };
                    // PLIRON census: bit 0x20 — the refused arg is
                    // remat-able (`Function::is_remat`): the emitted edge
                    // move rematerializes it for the same instruction the
                    // def would have cost anyway, so its tally weight
                    // overstates the true cost of the refusal.
                    if self.func.is_remat(from_vreg.index()) {
                        k |= 32;
                    }
                }
                self.ctx.bp_merge_kind.insert(key, k);
                }
            }
        }

        trace!("done merging bundles");
    }

    pub fn compute_bundle_prio(&self, bundle: LiveBundleIndex) -> u32 {
        // The priority is simply the total "length" -- the number of
        // instructions covered by all LiveRanges.
        let mut total = 0;
        for entry in &self.bundles[bundle].ranges {
            total += entry.range.len() as u32;
        }
        trace!(" -> prio {total}");
        total
    }

    pub fn compute_bundle_limit(&self, bundle: LiveBundleIndex) -> Option<u8> {
        let mut limit: Option<u8> = None;
        for entry in &self.bundles[bundle].ranges {
            for u in &self.ranges[entry.index].uses {
                use OperandConstraint::*;
                match u.operand.constraint() {
                    Limit(current) => {
                        let current = u8::try_from(current)
                            .expect("the current limit is too large to fit in a u8");
                        match limit {
                            Some(prev) => limit = Some(prev.min(current)),
                            None => limit = Some(current),
                        }
                    }
                    FixedReg(_) | Stack => {
                        break;
                    }
                    Any | Reg | Reuse(_) => {
                        continue;
                    }
                }
            }
        }
        limit
    }

    pub fn queue_bundles(&mut self) {
        for bundle in 0..self.bundles.len() {
            trace!("enqueueing bundle{}", bundle);
            let bundle = LiveBundleIndex::new(bundle);
            if self.bundles[bundle].ranges.is_empty() {
                trace!(" -> no ranges; skipping");
                continue;
            }
            self.recompute_bundle_properties(bundle);
            let prio = self.bundles[bundle].prio as usize;
            self.allocation_queue.insert(bundle, prio, PReg::invalid());
        }
        self.output.stats.merged_bundle_count = self.allocation_queue.heap.len();
    }
}

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

//! Spillslot allocation.

use super::{
    AllocRegResult, Env, LiveRangeKey, PRegIndex, RegTraversalIter, SpillSetIndex, SpillSlotData,
    SpillSlotIndex,
};
use crate::{
    Allocation, Ctx, Edit, Function, FxHashMap, FxHashSet, InstPosition, RegClass, SpillSlot,
};
use alloc::vec;
use alloc::vec::Vec;
use smallvec::SmallVec;

impl<'a, F: Function> Env<'a, F> {
    pub fn try_allocating_regs_for_spilled_bundles(&mut self) {
        trace!("allocating regs for spilled bundles");
        let mut scratch = core::mem::take(&mut self.ctx.scratch_conflicts);
        for i in 0..self.ctx.spilled_bundles.len() {
            let bundle = self.ctx.spilled_bundles[i]; // don't borrow self

            if self.ctx.bundles[bundle].ranges.is_empty() {
                continue;
            }

            let class = self.ctx.spillsets[self.ctx.bundles[bundle].spillset].class;
            let hint = self.ctx.spillsets[self.ctx.bundles[bundle].spillset]
                .hint
                .as_valid();

            // This may be an empty-range bundle whose ranges are not
            // sorted; sort all range-lists again here.
            self.ctx.bundles[bundle]
                .ranges
                .sort_unstable_by_key(|entry| entry.range.from);

            let mut success = false;
            self.ctx.output.stats.spill_bundle_reg_probes += 1;
            let limit = self.bundles[bundle].limit.map(|l| l as usize);
            for preg in RegTraversalIter::new(self.env, class, None, hint, bundle.index(), limit) {
                trace!("trying bundle {:?} to preg {:?}", bundle, preg);
                // PLIRON (`PLIRON_RA2_COLDSPLIT`): ranges a cold split put
                // on the stack to keep a callee-saved register closed do
                // not open one here either.
                if self.ctx.spillsets[self.ctx.bundles[bundle].spillset].cold_spill
                    && !self.env.preferred_regs_by_class[class as u8 as usize].contains(preg)
                    && self.ctx.pregs[preg.index()].allocations.btree.is_empty()
                {
                    continue;
                }
                let preg_idx = PRegIndex::new(preg.index());
                if let AllocRegResult::Allocated(_) =
                    self.try_to_allocate_bundle_to_reg(bundle, preg_idx, None, &mut scratch)
                {
                    self.ctx.output.stats.spill_bundle_reg_success += 1;
                    success = true;
                    break;
                }
            }

            if !success {
                trace!(
                    "spilling bundle {:?}: marking spillset {:?} as required",
                    bundle,
                    self.ctx.bundles[bundle].spillset
                );
                self.ctx.spillsets[self.ctx.bundles[bundle].spillset].required = true;
            }
        }
        self.ctx.scratch_conflicts = scratch;
    }

    pub fn spillslot_can_fit_spillset(
        &mut self,
        spillslot: SpillSlotIndex,
        spillset: SpillSetIndex,
    ) -> bool {
        !self.ctx.spillslots[spillslot.index()]
            .ranges
            .btree
            .contains_key(&LiveRangeKey::from_range(
                &self.ctx.spillsets[spillset].range,
            ))
    }

    pub fn allocate_spillset_to_spillslot(
        &mut self,
        spillset: SpillSetIndex,
        spillslot: SpillSlotIndex,
    ) {
        self.ctx.spillsets[spillset].slot = spillslot;

        let res = self.ctx.spillslots[spillslot.index()].ranges.btree.insert(
            LiveRangeKey::from_range(&self.ctx.spillsets[spillset].range),
            spillset,
        );

        debug_assert!(res.is_none());
    }

    pub fn allocate_spillslots(&mut self) {
        const MAX_ATTEMPTS: usize = 10;

        for spillset in 0..self.ctx.spillsets.len() {
            trace!("allocate spillslot: {}", spillset);
            let spillset = SpillSetIndex::new(spillset);
            if !self.ctx.spillsets[spillset].required {
                continue;
            }
            let class = self.ctx.spillsets[spillset].class as usize;
            // Try a few existing spillslots.
            let mut i = self.ctx.slots_by_class[class].probe_start;
            let mut success = false;
            // Never probe the same element more than once: limit the
            // attempt count to the number of slots in existence.
            for _attempt in
                0..core::cmp::min(self.ctx.slots_by_class[class].slots.len(), MAX_ATTEMPTS)
            {
                // Note: this indexing of `slots` is always valid
                // because either the `slots` list is empty and the
                // iteration limit above consequently means we don't
                // run this loop at all, or else `probe_start` is
                // in-bounds (because it is made so below when we add
                // a slot, and it always takes on the last index `i`
                // after this loop).
                let spillslot = self.ctx.slots_by_class[class].slots[i];

                if self.spillslot_can_fit_spillset(spillslot, spillset) {
                    self.allocate_spillset_to_spillslot(spillset, spillslot);
                    success = true;
                    self.ctx.slots_by_class[class].probe_start = i;
                    break;
                }

                i = self.ctx.slots_by_class[class].next_index(i);
            }

            if !success {
                // Allocate a new spillslot.
                let spillslot = SpillSlotIndex::new(self.ctx.spillslots.len());
                self.ctx.spillslots.push(SpillSlotData {
                    ranges: self.ctx.scratch_spillset_pool.pop().unwrap_or_default(),
                    alloc: Allocation::none(),
                    slots: self.func.spillslot_size(self.ctx.spillsets[spillset].class) as u32,
                });
                self.ctx.slots_by_class[class].slots.push(spillslot);
                self.ctx.slots_by_class[class].probe_start =
                    self.ctx.slots_by_class[class].slots.len() - 1;

                self.allocate_spillset_to_spillslot(spillset, spillslot);
            }
        }

        // Assign actual slot indices to spillslots.
        for i in 0..self.ctx.spillslots.len() {
            self.ctx.spillslots[i].alloc = self.allocate_spillslot(self.ctx.spillslots[i].slots);
        }

        trace!("spillslot allocator done");
    }

    pub fn allocate_spillslot(&mut self, size: u32) -> Allocation {
        let mut offset = self.ctx.output.num_spillslots as u32;
        // Align up to `size`.
        debug_assert!(size.is_power_of_two());
        offset = (offset + size - 1) & !(size - 1);
        let slot = if self.func.multi_spillslot_named_by_last_slot() {
            offset + size - 1
        } else {
            offset
        };
        offset += size;
        self.ctx.output.num_spillslots = offset as _;
        Allocation::stack(SpillSlot::new(slot as usize))
    }
}

/// PLIRON (`PLIRON_RA2_SLOTPAIR=1`, opt-in): repack spill-slot word
/// indices so that same-direction spill/reload edits emitted at the
/// same program-point gap get adjacent slots, which lets ISA pair
/// fusion (`ldp`/`stp` on AArch64) fire in vcode emission. Slot word
/// indices otherwise follow allocation order, which has no relation to
/// emission order — almost no consecutive memory edits share adjacent
/// slots, so pair fusion never fires on spill traffic (LLVM emits ~100
/// `ldp`/`stp` in the motivating HqZopfli function; we emitted none).
///
/// The remap is a pure permutation of the allocated slot-word space
/// [0, num_spillslots): every `Allocation::stack` in the output is
/// rewritten consistently, so semantics are unchanged. Pairing edges
/// are weighted by `cfginfo.hot_bonus` of the edit's block so hot-loop
/// pairs win slots first.
pub fn slot_pair_layout<F: Function>(func: &F, ctx: &mut Ctx) {
    if !crate::slotpair_on()
        || func.multi_spillslot_named_by_last_slot()
        || ctx.output.edits.is_empty()
    {
        return;
    }

    // word index -> (word size, class) for every allocated spillslot.
    let mut info: FxHashMap<u32, (u32, u8)> = FxHashMap::default();
    for class in 0..3 {
        for &ssi in &ctx.slots_by_class[class].slots {
            let sd = &ctx.spillslots[ssi.index()];
            if let Some(sl) = sd.alloc.as_stack() {
                info.insert(sl.index() as u32, (sd.slots, class as u8));
            }
        }
        for &a in &ctx.extra_spillslots_by_class[class] {
            if let Some(sl) = a.as_stack() {
                let regclass = [RegClass::Int, RegClass::Float, RegClass::Vector][class];
                let size = func.spillslot_size(regclass) as u32;
                info.insert(sl.index() as u32, (size, class as u8));
            }
        }
    }
    if info.is_empty() {
        return;
    }

    // Refuse to remap if any stack allocation in the output refers to a
    // slot we do not know the extent of — a repacked slot could collide
    // with its original position.
    let all_known = |a: &Allocation| {
        !a.is_stack() || info.contains_key(&(a.as_stack().unwrap().index() as u32))
    };
    for (_, edit) in &ctx.output.edits {
        let Edit::Move { from, to } = edit;
        if !all_known(from) || !all_known(to) {
            return;
        }
    }
    if !ctx.output.allocs.iter().all(all_known)
        || !ctx
            .output
            .debug_locations
            .iter()
            .all(|(_, _, _, a)| all_known(a))
    {
        return;
    }

    // Two edits are emitted adjacently iff they sit at the same
    // program-point gap: inst index plus the After bit.
    let gap_of = |p: &crate::ProgPoint| {
        p.inst().index() + (p.pos() == InstPosition::After) as usize
    };

    // Collect weighted pairing edges between slot words.
    let mut edges: FxHashMap<(u32, u32), f32> = FxHashMap::default();
    for w in ctx.output.edits.windows(2) {
        let (p1, e1) = &w[0];
        let (p2, e2) = &w[1];
        if gap_of(p1) != gap_of(p2) {
            continue;
        }
        let (Edit::Move { from: f1, to: t1 }, Edit::Move { from: f2, to: t2 }) = (e1, e2);
        // Same direction only: both spills (to-stack) or both fills
        // (from-stack). A spill+fill pair cannot fuse anyway.
        let (a1, a2) = if t1.is_stack() && t2.is_stack() {
            (*t1, *t2)
        } else if f1.is_stack() && f2.is_stack() {
            (*f1, *f2)
        } else {
            continue;
        };
        let (s1, s2) = (
            a1.as_stack().unwrap().index() as u32,
            a2.as_stack().unwrap().index() as u32,
        );
        if s1 == s2 {
            continue;
        }
        let (Some(&(z1, c1)), Some(&(z2, c2))) = (info.get(&s1), info.get(&s2)) else {
            continue;
        };
        if z1 != z2 || c1 != c2 {
            continue;
        }
        let key = (s1.min(s2), s1.max(s2));
        let block = ctx.cfginfo.insn_block[p1.inst().index()].index();
        *edges.entry(key).or_default() += ctx.cfginfo.hot_bonus[block];
    }
    if edges.is_empty() {
        return;
    }

    // Greedily link slots into chains: heaviest edges first, each slot
    // gets at most two neighbors, no cycles (union-find).
    let mut order: Vec<(u32, u32, f32)> =
        edges.iter().map(|(&(a, b), &w)| (a, b, w)).collect();
    order.sort_by(|x, y| {
        y.2.partial_cmp(&x.2)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(x.0.cmp(&y.0))
            .then(x.1.cmp(&y.1))
    });
    let mut nbr: FxHashMap<u32, SmallVec<[u32; 2]>> = FxHashMap::default();
    let mut parent: FxHashMap<u32, u32> = FxHashMap::default();
    fn find(parent: &mut FxHashMap<u32, u32>, mut x: u32) -> u32 {
        loop {
            let p = *parent.get(&x).unwrap_or(&x);
            if p == x {
                return x;
            }
            let gp = *parent.get(&p).unwrap_or(&p);
            if gp != p {
                parent.insert(x, gp);
            }
            x = p;
        }
    }
    for (a, b, _) in order {
        if nbr.get(&a).map_or(0, |v| v.len()) >= 2 || nbr.get(&b).map_or(0, |v| v.len()) >= 2 {
            continue;
        }
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra == rb {
            continue;
        }
        parent.insert(ra, rb);
        nbr.entry(a).or_default().push(b);
        nbr.entry(b).or_default().push(a);
    }

    // Extract chains. The union-find above prevented cycles, so every
    // component is a path; walk each from one end. Deterministic order:
    // sorted starts, then follow the single unvisited neighbor.
    let mut chains: Vec<Vec<u32>> = Vec::new();
    let mut seen: FxHashSet<u32> = FxHashSet::default();
    let mut starts: Vec<u32> = nbr.keys().copied().collect();
    starts.sort_unstable();
    for start in starts {
        if seen.contains(&start) {
            continue;
        }
        // Walk toward one end of the path containing `start`.
        let mut end = start;
        let mut prev = u32::MAX;
        while let Some(n) = nbr[&end].iter().copied().find(|&n| n != prev) {
            prev = end;
            end = n;
        }
        // Now walk from `end` to the other end.
        let mut chain = Vec::new();
        let mut prev = u32::MAX;
        let mut cur = end;
        loop {
            chain.push(cur);
            seen.insert(cur);
            match nbr[&cur].iter().copied().find(|&n| n != prev && !seen.contains(&n)) {
                Some(n) => {
                    prev = cur;
                    cur = n;
                }
                None => break,
            }
        }
        chains.push(chain);
    }

    // Repack [0, num_spillslots): chains first (heaviest total weight
    // first), then leftover slots. Chains take the HIGHEST aligned
    // bases — the top of the spillslot area sits just below the clobber
    // area, within `ldp`/`stp`'s simm7 reach of fp even when deep from
    // sp; unpaired slots fill bottom-up so they don't consume that
    // window.
    let total = ctx.output.num_spillslots;
    let mut used = vec![false; total];
    let mut new_of: FxHashMap<u32, u32> = FxHashMap::default();
    let first_fit = |used: &[bool], need: usize, align: usize| -> Option<usize> {
        let mut i = 0;
        while i + need <= used.len() {
            if i % align == 0 && used[i..i + need].iter().all(|&u| !u) {
                return Some(i);
            }
            i += 1;
        }
        None
    };
    let first_fit_top = |used: &[bool], need: usize, align: usize| -> Option<usize> {
        if need > used.len() {
            return None;
        }
        let mut i = used.len() - need;
        loop {
            if i % align == 0 && used[i..i + need].iter().all(|&u| !u) {
                return Some(i);
            }
            if i == 0 {
                return None;
            }
            i -= 1;
        }
    };
    chains.sort_by(|a, b| {
        let wa: f32 = a
            .windows(2)
            .map(|w| edges[&(w[0].min(w[1]), w[0].max(w[1]))])
            .sum();
        let wb: f32 = b
            .windows(2)
            .map(|w| edges[&(w[0].min(w[1]), w[0].max(w[1]))])
            .sum();
        wb.partial_cmp(&wa)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(a[0].cmp(&b[0]))
    });
    for chain in &chains {
        let size = info[&chain[0]].0 as usize;
        let need = chain.len() * size;
        let Some(base) = first_fit_top(&used, need, size) else {
            return;
        };
        for (k, &s) in chain.iter().enumerate() {
            new_of.insert(s, (base + k * size) as u32);
        }
        for u in &mut used[base..base + need] {
            *u = true;
        }
    }
    // Leftover slots: larger sizes first for determinism and to keep
    // aligned space for multi-word slots.
    let mut singles: Vec<u32> = info
        .keys()
        .copied()
        .filter(|s| !new_of.contains_key(s))
        .collect();
    singles.sort_by(|a, b| info[b].0.cmp(&info[a].0).then(a.cmp(b)));
    for s in singles {
        let size = info[&s].0 as usize;
        let Some(base) = first_fit(&used, size, size) else {
            return;
        };
        new_of.insert(s, base as u32);
        for u in &mut used[base..base + size] {
            *u = true;
        }
    }

    // Apply the permutation everywhere a stack allocation appears.
    let remap = |a: &mut Allocation| {
        if let Some(sl) = a.as_stack() {
            if let Some(&ni) = new_of.get(&(sl.index() as u32)) {
                *a = Allocation::stack(SpillSlot::new(ni as usize));
            }
        }
    };
    for (_, edit) in ctx.output.edits.iter_mut() {
        let Edit::Move { from, to } = edit;
        remap(from);
        remap(to);
    }
    for a in ctx.output.allocs.iter_mut() {
        remap(a);
    }
    for (_, _, _, a) in ctx.output.debug_locations.iter_mut() {
        remap(a);
    }
    for sd in ctx.spillslots.iter_mut() {
        remap(&mut sd.alloc);
    }
    for class in 0..3 {
        for a in ctx.extra_spillslots_by_class[class].iter_mut() {
            remap(a);
        }
    }
}

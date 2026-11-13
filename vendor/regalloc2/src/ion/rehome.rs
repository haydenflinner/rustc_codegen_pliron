//! PLIRON (`PLIRON_RA2_REHOME`, see `crate::rehome_mode`): choose where a
//! value lives in a block that only joins paths.
//!
//! A bundle that must split is cut at program points, and each piece is a
//! contiguous stretch of the linear block order. A block that many paths
//! jump to sits after all of them in that order, so it lands in whatever
//! piece covers the code just before it, usually the piece that crosses a
//! call and holds a callee-saved register. For a loop's single latch
//! (`looprot::merge_latches`) that is the wrong side: the loop header, the
//! latch's successor on nearly every trip, is in the first piece. The value
//! is then moved into the latch's register on the hot path into the latch
//! and moved back on the back edge (pulldown-cmark's `scanners::unescape`:
//! `mov x19, x2; mov x22, x3; cmp; b.hs; mov x2, x19; mov x3, x22; b`).
//!
//! After allocation, for every such region (a join block plus the
//! jump-only edge blocks behind it) and every value live through it, the
//! moves on the edges into and out of the region are priced with the block
//! frequencies for the register the value has now and for each register
//! it has on the other side of one of those edges. If another register is
//! cheaper and free over the region, the value's live range is cut at the
//! region's block boundaries and the middle piece is given that register.
//! Nothing else changes: the moves come from the ordinary edge-move
//! insertion (`moves.rs`), which sees one more range with its own
//! allocation, exactly as after any split.
//!
//! A value is left alone when it has a use in the region that needs a
//! particular place (anything but a plain register or "any" use), when its
//! live ranges overlap (a spilled value with per-use register pieces), or
//! when its bundle has a register limit.

use super::{Env, LiveRangeIndex, LiveRangeKey, LiveRangeListEntry, UseList, VRegIndex};
use crate::ion::data_structures::CodeRange;
use crate::{
    Allocation, Block, Function, FxHashMap, OperandConstraint, OperandKind, PReg, ProgPoint,
};
use alloc::vec::Vec;
use smallvec::SmallVec;

struct Region {
    /// The join block first, then its jump-only successors.
    blocks: SmallVec<[Block; 4]>,
    /// Edges leaving the region: (source in the region, target, weight).
    outs: SmallVec<[(Block, Block, f32); 4]>,
}

impl<'a, F: Function> Env<'a, F> {
    fn rehome_regions(&self, mode: u8) -> Vec<Region> {
        let hot = &self.ctx.cfginfo.hot_bonus;
        let mut regions = Vec::new();
        for b in 0..self.func.num_blocks() {
            let block = Block::new(b);
            let preds = self.func.block_preds(block);
            if preds.len() < 2 || preds.contains(&block) {
                continue;
            }
            let mut blocks: SmallVec<[Block; 4]> = SmallVec::new();
            blocks.push(block);
            let mut outs: SmallVec<[(Block, Block, f32); 4]> = SmallVec::new();
            let mut ok = true;
            for &s in self.func.block_succs(block) {
                let edge_block = s != block
                    && self.func.block_preds(s).len() == 1
                    && self.func.block_succs(s).len() == 1
                    && self.func.block_insns(s).len() == 1
                    && self.func.block_params(s).is_empty();
                if edge_block {
                    let t = self.func.block_succs(s)[0];
                    if t == block || t == s || blocks.contains(&s) {
                        ok = false;
                        break;
                    }
                    blocks.push(s);
                    outs.push((s, t, hot[s.index()].min(hot[t.index()])));
                } else {
                    outs.push((block, s, hot[b].min(hot[s.index()])));
                }
            }
            if !ok || outs.is_empty() {
                continue;
            }
            // Mode 1: only a block that jumps backwards (a latch).
            if mode == 1 && !outs.iter().any(|&(_, t, _)| t.index() <= b) {
                continue;
            }
            regions.push(Region { blocks, outs });
        }
        regions
    }

    /// The live range of `vreg` that contains `p` (range lists sorted and
    /// free of overlaps).
    fn rehome_lr_at(&self, vreg: VRegIndex, p: ProgPoint) -> Option<LiveRangeListEntry> {
        let rs = &self.ctx.vregs[vreg].ranges;
        let i = rs.partition_point(|e| e.range.from <= p);
        if i == 0 {
            return None;
        }
        let e = rs[i - 1];
        (e.range.to > p).then_some(e)
    }

    /// Cut live range `orig` of `vreg` at the block entry `at` (strictly inside it).
    /// `orig` keeps `[from, at)`; the new range gets `[at, to)`, the uses
    /// from `at` on, the same bundle, and its place in the vreg's and the
    /// bundle's sorted range lists.
    fn rehome_split(
        &mut self,
        vreg: VRegIndex,
        orig: LiveRangeIndex,
        at: ProgPoint,
    ) -> LiveRangeIndex {
        let old = self.ctx.ranges[orig].range;
        debug_assert!(old.from < at && at < old.to);
        // (A range's own `vreg` field is not always set: the lists are
        // the record of which value a range belongs to.)
        let bundle = self.ctx.ranges[orig].bundle;
        let new_lr = self
            .ctx
            .ranges
            .add(CodeRange { from: at, to: old.to }, self.ctx.scratch_bump.clone());
        self.ctx.ranges[new_lr].vreg = self.ctx.ranges[orig].vreg;
        self.ctx.ranges[new_lr].bundle = bundle;
        let first_use = self.ctx.ranges[orig]
            .uses
            .iter()
            .position(|u| u.pos >= at)
            .unwrap_or(self.ctx.ranges[orig].uses.len());
        let mut rest_uses = UseList::new_in(self.ctx.bump());
        rest_uses.extend(self.ctx.ranges[orig].uses.iter().cloned().skip(first_use));
        self.ctx.ranges[new_lr].uses = rest_uses;
        self.ctx.ranges[orig].uses.truncate(first_use);
        self.ctx.ranges[orig].range.to = at;
        self.recompute_range_properties(orig);
        self.recompute_range_properties(new_lr);
        let entry = LiveRangeListEntry {
            range: CodeRange { from: at, to: old.to },
            index: new_lr,
        };
        let rs = &mut self.ctx.vregs[vreg].ranges;
        if let Some(i) = rs.iter().position(|e| e.index == orig) {
            rs[i].range.to = at;
            rs.insert(i + 1, entry);
        } else {
            debug_assert!(false, "rehome: range not in its vreg's list");
            rs.push(entry);
        }
        let rs = &mut self.ctx.bundles[bundle].ranges;
        if let Some(i) = rs.iter().position(|e| e.index == orig) {
            rs[i].range.to = at;
            rs.insert(i + 1, entry);
        } else {
            rs.push(entry);
        }
        new_lr
    }

    pub(crate) fn rehome_joins(&mut self) {
        let mode = crate::rehome_mode();
        if mode == 0 {
            return;
        }
        let regions = self.rehome_regions(mode);
        if regions.is_empty() {
            return;
        }

        // Sorted, current range lists (as `apply_allocations_and_insert_moves`
        // does next), and the vregs whose ranges overlap.
        let mut overlap: Vec<bool> = Vec::with_capacity(self.ctx.vregs.len());
        for vreg in &mut self.ctx.vregs {
            for entry in &mut vreg.ranges {
                entry.range = self.ctx.ranges[entry.index].range;
            }
            vreg.ranges.sort_unstable_by_key(|entry| entry.range.from);
            let mut end = ProgPoint::from_index(0);
            let mut ov = false;
            for entry in &vreg.ranges {
                if entry.range.from < end {
                    ov = true;
                    break;
                }
                end = entry.range.to;
            }
            overlap.push(ov);
        }

        // (block parameter, predecessor) -> the argument passed.
        let mut args: FxHashMap<(u32, u32), VRegIndex> = FxHashMap::default();
        for o in &self.ctx.blockparam_outs {
            args.insert((o.to_vreg.raw_u32(), o.from_block.raw_u32()), o.from_vreg);
        }

        let mut changed = 0usize;
        let mut vals: Vec<(VRegIndex, bool)> = Vec::new();
        let mut ins: SmallVec<[(Allocation, f32); 8]> = SmallVec::new();
        let mut outs: SmallVec<[(Allocation, f32); 8]> = SmallVec::new();
        let mut pieces: SmallVec<[(Block, LiveRangeListEntry); 4]> = SmallVec::new();
        for region in &regions {
            let block = region.blocks[0];
            vals.clear();
            for p in self.func.block_params(block) {
                vals.push((VRegIndex::new(p.vreg()), true));
            }
            for v in self.ctx.liveins[block.index()].iter() {
                vals.push((VRegIndex::new(v), false));
            }
            'val: for &(v, is_param) in &vals {
                if overlap[v.index()] {
                    continue;
                }
                // The value's range in each region block it is live in.
                pieces.clear();
                let mut cur = Allocation::none();
                for (i, &blk) in region.blocks.iter().enumerate() {
                    if i > 0 && !self.ctx.liveins[blk.index()].get(v.index()) {
                        continue;
                    }
                    let entry = self.ctx.cfginfo.block_entry[blk.index()];
                    let end = self.ctx.cfginfo.block_exit[blk.index()].next();
                    let Some(e) = self.rehome_lr_at(v, entry) else {
                        continue 'val;
                    };
                    if e.range.to < end {
                        continue 'val;
                    }
                    let lr = &self.ctx.ranges[e.index];
                    if self.ctx.bundles[lr.bundle].limit.is_some() {
                        continue 'val;
                    }
                    for u in &lr.uses {
                        if u.pos < entry {
                            continue;
                        }
                        if u.pos >= end {
                            break;
                        }
                        let plain = u.operand.kind() == OperandKind::Use
                            && matches!(
                                u.operand.constraint(),
                                OperandConstraint::Any | OperandConstraint::Reg
                            );
                        if !plain {
                            continue 'val;
                        }
                    }
                    let a = self.get_alloc_for_range(e.index);
                    if i == 0 {
                        cur = a;
                    } else if a != cur {
                        continue 'val;
                    }
                    pieces.push((blk, e));
                }

                // Where the value is on the far side of every edge.
                ins.clear();
                outs.clear();
                let hot = &self.ctx.cfginfo.hot_bonus;
                for &p in self.func.block_preds(block) {
                    let src = if is_param {
                        match args.get(&(v.raw_u32(), p.raw_u32())) {
                            Some(&s) => s,
                            None => continue 'val,
                        }
                    } else {
                        v
                    };
                    if overlap[src.index()] {
                        continue 'val;
                    }
                    let Some(e) = self.rehome_lr_at(src, self.ctx.cfginfo.block_exit[p.index()])
                    else {
                        continue 'val;
                    };
                    let w = hot[p.index()].min(hot[block.index()]);
                    ins.push((self.get_alloc_for_range(e.index), w));
                }
                for &(s, t, w) in &region.outs {
                    if !pieces.iter().any(|&(blk, _)| blk == s) {
                        continue;
                    }
                    let t_entry = self.ctx.cfginfo.block_entry[t.index()];
                    if self.ctx.liveins[t.index()].get(v.index()) {
                        let Some(e) = self.rehome_lr_at(v, t_entry) else {
                            continue 'val;
                        };
                        outs.push((self.get_alloc_for_range(e.index), w));
                    }
                    // Parameters of `t` this value is passed to from `s`.
                    let outs_list = &self.ctx.blockparam_outs;
                    let lo = outs_list
                        .partition_point(|o| (o.from_vreg, o.from_block) < (v, s));
                    for o in &outs_list[lo..] {
                        if (o.from_vreg, o.from_block) != (v, s) {
                            break;
                        }
                        if o.to_block != t || overlap[o.to_vreg.index()] {
                            continue;
                        }
                        let Some(e) = self.rehome_lr_at(o.to_vreg, t_entry) else {
                            continue 'val;
                        };
                        outs.push((self.get_alloc_for_range(e.index), w));
                    }
                }
                let cost = |a: Allocation| -> f32 {
                    let mut c = 0.0f32;
                    for &(x, w) in ins.iter().chain(outs.iter()) {
                        if x != a {
                            c += w;
                        }
                    }
                    c
                };
                let cur_cost = cost(cur);
                if !(cur_cost > 0.0) {
                    continue;
                }
                let mut best: Option<(PReg, f32)> = None;
                for k in 0..ins.len() + outs.len() {
                    let a = if k < ins.len() { ins[k].0 } else { outs[k - ins.len()].0 };
                    let Some(preg) = a.as_reg() else { continue };
                    if a == cur || self.ctx.pregs[preg.index()].is_stack {
                        continue;
                    }
                    if best.is_some_and(|(b, _)| b == preg) {
                        continue;
                    }
                    // A gain below 1/16 of what the edges cost now is not
                    // worth a new range (and equal costs differ by rounding).
                    let c = cost(a);
                    if !(c <= cur_cost * (15.0 / 16.0)) || best.is_some_and(|(_, bc)| bc <= c) {
                        continue;
                    }
                    let free = pieces.iter().all(|&(blk, _)| {
                        let key = LiveRangeKey::from_range(&CodeRange {
                            from: self.ctx.cfginfo.block_entry[blk.index()],
                            to: self.ctx.cfginfo.block_exit[blk.index()].next(),
                        });
                        !self.ctx.pregs[preg.index()].allocations.btree.contains_key(&key)
                    });
                    if free {
                        best = Some((preg, c));
                    }
                }
                let Some((preg, best_cost)) = best else { continue };
                explain!(
                    "rehome block{} v{}: {} -> {} (edge moves {} -> {}), {} block(s)",
                    block.index(),
                    v.index(),
                    cur,
                    Allocation::reg(preg),
                    cur_cost,
                    best_cost,
                    pieces.len()
                );
                changed += 1;

                for k in 0..pieces.len() {
                    let (blk, e) = pieces[k];
                    let from = self.ctx.cfginfo.block_entry[blk.index()];
                    let to = self.ctx.cfginfo.block_exit[blk.index()].next();
                    // An earlier piece's cut may have shortened this range:
                    // look it up again.
                    let Some(e) = self.rehome_lr_at(v, from).or(Some(e)) else { continue };
                    let mut mid = e.index;
                    if self.ctx.ranges[mid].range.from < from {
                        mid = self.rehome_split(v, mid, from);
                    }
                    if self.ctx.ranges[mid].range.to > to {
                        self.rehome_split(v, mid, to);
                    }
                    let old_bundle = self.ctx.ranges[mid].bundle;
                    let range = self.ctx.ranges[mid].range;
                    let piece = self.ctx.bundles.add(self.ctx.scratch_bump.clone());
                    self.ctx.bundles[piece].spillset = self.ctx.bundles[old_bundle].spillset;
                    self.ctx.bundles[piece].allocation = Allocation::reg(preg);
                    self.ctx.bundles[old_bundle].ranges.retain(|x| x.index != mid);
                    self.ctx.bundles[piece]
                        .ranges
                        .push(LiveRangeListEntry { range, index: mid });
                    self.ctx.ranges[mid].bundle = piece;
                    let res = self.ctx.pregs[preg.index()]
                        .allocations
                        .btree
                        .insert(LiveRangeKey::from_range(&range), mid);
                    debug_assert!(res.is_none());
                }
            }
        }
        if changed > 0 {
            explain!("rehome: {} value(s) moved in {} region(s)", changed, regions.len());
        }
    }
}

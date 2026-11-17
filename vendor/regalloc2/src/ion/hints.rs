//! PLIRON (`PLIRON_RA2_COPYHINT`, see `crate::copyhint_mode`): copy hints.
//!
//! regalloc2's only register preference is the spillset's hint: the
//! register the last allocated piece of the same spillset got. Two bundles
//! joined by a move that merging could not remove (a block parameter and
//! its input whose live ranges overlap somewhere, a blockparam whose merge
//! a fixed requirement refused, the two sides of a split's boundary that
//! ended in different spillsets) know nothing of each other, so the second
//! one to be allocated takes the first free register in scan order and the
//! move stays. LLVM's greedy allocator orders its candidates by copy hints
//! (`getRegAllocationHints`): the registers of the values a virtual
//! register is copied to or from, weighted by the copies' frequency.
//!
//! `copy_hint` does the same for a bundle about to be allocated: for each
//! of its ranges it looks at the moves the edge-move insertion would emit
//! at the range's ends (the same vreg's abutting ranges, block-parameter
//! edges in both directions), and for each neighbour already in a register
//! adds the edge's block frequency (`CFGInfo::hot_bonus`) to that
//! register. The heaviest register becomes the hint, which `process_bundle`
//! tries first; nothing else changes (a hint is taken only when the whole
//! bundle fits it). Budgets: 64 ranges per bundle, 64 ranges per neighbour
//! vreg, 16 blockparam edges per vreg.

use super::{Env, LiveRangeIndex, VRegIndex};
use crate::ion::data_structures::LiveBundleIndex;
use crate::{Block, Function, PReg, ProgPoint};
use alloc::vec::Vec;
use smallvec::SmallVec;

const MAX_RANGES: usize = 64;
const MAX_EDGES: usize = 16;

impl<'a, F: Function> Env<'a, F> {
    /// Index `(to_vreg, from_block, from_vreg)` of the blockparam edges,
    /// sorted, for the parameter side of `copy_hint`. Built once per
    /// function when the knob is on.
    pub(crate) fn build_copy_hint_index(&mut self) {
        let mut v: Vec<(u32, u32, u32)> = self
            .blockparam_outs
            .iter()
            .map(|o| (o.to_vreg.raw_u32(), o.from_block.raw_u32(), o.from_vreg.raw_u32()))
            .collect();
        v.sort_unstable();
        self.ctx.bp_by_to = v;
    }

    /// The register holding `vreg` at `pos`, if that range's bundle is
    /// already allocated to one.
    fn reg_of_vreg_at(&self, vreg: VRegIndex, pos: ProgPoint, me: LiveBundleIndex) -> Option<PReg> {
        for e in self.vregs[vreg].ranges.iter().take(MAX_RANGES) {
            let r = &self.ranges[e.index];
            if r.range.contains_point(pos) {
                if r.bundle == me || !r.bundle.is_valid() {
                    return None;
                }
                let a = self.bundles[r.bundle].allocation;
                return a.as_reg().filter(|p| !self.pregs[p.index()].is_stack);
            }
        }
        None
    }

    /// The copy hint of `bundle` (see the module doc), or `PReg::invalid()`.
    pub(crate) fn copy_hint(&self, bundle: LiveBundleIndex) -> PReg {
        let mut w: SmallVec<[(PReg, f32); 8]> = SmallVec::new();
        let mut add = |p: PReg, x: f32| {
            if let Some(e) = w.iter_mut().find(|e| e.0 == p) {
                e.1 += x;
            } else {
                w.push((p, x));
            }
        };
        let hot = &self.ctx.cfginfo.hot_bonus;
        let freq = |b: Block| hot.get(b.index()).copied().unwrap_or(1.0);
        for entry in self.bundles[bundle].ranges.iter().take(MAX_RANGES) {
            let ri: LiveRangeIndex = entry.index;
            let r = &self.ranges[ri];
            let v = r.vreg;
            let (from, to) = (r.range.from, r.range.to);
            let fb = self.cfginfo.insn_block[from.inst().index()];
            let lb = self.cfginfo.insn_block[to.prev().inst().index()];
            // Same vreg, abutting inside a block: a split move.
            if from != self.cfginfo.block_entry[fb.index()] {
                if let Some(p) = self.reg_of_vreg_at(v, from.prev(), bundle) {
                    add(p, freq(fb));
                }
            }
            if to.prev() != self.cfginfo.block_exit[lb.index()] {
                if let Some(p) = self.reg_of_vreg_at(v, to, bundle) {
                    add(p, freq(lb));
                }
            }
            // Ranges starting at a block entry: the same vreg live out of
            // a predecessor, or this vreg is the block's parameter.
            if from == self.cfginfo.block_entry[fb.index()] {
                let vr = v.raw_u32();
                let lo = self.ctx.bp_by_to.partition_point(|e| e.0 < vr);
                let mut n = 0;
                for &(tv, fblk, fv) in &self.ctx.bp_by_to[lo..] {
                    if tv != vr || n >= MAX_EDGES {
                        break;
                    }
                    n += 1;
                    let pb = Block::new(fblk as usize);
                    if self.func.block_succs(pb).contains(&fb) {
                        let at = self.cfginfo.block_exit[pb.index()];
                        if let Some(p) = self.reg_of_vreg_at(VRegIndex::new(fv as usize), at, bundle) {
                            add(p, freq(pb));
                        }
                    }
                }
                if n == 0 {
                    for &pb in self.func.block_preds(fb).iter().take(MAX_EDGES) {
                        let at = self.cfginfo.block_exit[pb.index()];
                        if let Some(p) = self.reg_of_vreg_at(v, at, bundle) {
                            add(p, freq(pb).min(freq(fb)));
                        }
                    }
                }
            }
            // Ranges reaching a block exit: blockparam edges out of it (this
            // vreg passed as an argument), or the same vreg live into a
            // successor.
            if to.prev() == self.cfginfo.block_exit[lb.index()] {
                let lo = self
                    .blockparam_outs
                    .partition_point(|o| (o.from_vreg, o.from_block) < (v, lb));
                for o in self.blockparam_outs[lo..].iter().take(MAX_EDGES) {
                    if (o.from_vreg, o.from_block) != (v, lb) {
                        break;
                    }
                    let at = self.cfginfo.block_entry[o.to_block.index()];
                    if let Some(p) = self.reg_of_vreg_at(o.to_vreg, at, bundle) {
                        add(p, freq(lb));
                    }
                }
                for &sb in self.func.block_succs(lb).iter().take(MAX_EDGES) {
                    let at = self.cfginfo.block_entry[sb.index()];
                    if let Some(p) = self.reg_of_vreg_at(v, at, bundle) {
                        add(p, freq(lb).min(freq(sb)));
                    }
                }
            }
        }
        w.iter()
            .copied()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(core::cmp::Ordering::Equal))
            .map_or(PReg::invalid(), |e| e.0)
    }
}

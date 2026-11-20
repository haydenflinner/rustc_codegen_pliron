//! PLIRON (`PLIRON_RA2_POTTS`, see `crate::potts_mode`): after allocation,
//! relabel where each value lives, block by block, to remove edge moves.
//!
//! ion decides a value's register piece by piece (bundles, splits,
//! evictions); where two pieces meet on a CFG edge with different
//! registers, `moves.rs` inserts a move on that edge. On the held-out
//! programs those edge moves are the largest excess over LLVM
//! (`docs/regalloc.md` "Move kinds": compression 27x LLVM's count).
//!
//! Here one value at a time is treated as a labelling problem (a Potts
//! model, `docs/history/2026-11-19-ra-flow.md`): the sites are the blocks
//! where the value is live-in or is a block parameter; each edge into a
//! site pairs the value's place at the predecessor's exit with its place
//! at the site's entry (for a block parameter, the argument's place with
//! the parameter's), and costs the edge's frequency when they differ. A
//! site the value lives through in one live range with only plain uses
//! is a node, free to change register; every other place is fixed.
//!
//! For each candidate register α (the registers the value and its
//! partners across those edges already use), an α-expansion move (every
//! node keeps its register or switches to α, where α is free over the
//! whole block) is solved exactly as a minimum s-t cut: Potts costs are a
//! metric, so the binary energy is submodular (Boykov, Veksler, Zabih).
//! The labelling is kept when its energy, recomputed exactly, falls by
//! more than 1/16. Values go in order of their current edge cost.
//!
//! The frequencies are static estimates, and they can be wrong in the way
//! that matters: regex's `Fsm::follow_epsilons` has an eight-way
//! `EmptyLook` dispatch whose arms each end in a branch back into the
//! loop, which the placement model calls likely; trusting it traded eight
//! reloads in those (in practice cold) arms for reloads on the real back
//! edge and the loop entry, +220k executed loads. Mode `1` therefore takes
//! only Pareto steps: an edge that agrees now weighs `PARETO_W` times its
//! frequency in the cut, and an expansion that still gives any such edge
//! a move is refused, so the result removes moves under every execution
//! profile. Mode `2` trusts the frequencies (for profiled builds).
//!
//! Applying a relabelling only cuts the value's live ranges at the
//! switched blocks' boundaries and gives each block's piece its own
//! bundle with the new register, as `rehome.rs` does; the moves come from
//! the ordinary edge-move insertion. Other values are never touched, so
//! a step can only remove moves of this value's own edges (and of its
//! block-parameter partners' edges, which are counted in its cost).

use super::{Env, LiveRangeKey, LiveRangeListEntry, VRegIndex};
use crate::ion::data_structures::CodeRange;
use crate::ion::regionsplit::{to_cap, Flow};
use crate::{Allocation, Block, Function, FxHashMap, OperandConstraint, OperandKind, PReg, ProgPoint};
use alloc::vec;
use alloc::vec::Vec;
use smallvec::SmallVec;

/// One edge of the value's energy: the place at the predecessor's exit
/// (a node, or fixed) against the place at the successor's entry.
#[derive(Clone, Copy)]
struct Pair {
    a: Side,
    b: Side,
    w: f32,
    /// The edge (for `PLIRON_RA_EXPLAIN`).
    from: Block,
    to: Block,
}

/// A place of the value that may change register: its piece of one block
/// (the whole block, or with `PLIRON_RA2_POTTS_PIECES` the part from the
/// block's entry to its death, or from its definition to the block's end
/// or its death) and the live range holding it.
#[derive(Clone, Copy)]
struct Node {
    blk: Block,
    range: CodeRange,
    lr: super::LiveRangeIndex,
}

#[derive(Clone, Copy)]
enum Side {
    Node(u32),
    Fixed(Allocation),
}

/// Max-flow operations per expansion.
const FLOW_BUDGET: usize = 20_000;
/// Nodes per value (larger values are left alone).
const MAX_NODES: usize = 512;
/// Candidate registers per value.
const MAX_CANDS: usize = 8;
/// Expansion rounds per value.
const ROUNDS: usize = 2;
/// Pareto mode: weight multiplier on edges that agree now.
const PARETO_W: f32 = 1024.0;

impl<'a, F: Function> Env<'a, F> {
    pub(crate) fn potts_relabel(&mut self) {
        let mode = crate::potts_mode();
        let copies = crate::potts_copy_on();
        if mode == 0 && !copies {
            return;
        }
        // Mode 1: only Pareto steps (no edge of the value gains a move), so
        // no execution profile can make the result worse than ion's.
        let pareto = mode == 1;
        let nb = self.func.num_blocks();
        let nv = self.ctx.vregs.len();
        // Whole-function budget: the live-in sets are this function's
        // liveness, so walking them is linear in what regalloc built.
        let mut livein_total = 0usize;
        for b in 0..nb {
            livein_total += self.ctx.liveins[b].iter().count();
        }
        if livein_total > crate::potts_max_liveins() {
            explain!("potts: skipped, {} live-ins", livein_total);
            return;
        }

        // Sorted, current range lists, and the vregs whose ranges overlap.
        let mut overlap: Vec<bool> = Vec::with_capacity(nv);
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

        // Sites per vreg: blocks where it is live-in (and the block it is a
        // parameter of), as a CSR list.
        let mut count = vec![0u32; nv + 1];
        for b in 0..nb {
            for v in self.ctx.liveins[b].iter() {
                count[v] += 1;
            }
            for p in self.func.block_params(Block::new(b)) {
                count[p.vreg()] += 1;
            }
        }
        let mut start = vec![0u32; nv + 1];
        for v in 0..nv {
            start[v + 1] = start[v] + count[v];
        }
        let mut fill = start.clone();
        let mut sites = vec![0u32; start[nv] as usize];
        // Bit 31 marks a parameter site.
        const PARAM: u32 = 1 << 31;
        for b in 0..nb {
            for v in self.ctx.liveins[b].iter() {
                sites[fill[v] as usize] = b as u32;
                fill[v] += 1;
            }
            for p in self.func.block_params(Block::new(b)) {
                sites[fill[p.vreg()] as usize] = b as u32 | PARAM;
                fill[p.vreg()] += 1;
            }
        }

        // (block parameter, predecessor) -> the argument passed.
        let mut args: FxHashMap<(u32, u32), VRegIndex> = FxHashMap::default();
        for o in &self.ctx.blockparam_outs {
            args.insert((o.to_vreg.raw_u32(), o.from_block.raw_u32()), o.from_vreg);
        }

        // Scratch, reused across values.
        let mut stamp = vec![u32::MAX; nb];
        let mut node_of = vec![0u32; nb];
        let mut nodes: Vec<Node> = Vec::new();
        let pieces = crate::potts_pieces_on();
        let mut cur: Vec<Allocation> = Vec::new();
        let mut pairs: Vec<Pair> = Vec::new();

        // First pass: each value's current edge cost, to order the work.
        let mut order: Vec<(f32, u32)> = Vec::new();
        for v in 0..nv {
            if mode == 0 {
                break;
            }
            if overlap[v] || (start[v] == start[v + 1] && !pieces) {
                continue;
            }
            let vi = VRegIndex::new(v);
            if !self.potts_build(vi, &sites[start[v] as usize..start[v + 1] as usize], &args,
                                 v as u32, pieces, &mut stamp, &mut node_of, &mut nodes, &mut cur, &mut pairs) {
                continue;
            }
            let c = energy(&pairs, &cur);
            if c > 0.0 && !nodes.is_empty() {
                order.push((c, v as u32));
            }
        }
        order.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(core::cmp::Ordering::Equal));

        let mut changed = 0usize;
        let mut moved_blocks = 0usize;
        let mut saved = 0.0f32;
        let mut cands: SmallVec<[(PReg, f32); 16]> = SmallVec::new();
        let mut free: Vec<bool> = Vec::new();
        let csr_blocks = if order.is_empty() { Vec::new() } else { self.potts_csr_blocks() };
        for &(_, v) in &order {
            let vi = VRegIndex::new(v as usize);
            // Labels may have moved since the first pass (a partner changed).
            let vs = &sites[start[v as usize] as usize..start[v as usize + 1] as usize];
            if !self.potts_build(vi, vs, &args, v | (1 << 30), pieces, &mut stamp, &mut node_of, &mut nodes,
                                 &mut cur, &mut pairs) {
                continue;
            }
            if nodes.is_empty() || nodes.len() > MAX_NODES {
                continue;
            }
            let orig = cur.clone();
            let e0 = energy(&pairs, &cur);
            if !(e0 > 0.0) {
                continue;
            }
            // Candidates: registers on either side of the value's edges,
            // heaviest first.
            cands.clear();
            for p in &pairs {
                for s in [p.a, p.b] {
                    let a = match s {
                        Side::Node(i) => cur[i as usize],
                        Side::Fixed(a) => a,
                    };
                    let Some(r) = a.as_reg() else { continue };
                    if self.ctx.pregs[r.index()].is_stack {
                        continue;
                    }
                    match cands.iter_mut().find(|c| c.0 == r) {
                        Some(c) => c.1 += p.w,
                        None => cands.push((r, p.w)),
                    }
                }
            }
            cands.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal));
            cands.truncate(MAX_CANDS);

            let mut e = e0;
            for _round in 0..ROUNDS {
                let mut improved = false;
                for k in 0..cands.len() {
                    let alpha = Allocation::reg(cands[k].0);
                    // Which nodes may take alpha: free over the node's piece.
                    free.clear();
                    let alpha_csr = self.potts_is_csr(cands[k].0);
                    for (i, nd) in nodes.iter().enumerate() {
                        let ok = cur[i] == alpha || {
                            let key = LiveRangeKey::from_range(&nd.range);
                            (!alpha_csr || csr_blocks[nd.blk.index()])
                                && !self.ctx.pregs[cands[k].0.index()].allocations.btree.contains_key(&key)
                        };
                        free.push(ok);
                    }
                    let Some(next) = expand(&pairs, &cur, &free, alpha, pareto) else { continue };
                    let e1 = energy(&pairs, &next);
                    if pareto && worsens(&pairs, &cur, &next) {
                        continue;
                    }
                    if e1 < e && e1 <= e * (15.0 / 16.0) {
                        cur = next;
                        e = e1;
                        improved = true;
                    }
                }
                if !improved {
                    break;
                }
            }
            if !(e < e0) {
                continue;
            }
            if !crate::potts_take() {
                continue;
            }
            explain!("potts v{}: edge cost {} -> {}, {} node(s)", v, e0, e, nodes.len());
            if crate::explain_on() {
                for p in &pairs {
                    let (oa, ob) = (place(p.a, &orig), place(p.b, &orig));
                    let (na, nb) = (place(p.a, &cur), place(p.b, &cur));
                    if (oa != ob) != (na != nb) {
                        explain!("potts v{}:   block{}->block{} w {}: {}/{} -> {}/{}", v,
                                 p.from.index(), p.to.index(), p.w, oa, ob, na, nb);
                    }
                }
            }
            changed += 1;
            crate::POTTS_VALUES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            saved += e0 - e;
            for i in 0..nodes.len() {
                if cur[i] == orig[i] {
                    continue;
                }
                let preg = cur[i].as_reg().expect("potts labels are registers");
                self.potts_assign(vi, nodes[i].range, preg);
                moved_blocks += 1;
            }
        }
        if changed > 0 {
            explain!("potts: {} value(s) relabelled, {} block piece(s), weighted edge cost -{}",
                     changed, moved_blocks, saved);
        }
        if copies {
            self.potts_copies(&overlap);
        }
    }

    /// Per block: does any callee-saved register (the environment's
    /// non-preferred set, as Cranelift builds it) already hold a value in
    /// it? A relabelling never brings one into a block that has none: the
    /// energy does not price callee-save placement, and shrink-wrapping
    /// (`PLIRON_SHRINKWRAP`) keeps the blocks ahead of its save point free
    /// of them.
    fn potts_csr_blocks(&self) -> Vec<bool> {
        let nb = self.func.num_blocks();
        let mut out = vec![false; nb];
        for class in 0..3 {
            for preg in self.env.non_preferred_regs_by_class[class] {
                for key in self.ctx.pregs[preg.index()].allocations.btree.keys() {
                    if key.to <= key.from {
                        continue;
                    }
                    let a = ProgPoint::from_index(key.from).inst().index();
                    let b = ProgPoint::from_index(key.to - 1).inst().index();
                    if a >= self.ctx.cfginfo.insn_block.len() || b >= self.ctx.cfginfo.insn_block.len() {
                        continue;
                    }
                    let (ba, bb) = (self.ctx.cfginfo.insn_block[a].index(), self.ctx.cfginfo.insn_block[b].index());
                    for blk in ba..=bb.min(nb - 1) {
                        out[blk] = true;
                    }
                }
            }
        }
        out
    }

    fn potts_is_csr(&self, preg: PReg) -> bool {
        self.env.non_preferred_regs_by_class[preg.class() as u8 as usize].contains(preg)
    }

    /// `PLIRON_RA2_POTTS_COPY`: make register copies identities. For a copy
    /// `d = s` whose two sides hold different registers, give one side the
    /// other's register when that side is a single live range defined by
    /// an instruction, is neither a block parameter nor a branch argument
    /// (so no CFG edge's move changes), has only plain operands, and the
    /// register is free over the whole range. Kept only as a Pareto step
    /// over every copy the range takes part in: none that is an identity
    /// now stops being one, and at least one becomes one.
    fn potts_copies(&mut self, overlap: &[bool]) {
        let n = self.func.num_insts();
        let mut done = 0usize;
        let csr_blocks = self.potts_csr_blocks();
        for i in 0..n {
            let inst = crate::Inst::new(i);
            let Some((src, dst)) = self.func.pliron_move(inst) else { continue };
            let (vs, vd) = (VRegIndex::new(src.vreg()), VRegIndex::new(dst.vreg()));
            if vs == vd || overlap[vs.index()] || overlap[vd.index()] {
                continue;
            }
            let (ps, pd) = (ProgPoint::before(inst), ProgPoint::after(inst));
            let (Some(rs), Some(rd)) = (self.rehome_lr_at(vs, ps), self.rehome_lr_at(vd, pd)) else { continue };
            let (as_, ad) = (self.get_alloc_for_range(rs.index), self.get_alloc_for_range(rd.index));
            if as_ == ad {
                continue;
            }
            // Try the source taking the destination's register, then the
            // destination taking the source's.
            for (v, lr, to) in [(vs, rs, ad), (vd, rd, as_)] {
                if self.potts_copy_try(v, lr, to, &csr_blocks) {
                    done += 1;
                    break;
                }
            }
        }
        if done > 0 {
            explain!("potts: {} copy range(s) relabelled", done);
        }
    }

    /// One side of `potts_copies`: may value `v`'s only live range `lr` take
    /// `to`? Applies it when so.
    fn potts_copy_try(&mut self, v: VRegIndex, lr: LiveRangeListEntry, to: Allocation, csr_blocks: &[bool]) -> bool {
        let Some(preg) = to.as_reg() else { return false };
        if self.potts_is_csr(preg) {
            let ib = &self.ctx.cfginfo.insn_block;
            let a = ib[lr.range.from.inst().index()].index();
            let b = ib[ProgPoint::from_index(lr.range.to.to_index() - 1).inst().index()].index();
            if (a..=b).any(|blk| !csr_blocks[blk]) {
                return false;
            }
        }
        if self.ctx.pregs[preg.index()].is_stack || self.ctx.vregs[v].ranges.len() != 1 {
            return false;
        }
        let r = &self.ctx.ranges[lr.index];
        if self.ctx.bundles[r.bundle].limit.is_some() {
            return false;
        }
        let from = self.get_alloc_for_range(lr.index);
        if from.as_reg().is_none() {
            return false;
        }
        // Defined by an instruction inside the range (not a block parameter),
        // every operand plain.
        let mut has_def = false;
        for u in r.uses.iter() {
            match u.operand.constraint() {
                OperandConstraint::Any | OperandConstraint::Reg => {}
                _ => return false,
            }
            if u.operand.kind() == OperandKind::Def {
                has_def = true;
            }
        }
        if !has_def {
            return false;
        }
        // Not a branch argument.
        let outs = &self.ctx.blockparam_outs;
        let lo = outs.partition_point(|o| o.from_vreg < v);
        if outs.get(lo).is_some_and(|o| o.from_vreg == v) {
            return false;
        }
        // Pareto over the range's copies.
        let mut gain = 0i32;
        for u in r.uses.iter() {
            let inst = u.pos.inst();
            let Some((src, dst)) = self.func.pliron_move(inst) else { continue };
            let (other, at) = if VRegIndex::new(src.vreg()) == v {
                (VRegIndex::new(dst.vreg()), ProgPoint::after(inst))
            } else if VRegIndex::new(dst.vreg()) == v {
                (VRegIndex::new(src.vreg()), ProgPoint::before(inst))
            } else {
                continue;
            };
            let Some(oe) = self.rehome_lr_at(other, at) else { return false };
            let oa = self.get_alloc_for_range(oe.index);
            if oa == from {
                return false;
            }
            if oa == to {
                gain += 1;
            }
        }
        if gain <= 0 {
            return false;
        }
        let key = LiveRangeKey::from_range(&lr.range);
        if self.ctx.pregs[preg.index()].allocations.btree.contains_key(&key) {
            return false;
        }
        if !crate::potts_take() {
            return false;
        }
        explain!("potts copy v{}: {} -> {} ({} copy(s))", v.index(), from, to, gain);
        crate::POTTS_COPIES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let old_bundle = self.ctx.ranges[lr.index].bundle;
        let range = self.ctx.ranges[lr.index].range;
        let piece = self.ctx.bundles.add(self.ctx.scratch_bump.clone());
        self.ctx.bundles[piece].spillset = self.ctx.bundles[old_bundle].spillset;
        self.ctx.bundles[piece].allocation = to;
        self.ctx.bundles[old_bundle].ranges.retain(|x| x.index != lr.index);
        self.ctx.bundles[piece].ranges.push(LiveRangeListEntry { range, index: lr.index });
        self.ctx.ranges[lr.index].bundle = piece;
        let res = self.ctx.pregs[preg.index()].allocations.btree.insert(key, lr.index);
        debug_assert!(res.is_none());
        true
    }

    /// The energy of value `v`: its nodes (blocks it lives through in one
    /// plain range), their current places, and its edge pairs. `false` when
    /// a place cannot be read (the value is left alone).
    #[allow(clippy::too_many_arguments)]
    fn potts_build(
        &self,
        v: VRegIndex,
        vsites: &[u32],
        args: &FxHashMap<(u32, u32), VRegIndex>,
        gen: u32,
        pieces: bool,
        stamp: &mut [u32],
        node_of: &mut [u32],
        nodes: &mut Vec<Node>,
        cur: &mut Vec<Allocation>,
        pairs: &mut Vec<Pair>,
    ) -> bool {
        const PARAM: u32 = 1 << 31;
        nodes.clear();
        cur.clear();
        pairs.clear();
        let hot = &self.ctx.cfginfo.hot_bonus;
        // Nodes first.
        for &s in vsites {
            let blk = Block::new((s & !PARAM) as usize);
            let entry = self.ctx.cfginfo.block_entry[blk.index()];
            let end = self.ctx.cfginfo.block_exit[blk.index()].next();
            let Some(e) = self.rehome_lr_at(v, entry) else { return false };
            // Through the block, or (with pieces) a head piece: live in,
            // dies inside.
            if e.range.to < end && !pieces {
                continue;
            }
            let to = if e.range.to < end { e.range.to } else { end };
            let lr = &self.ctx.ranges[e.index];
            if self.ctx.bundles[lr.bundle].limit.is_some() {
                continue;
            }
            let plain = lr.uses.iter().filter(|u| u.pos >= entry && u.pos < to).all(|u| {
                u.operand.kind() == OperandKind::Use
                    && matches!(u.operand.constraint(), OperandConstraint::Any | OperandConstraint::Reg)
            });
            if !plain {
                continue;
            }
            let a = self.get_alloc_for_range(e.index);
            if a.is_none() {
                continue;
            }
            stamp[blk.index()] = gen;
            node_of[blk.index()] = nodes.len() as u32;
            nodes.push(Node { blk, range: CodeRange { from: entry, to }, lr: e.index });
            cur.push(a);
        }
        // With pieces: the piece from the value's definition to the end of
        // its block (tail) or to its death there (local).
        if pieces {
            if let Some(&first) = self.ctx.vregs[v].ranges.first() {
                let from = first.range.from;
                let blk = self.ctx.cfginfo.insn_block[from.inst().index()];
                let entry = self.ctx.cfginfo.block_entry[blk.index()];
                let end = self.ctx.cfginfo.block_exit[blk.index()].next();
                let lr = &self.ctx.ranges[first.index];
                if from > entry && stamp[blk.index()] != gen && self.ctx.bundles[lr.bundle].limit.is_none() {
                    let to = if first.range.to < end { first.range.to } else { end };
                    let mut has_def = false;
                    let plain = lr.uses.iter().filter(|u| u.pos >= from && u.pos < to).all(|u| {
                        has_def |= u.operand.kind() == OperandKind::Def;
                        matches!(u.operand.constraint(), OperandConstraint::Any | OperandConstraint::Reg)
                    });
                    let a = self.get_alloc_for_range(first.index);
                    if plain && has_def && a.as_reg().is_some() {
                        stamp[blk.index()] = gen;
                        node_of[blk.index()] = nodes.len() as u32;
                        nodes.push(Node { blk, range: CodeRange { from, to }, lr: first.index });
                        cur.push(a);
                    }
                }
            }
            // Copies inside a piece where the value dies (source) or is born
            // (destination): the other side is fixed.
            for i in 0..nodes.len() {
                let nd = nodes[i];
                // A piece that ends inside its block where ion split the
                // value: the next range of the value starts there, and the
                // move between the two is an edge of the energy too.
                let bend = self.ctx.cfginfo.block_exit[nd.blk.index()].next();
                if nd.range.to < bend {
                    if let Some(e2) = self.rehome_lr_at(v, nd.range.to) {
                        if e2.index != nd.lr {
                            let b = Side::Fixed(self.get_alloc_for_range(e2.index));
                            pairs.push(Pair { a: Side::Node(i as u32), b, w: hot[nd.blk.index()], from: nd.blk, to: nd.blk });
                        }
                    }
                }
                for u in self.ctx.ranges[nd.lr].uses.iter() {
                    if u.pos < nd.range.from || u.pos >= nd.range.to {
                        continue;
                    }
                    let inst = u.pos.inst();
                    let Some((src, dst)) = self.func.pliron_move(inst) else { continue };
                    let (other, at) = if VRegIndex::new(src.vreg()) == v
                        && u.operand.kind() == OperandKind::Use
                        && nd.range.to <= ProgPoint::after(inst)
                    {
                        (VRegIndex::new(dst.vreg()), ProgPoint::after(inst))
                    } else if VRegIndex::new(dst.vreg()) == v && u.operand.kind() == OperandKind::Def {
                        (VRegIndex::new(src.vreg()), ProgPoint::before(inst))
                    } else {
                        continue;
                    };
                    let Some(oe) = self.rehome_lr_at(other, at) else { continue };
                    let b = Side::Fixed(self.get_alloc_for_range(oe.index));
                    pairs.push(Pair { a: Side::Node(i as u32), b, w: hot[nd.blk.index()], from: nd.blk, to: nd.blk });
                }
            }
        }
        let side = |blk: Block, at: ProgPoint, of: VRegIndex| -> Option<Side> {
            if of == v && stamp[blk.index()] == gen {
                return Some(Side::Node(node_of[blk.index()]));
            }
            let e = self.rehome_lr_at(of, at)?;
            Some(Side::Fixed(self.get_alloc_for_range(e.index)))
        };
        // Edges into every site.
        for &s in vsites {
            let blk = Block::new((s & !PARAM) as usize);
            let is_param = s & PARAM != 0;
            let entry = self.ctx.cfginfo.block_entry[blk.index()];
            let Some(b) = side(blk, entry, v) else { return false };
            for &p in self.func.block_preds(blk) {
                let src = if is_param {
                    match args.get(&(v.raw_u32(), p.raw_u32())) {
                        Some(&a) => a,
                        None => continue,
                    }
                } else {
                    v
                };
                let Some(a) = side(p, self.ctx.cfginfo.block_exit[p.index()], src) else { return false };
                let w = hot[p.index()].min(hot[blk.index()]);
                pairs.push(Pair { a, b, w, from: p, to: blk });
            }
        }
        // Edges out of a node into another value's parameter (the value as
        // a branch argument).
        let outs = &self.ctx.blockparam_outs;
        let lo = outs.partition_point(|o| o.from_vreg < v);
        for o in &outs[lo..] {
            if o.from_vreg != v {
                break;
            }
            if stamp[o.from_block.index()] != gen {
                continue;
            }
            let a = Side::Node(node_of[o.from_block.index()]);
            let at = self.ctx.cfginfo.block_entry[o.to_block.index()];
            let Some(e) = self.rehome_lr_at(o.to_vreg, at) else { continue };
            let b = Side::Fixed(self.get_alloc_for_range(e.index));
            let w = hot[o.from_block.index()].min(hot[o.to_block.index()]);
            pairs.push(Pair { a, b, w, from: o.from_block, to: o.to_block });
        }
        true
    }

    /// Give `v`'s piece in `blk` (which it lives through in one range) the
    /// register `preg`: cut the range at the block's boundaries and move
    /// the middle piece to a bundle of its own.
    fn potts_assign(&mut self, v: VRegIndex, piece: CodeRange, preg: PReg) {
        let (from, to) = (piece.from, piece.to);
        let Some(e) = self.rehome_lr_at(v, from) else { return };
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
        self.ctx.bundles[piece].ranges.push(LiveRangeListEntry { range, index: mid });
        self.ctx.ranges[mid].bundle = piece;
        let res = self.ctx.pregs[preg.index()]
            .allocations
            .btree
            .insert(LiveRangeKey::from_range(&range), mid);
        debug_assert!(res.is_none());
    }
}

fn place(s: Side, cur: &[Allocation]) -> Allocation {
    match s {
        Side::Node(i) => cur[i as usize],
        Side::Fixed(a) => a,
    }
}

/// Some edge that agrees under `old` disagrees under `new`.
fn worsens(pairs: &[Pair], old: &[Allocation], new: &[Allocation]) -> bool {
    pairs.iter().any(|p| place(p.a, old) == place(p.b, old) && place(p.a, new) != place(p.b, new))
}

/// Weighted edge cost of a labelling.
fn energy(pairs: &[Pair], cur: &[Allocation]) -> f32 {
    let mut c = 0.0f32;
    for p in pairs {
        if place(p.a, cur) != place(p.b, cur) {
            c += p.w;
        }
    }
    c
}

/// The best α-expansion of `cur` (nodes with `free[i]` may switch to
/// `alpha`), by minimum cut. Source side keeps its label, sink side takes
/// `alpha`. `None` when the flow budget runs out or no node may switch.
/// With `pareto`, an edge that agrees now weighs `PARETO_W` times its
/// frequency, so the cut avoids giving it a move whenever any expansion
/// does (the caller still checks).
fn expand(pairs: &[Pair], cur: &[Allocation], free: &[bool], alpha: Allocation, pareto: bool) -> Option<Vec<Allocation>> {
    let n = cur.len();
    let var = |i: u32| free[i as usize] && cur[i as usize] != alpha;
    if !(0..n as u32).any(var) {
        return None;
    }
    // Unary costs (label kept, label alpha) per node.
    let mut u0 = vec![0.0f32; n];
    let mut u1 = vec![0.0f32; n];
    let src = n;
    let sink = n + 1;
    let mut g = Flow::new(n + 2);
    let ne = |x: Allocation, y: Allocation| if x != y { 1.0f32 } else { 0.0 };
    for p0 in pairs {
        let mut p = *p0;
        if pareto && place(p.a, cur) == place(p.b, cur) {
            p.w *= PARETO_W;
        }
        let p = &p;
        match (p.a, p.b) {
            // A block's edge to itself: both ends always agree.
            (Side::Node(i), Side::Node(j)) if i == j => {}
            (Side::Node(i), Side::Node(j)) if var(i) && var(j) => {
                // V(0,0)=A, V(0,1)=B, V(1,0)=C, V(1,1)=D=0 (both alpha).
                let (ci, cj) = (cur[i as usize], cur[j as usize]);
                let a = p.w * ne(ci, cj);
                let b = p.w * ne(ci, alpha);
                let c = p.w * ne(alpha, cj);
                // E = A + (C-A)[x_i] + (D-C)[x_j] + (B+C-A-D)[x_i=0, x_j=1]
                u1[i as usize] += c - a;
                u1[j as usize] -= c;
                let k = b + c - a;
                if k > 0.0 {
                    g.add(i as usize, j as usize, to_cap(k), 0);
                }
            }
            (Side::Node(i), other) | (other, Side::Node(i)) if var(i) => {
                let f = match other {
                    Side::Node(j) => cur[j as usize],
                    Side::Fixed(a) => a,
                };
                u0[i as usize] += p.w * ne(cur[i as usize], f);
                u1[i as usize] += p.w * ne(alpha, f);
            }
            _ => {}
        }
    }
    for i in 0..n {
        if !var(i as u32) {
            continue;
        }
        let m = u0[i].min(u1[i]);
        // x_i = 1 (sink side) cuts src->i; x_i = 0 cuts i->sink.
        let c1 = u1[i] - m;
        let c0 = u0[i] - m;
        if c1 > 0.0 {
            g.add(src, i, to_cap(c1), 0);
        }
        if c0 > 0.0 {
            g.add(i, sink, to_cap(c0), 0);
        }
    }
    g.max_flow(src, sink, FLOW_BUDGET)?;
    let reach = g.source_side(src);
    Some((0..n).map(|i| if var(i as u32) && !reach[i] { alpha } else { cur[i] }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PReg, RegClass};

    fn r(i: usize) -> Allocation {
        Allocation::reg(PReg::new(i, RegClass::Int))
    }

    /// The minimum cut finds the best α-expansion: on random small
    /// energies (nodes, fixed places, weights) it matches brute force over
    /// every keep/switch choice.
    #[test]
    fn expansion_matches_brute_force() {
        let mut seed = 0x9e37_79b9u64;
        let mut rnd = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for _case in 0..2000 {
            let n = 1 + rnd(7) as usize;
            let cur: Vec<Allocation> = (0..n).map(|_| r(rnd(4) as usize)).collect();
            let free: Vec<bool> = (0..n).map(|_| rnd(4) != 0).collect();
            let alpha = r(rnd(4) as usize);
            let mut pairs = Vec::new();
            for _ in 0..rnd(12) + 1 {
                let side = |x: u64, rnd: &mut dyn FnMut(u64) -> u64| {
                    if x < 3 {
                        Side::Node(rnd(n as u64) as u32)
                    } else {
                        Side::Fixed(r(rnd(4) as usize))
                    }
                };
                let xa = rnd(4);
                let a = side(xa, &mut rnd);
                let xb = rnd(4);
                let b = side(xb, &mut rnd);
                let w = (1 + rnd(16)) as f32;
                pairs.push(Pair { a, b, w, from: Block::new(0), to: Block::new(0) });
            }
            let var: Vec<usize> = (0..n).filter(|&i| free[i] && cur[i] != alpha).collect();
            let got = expand(&pairs, &cur, &free, alpha, false);
            if var.is_empty() {
                assert!(got.is_none());
                continue;
            }
            let got = got.expect("small graphs fit the budget");
            let mut best = f32::MAX;
            for mask in 0..(1u32 << var.len()) {
                let mut lab = cur.clone();
                for (k, &i) in var.iter().enumerate() {
                    if mask >> k & 1 == 1 {
                        lab[i] = alpha;
                    }
                }
                best = best.min(energy(&pairs, &lab));
            }
            let e = energy(&pairs, &got);
            assert!((e - best).abs() < 1e-3, "cut energy {e} vs best {best}");
            // Only free nodes move, and only to alpha.
            for i in 0..n {
                assert!(got[i] == cur[i] || (got[i] == alpha && free[i]));
            }
        }
    }
}

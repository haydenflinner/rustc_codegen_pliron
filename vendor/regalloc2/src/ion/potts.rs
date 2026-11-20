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
        if mode == 0 {
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
        let mut nodes: Vec<Block> = Vec::new();
        let mut cur: Vec<Allocation> = Vec::new();
        let mut pairs: Vec<Pair> = Vec::new();

        // First pass: each value's current edge cost, to order the work.
        let mut order: Vec<(f32, u32)> = Vec::new();
        for v in 0..nv {
            if overlap[v] || start[v] == start[v + 1] {
                continue;
            }
            let vi = VRegIndex::new(v);
            if !self.potts_build(vi, &sites[start[v] as usize..start[v + 1] as usize], &args,
                                 v as u32, &mut stamp, &mut node_of, &mut nodes, &mut cur, &mut pairs) {
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
        for &(_, v) in &order {
            let vi = VRegIndex::new(v as usize);
            // Labels may have moved since the first pass (a partner changed).
            let vs = &sites[start[v as usize] as usize..start[v as usize + 1] as usize];
            if !self.potts_build(vi, vs, &args, v | (1 << 30), &mut stamp, &mut node_of, &mut nodes,
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
                    // Which nodes may take alpha: free over the whole block.
                    free.clear();
                    for (i, &blk) in nodes.iter().enumerate() {
                        let ok = cur[i] == alpha || {
                            let key = LiveRangeKey::from_range(&CodeRange {
                                from: self.ctx.cfginfo.block_entry[blk.index()],
                                to: self.ctx.cfginfo.block_exit[blk.index()].next(),
                            });
                            !self.ctx.pregs[cands[k].0.index()].allocations.btree.contains_key(&key)
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
            saved += e0 - e;
            for i in 0..nodes.len() {
                if cur[i] == orig[i] {
                    continue;
                }
                let preg = cur[i].as_reg().expect("potts labels are registers");
                self.potts_assign(vi, nodes[i], preg);
                moved_blocks += 1;
            }
        }
        if changed > 0 {
            explain!("potts: {} value(s) relabelled, {} block piece(s), weighted edge cost -{}",
                     changed, moved_blocks, saved);
        }
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
        stamp: &mut [u32],
        node_of: &mut [u32],
        nodes: &mut Vec<Block>,
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
            if e.range.to < end {
                continue;
            }
            let lr = &self.ctx.ranges[e.index];
            if self.ctx.bundles[lr.bundle].limit.is_some() {
                continue;
            }
            let plain = lr.uses.iter().filter(|u| u.pos >= entry && u.pos < end).all(|u| {
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
            nodes.push(blk);
            cur.push(a);
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
    fn potts_assign(&mut self, v: VRegIndex, blk: Block, preg: PReg) {
        let from = self.ctx.cfginfo.block_entry[blk.index()];
        let to = self.ctx.cfginfo.block_exit[blk.index()].next();
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

//! PLIRON: chain-based block placement with static branch probabilities
//! (`PLIRON_PLACE=1`), in the style of LLVM's MachineBlockPlacement.
//!
//! Runs at emission on the VCode CFG, after register allocation, and only
//! chooses the order blocks are emitted in: `MachBuffer::optimize_branches`
//! then turns adjacency into fallthrough. Lowering and regalloc see the
//! RPO-based `BlockLoweringOrder` as before; cold blocks still go last.
//!
//! 1. Edge probabilities (Ball-Larus-lite): a cold target is unlikely, a
//!    back edge is likely, a loop exit is unlikely, otherwise even.
//! 2. Block frequencies by packaged loops (BlockFrequencyInfo): a loop's
//!    body is distributed relative to its header once, scaled by
//!    `1 / (1 - back-edge mass)`, capped, and packaged into its parent.
//! 3. Chains: per loop, innermost first, a greedy walk from the header
//!    takes the hottest unplaced successor (unless a hotter predecessor
//!    of that successor is still unplaced); loop chains are rotated to
//!    the layout with the fewest expected taken branches; the function
//!    chain is built from the entry the same way, with loop chains as
//!    units.
//!
//! Bails (keeps the incoming order) on irreducible or oversized CFGs.

use alloc::vec;
use alloc::vec::Vec;

/// Probability weights (out of their row's sum).
const W_EVEN: f64 = 32.0;
const W_COLD: f64 = 1.0;
const W_BACK: f64 = 124.0;
const W_NOT_BACK: f64 = 4.0;
/// A CLIF-hinted unlikely arm against an even sibling (~11%).
const W_UNLIKELY: f64 = 4.0;
/// Loop scale cap (LLVM caps at 2^12 per loop; nested loops multiply).
const SCALE_CAP: f64 = 4096.0;
const FREQ_CAP: f64 = 1.0e12;
/// A conditional branch instruction that is not taken still costs this
/// fraction of a taken one (fetch slot). Breaks the tie between
/// `H: b.!c exit; B: ...; b H` (two branch instructions per iteration)
/// and `B: ...; H: b.c B` (one) in favor of the latter.
const COND_INST_COST: f64 = 0.2;
const MAX_BLOCKS: usize = 20_000;
const MAX_LOOP_WALK: usize = 400_000;
const MAX_ROTATE: usize = 64;

/// Total order on f64 for the heap (NaN never occurs).
#[derive(PartialEq, PartialOrd)]
struct OrdF(f64);
impl Eq for OrdF {}
impl Ord for OrdF {
    fn cmp(&self, o: &Self) -> core::cmp::Ordering {
        self.partial_cmp(o).unwrap_or(core::cmp::Ordering::Equal)
    }
}

struct Loop {
    header: usize,
    /// Blocks whose innermost loop is this one.
    members: Vec<usize>,
    parent: Option<usize>,
    /// Sub-loops (innermost-first order is by ascending `size`).
    size: usize,
    /// Relative mass leaving the loop: (target, mass per unit header mass,
    /// already scaled).
    exits: Vec<(usize, f64)>,
    scale: f64,
    /// Entry mass relative to the parent's header.
    entry: f64,
    factor: f64,
}

/// What `analyze` computes: the loop forest, edge probabilities and block
/// frequencies that both placement and (through `frequencies`) the
/// register allocator's spill weights use.
struct Freq {
    n: usize,
    preds: Vec<Vec<usize>>,
    rpo: Vec<usize>,
    rpo_idx: Vec<usize>,
    loops: Vec<Loop>,
    /// Loop ids by ascending size (innermost first).
    asc: Vec<usize>,
    innermost: Vec<Option<usize>>,
    in_loop: Vec<Vec<bool>>,
    prob: Vec<Vec<f64>>,
    freq: Vec<f64>,
}

/// Block frequencies relative to the entry block (`succs`, `cold`,
/// `unlikely` as for `place`), or `None` where `place` would also give up
/// (tiny, huge or irreducible graphs, a cold entry).
pub fn frequencies(entry: usize, succs: &[Vec<usize>], cold: &[bool], unlikely: &[Vec<bool>]) -> Option<Vec<f64>> {
    analyze(entry, succs, cold, unlikely).map(|a| a.freq)
}

fn analyze(entry: usize, succs: &[Vec<usize>], cold: &[bool], unlikely: &[Vec<bool>]) -> Option<Freq> {
    let n = succs.len();
    if n < 3 || n > MAX_BLOCKS || cold[entry] {
        return None;
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for b in 0..n {
        for &s in &succs[b] {
            preds[s].push(b);
        }
    }
    // RPO over the whole graph (DFS, iterative).
    let mut rpo = Vec::with_capacity(n);
    {
        let mut seen = vec![false; n];
        let mut stack: Vec<(usize, usize)> = vec![(entry, 0)];
        seen[entry] = true;
        while let Some(&mut (b, ref mut i)) = stack.last_mut() {
            if *i < succs[b].len() {
                let s = succs[b][*i];
                *i += 1;
                if !seen[s] {
                    seen[s] = true;
                    stack.push((s, 0));
                }
            } else {
                rpo.push(b);
                stack.pop();
            }
        }
        rpo.reverse();
    }
    let mut rpo_idx = vec![usize::MAX; n];
    for (i, &b) in rpo.iter().enumerate() {
        rpo_idx[b] = i;
    }
    // Dominators (Cooper-Harvey-Kennedy) on reachable blocks.
    let mut idom = vec![usize::MAX; n];
    idom[entry] = entry;
    let intersect = |idom: &[usize], rpo_idx: &[usize], mut a: usize, mut b: usize| {
        while a != b {
            while rpo_idx[a] > rpo_idx[b] {
                a = idom[a];
            }
            while rpo_idx[b] > rpo_idx[a] {
                b = idom[b];
            }
        }
        a
    };
    let mut changed = true;
    let mut rounds = 0;
    while changed {
        changed = false;
        rounds += 1;
        if rounds > 64 {
            return None;
        }
        for &b in rpo.iter().skip(1) {
            let mut new = usize::MAX;
            for &p in &preds[b] {
                if idom[p] == usize::MAX {
                    continue;
                }
                new = if new == usize::MAX { p } else { intersect(&idom, &rpo_idx, new, p) };
            }
            if new != usize::MAX && idom[b] != new {
                idom[b] = new;
                changed = true;
            }
        }
    }
    let dominates = |idom: &[usize], rpo_idx: &[usize], a: usize, mut b: usize| -> bool {
        loop {
            if a == b {
                return true;
            }
            if b == entry || idom[b] == usize::MAX {
                return false;
            }
            if rpo_idx[b] < rpo_idx[a] {
                return false;
            }
            b = idom[b];
        }
    };
    // Natural loops from dominance back edges; a retreating edge whose
    // target does not dominate its source makes the CFG irreducible: bail.
    let mut header_of: Vec<Option<usize>> = vec![None; n]; // header -> loop id
    let mut loops: Vec<Loop> = Vec::new();
    let mut in_loop: Vec<Vec<bool>> = Vec::new();
    let mut walk = 0usize;
    for &b in &rpo {
        for &s in &succs[b] {
            if rpo_idx[s] <= rpo_idx[b] {
                if !dominates(&idom, &rpo_idx, s, b) {
                    return None;
                }
                let id = match header_of[s] {
                    Some(id) => id,
                    None => {
                        let id = loops.len();
                        header_of[s] = Some(id);
                        loops.push(Loop {
                            header: s,
                            members: Vec::new(),
                            parent: None,
                            size: 0,
                            exits: Vec::new(),
                            scale: 1.0,
                            entry: 0.0,
                            factor: 1.0,
                        });
                        let mut m = vec![false; n];
                        m[s] = true;
                        in_loop.push(m);
                        id
                    }
                };
                // Body: everything that reaches the latch without passing
                // through the header.
                let mut stack = vec![b];
                while let Some(x) = stack.pop() {
                    if in_loop[id][x] {
                        continue;
                    }
                    in_loop[id][x] = true;
                    walk += 1;
                    if walk > MAX_LOOP_WALK {
                        return None;
                    }
                    for &p in &preds[x] {
                        if rpo_idx[p] != usize::MAX {
                            stack.push(p);
                        }
                    }
                }
            }
        }
    }
    for (id, m) in in_loop.iter().enumerate() {
        loops[id].size = m.iter().filter(|&&x| x).count();
    }
    // Innermost loop per block and loop parents: visit loops by descending
    // size, so each block's last assignment is its smallest loop and a
    // loop's header is already owned by its smallest enclosing loop.
    let mut by_size: Vec<usize> = (0..loops.len()).collect();
    by_size.sort_by_key(|&id| core::cmp::Reverse(loops[id].size));
    let mut innermost: Vec<Option<usize>> = vec![None; n];
    for &id in &by_size {
        loops[id].parent = innermost[loops[id].header];
        for b in 0..n {
            if in_loop[id][b] {
                innermost[b] = Some(id);
            }
        }
    }
    for b in 0..n {
        if let Some(id) = innermost[b] {
            loops[id].members.push(b);
        }
    }
    // Edge probabilities.
    let is_back = |b: usize, s: usize| rpo_idx[s] <= rpo_idx[b];
    let prob: Vec<Vec<f64>> = (0..n)
        .map(|b| {
            let ss = &succs[b];
            if ss.is_empty() {
                return Vec::new();
            }
            let any_back = ss.iter().any(|&s| is_back(b, s) && !cold[s]);
            let lp = innermost[b];
            // Staying in b's innermost loop (entering a sub-loop counts as
            // staying) against leaving it.
            let stays = |s: usize| lp.map_or(false, |l| in_loop[l][s]);
            let any_stay = ss.iter().any(|&s| !cold[s] && stays(s) && !is_back(b, s));
            let any_exit = ss.iter().any(|&s| !cold[s] && lp.is_some() && !stays(s));
            let any_hint = unlikely[b].iter().any(|&u| u);
            let w: Vec<f64> = ss
                .iter()
                .enumerate()
                .map(|(k, &s)| {
                    if cold[s] {
                        W_COLD
                    } else if is_back(b, s) {
                        if any_back { W_BACK } else { W_EVEN }
                    } else if any_back {
                        W_NOT_BACK
                    } else if any_hint {
                        if unlikely[b][k] { W_UNLIKELY } else { W_EVEN }
                    } else if any_stay && any_exit {
                        if stays(s) { W_BACK } else { W_NOT_BACK }
                    } else {
                        W_EVEN
                    }
                })
                .collect();
            let sum: f64 = w.iter().sum();
            w.into_iter().map(|x| x / sum).collect()
        })
        .collect();
    // Block frequencies by packaged loops, innermost first.
    let mut asc: Vec<usize> = (0..loops.len()).collect();
    asc.sort_by_key(|&id| loops[id].size);
    let mut rel = vec![0.0f64; n];
    // Distribute mass `m` from block `b` along edge to `s` within the loop
    // `lp` (None = function root): returns it to the caller's accumulators.
    fn distribute(
        lp: Option<usize>,
        s: usize,
        m: f64,
        loops: &mut [Loop],
        innermost: &[Option<usize>],
        in_loop: &[Vec<bool>],
        rel: &mut [f64],
        back: &mut f64,
        exits: &mut Vec<(usize, f64)>,
    ) {
        let header = lp.map(|l| loops[l].header);
        if Some(s) == header {
            *back += m;
            return;
        }
        if innermost[s] == lp {
            rel[s] += m;
            return;
        }
        // Inside this loop but in a sub-loop (its header, in a reducible
        // CFG), or outside this loop entirely.
        match (lp, innermost[s]) {
            (Some(l), Some(sl)) if in_loop[l][s] => {
                // Find the sub-loop of `l` that contains `s`.
                let mut id = sl;
                while loops[id].parent != Some(l) {
                    id = match loops[id].parent {
                        Some(p) => p,
                        None => break,
                    };
                }
                if loops[id].header == s {
                    loops[id].entry += m;
                } else {
                    exits.push((s, m));
                }
            }
            (None, Some(sl)) => {
                let mut id = sl;
                while let Some(p) = loops[id].parent {
                    id = p;
                }
                if loops[id].header == s {
                    loops[id].entry += m;
                } else {
                    exits.push((s, m));
                }
            }
            _ => exits.push((s, m)),
        }
    }
    let package = |lp: Option<usize>, loops: &mut Vec<Loop>, rel: &mut Vec<f64>| {
        // Blocks and sub-loop headers in this region, in RPO.
        let mut items: Vec<usize> = match lp {
            Some(l) => loops[l].members.clone(),
            None => (0..n).filter(|&b| innermost[b].is_none() && rpo_idx[b] != usize::MAX).collect(),
        };
        let subs: Vec<usize> = (0..loops.len()).filter(|&id| loops[id].parent == lp).collect();
        for &id in &subs {
            items.push(loops[id].header);
            loops[id].entry = 0.0;
        }
        items.sort_by_key(|&b| rpo_idx[b]);
        let start = match lp {
            Some(l) => loops[l].header,
            None => entry,
        };
        let mut back = 0.0;
        let mut exits: Vec<(usize, f64)> = Vec::new();
        for &b in &items {
            if b == start {
                rel[b] = 1.0;
            }
            if innermost[b] == lp {
                let m = rel[b];
                if m == 0.0 {
                    continue;
                }
                for (k, &s) in succs[b].iter().enumerate() {
                    distribute(lp, s, m * prob[b][k], loops, &innermost, &in_loop, rel, &mut back, &mut exits);
                }
            } else {
                // A sub-loop's header: package its exits.
                let id = subs.iter().copied().find(|&id| loops[id].header == b).unwrap();
                let e = loops[id].entry;
                if e == 0.0 {
                    continue;
                }
                let sub_exits = core::mem::take(&mut loops[id].exits);
                for &(t, m) in &sub_exits {
                    distribute(lp, t, e * m, loops, &innermost, &in_loop, rel, &mut back, &mut exits);
                }
                loops[id].exits = sub_exits;
            }
        }
        if let Some(l) = lp {
            let scale = if back >= 1.0 { SCALE_CAP } else { (1.0 / (1.0 - back)).min(SCALE_CAP) };
            loops[l].scale = scale;
            loops[l].exits = exits.into_iter().map(|(t, m)| (t, m * scale)).collect();
        }
    };
    for &id in &asc {
        package(Some(id), &mut loops, &mut rel);
    }
    package(None, &mut loops, &mut rel);
    for &id in &by_size {
        let f = match loops[id].parent {
            Some(p) => loops[p].factor,
            None => 1.0,
        };
        loops[id].factor = (f * loops[id].entry * loops[id].scale).min(FREQ_CAP);
    }
    let freq: Vec<f64> = (0..n)
        .map(|b| match innermost[b] {
            Some(l) => (loops[l].factor * rel[b]).min(FREQ_CAP),
            None => rel[b].min(FREQ_CAP),
        })
        .collect();
    Some(Freq { n, preds, rpo, rpo_idx, loops, asc, innermost, in_loop, prob, freq })
}

/// `succs[b]` lists b's successors (with repeats for `br_table`); `cold[b]`
/// marks blocks that are emitted last anyway. Returns the emission order
/// of the non-cold blocks, entry first, or `None` to keep the old order.
/// `empty[b]`: b is only an unconditional jump (its label is aliased to the
/// target at emission), so it costs nothing wherever it is placed and is
/// never worth a fallthrough slot.
/// Returns the emission order of the non-cold blocks (entry first) and
/// the blocks that start a loop chain (aligned like loop headers), or
/// `None` to keep the old order.
/// `unlikely[b][k]`: the CLIF hinted successor `k` of `b` unlikely.
pub fn place(entry: usize, succs: &[Vec<usize>], cold: &[bool], empty: &[bool], unlikely: &[Vec<bool>]) -> Option<(Vec<usize>, Vec<bool>)> {
    let Freq { n, preds, rpo, rpo_idx, loops, asc, innermost, in_loop, prob, freq } = analyze(entry, succs, cold, unlikely)?;

    // An empty jump block emits nothing (its label aliases the target), so
    // chains see through it: `res[b]` is the first non-empty block reached.
    let res: Vec<usize> = (0..n)
        .map(|b| {
            let mut t = b;
            let mut hops = 0;
            while empty[t] && hops < 8 {
                t = succs[t][0];
                hops += 1;
            }
            if empty[t] { b } else { t }
        })
        .collect();
    // Chains.
    let mut chain_of: Vec<usize> = (0..n).collect();
    let mut chains: Vec<Vec<usize>> = (0..n).map(|b| vec![b]).collect();
    // A chain that has been appended to another is no longer free.
    let mut free = vec![true; n];
    for b in 0..n {
        if cold[b] || empty[b] || rpo_idx[b] == usize::MAX {
            free[b] = false;
        }
    }
    let edge_w = |b: usize, k: usize| freq[b] * prob[b][k];
    // A predecessor through a back edge (later in RPO) is never waited for.
    let in_loop_back = |_lp: Option<usize>, p: usize, b: usize| rpo_idx[p] >= rpo_idx[b];
    // PLIRON (`PLIRON_LOOPFT=0` disables): on edge-weight ties prefer
    // a successor that can reach a loop — loop work stays on the
    // straight-line fetch path while a leaf/return arm takes the
    // out-of-line spot. Recursive hot loops otherwise pay a taken
    // front-end redirect per non-leaf call (~1.4x on fib).
    let loopft = std::env::var("PLIRON_LOOPFT").as_deref() == Ok("1");
    let reach_loop: Vec<bool> = {
        let mut reach = vec![false; n];
        if loopft {
            let mut stack = Vec::new();
            for b in 0..n {
                if innermost[b].is_some() {
                    reach[b] = true;
                    stack.push(b);
                }
            }
            while let Some(b) = stack.pop() {
                for &p in &preds[b] {
                    if !reach[p] {
                        reach[p] = true;
                        stack.push(p);
                    }
                }
            }
        }
        reach
    };
    // Build the chain for region `lp`, starting at `start`.
    let build = |lp: Option<usize>, start: usize, chain_of: &mut Vec<usize>, chains: &mut Vec<Vec<usize>>, free: &mut Vec<bool>| {
        let in_region = |b: usize| match lp {
            Some(l) => in_loop[l][b],
            None => true,
        };
        let cur = chain_of[start];
        free[cur] = false;
        let mut placed_here = vec![false; n];
        for &b in &chains[cur] {
            placed_here[b] = true;
        }
        // Fallback when no successor can follow (MachineBlockPlacement's
        // rule): the hottest region block whose predecessors are all
        // placed, else the first unplaced region block in RPO. `pending`
        // counts a block's unplaced region predecessors; a block that
        // reaches zero goes on the heap (lazily validated when popped).
        let region_pred = |b: usize, p: usize| -> bool {
            !cold[p] && !empty[p] && rpo_idx[p] != usize::MAX && in_region(p) && p != b && !(lp.is_some() && b == start)
        };
        let mut pending = vec![0usize; n];
        let mut heap: alloc::collections::BinaryHeap<(OrdF, core::cmp::Reverse<usize>)> = alloc::collections::BinaryHeap::new();
        let mut rpo_cands: Vec<usize> = match lp {
            Some(l) => rpo.iter().copied().filter(|&b| in_loop[l][b] && !cold[b] && !empty[b]).collect(),
            None => rpo.iter().copied().filter(|&b| !cold[b] && !empty[b]).collect(),
        };
        rpo_cands.reverse();
        for &b in &rpo_cands {
            let mut k = 0;
            for &p in &preds[b] {
                // Through empty blocks: the real predecessor is the one
                // that jumps to the empty block.
                let mut q = p;
                let mut hops = 0;
                while empty[q] && hops < 8 {
                    match preds[q].first() {
                        Some(&pp) if preds[q].len() == 1 => q = pp,
                        _ => break,
                    }
                    hops += 1;
                }
                if region_pred(b, q) && !placed_here[q] && !in_loop_back(lp, q, b) {
                    k += 1;
                }
            }
            pending[b] = k;
        }
        let mut note_placed = |b: usize, pending: &mut Vec<usize>, heap: &mut alloc::collections::BinaryHeap<(OrdF, core::cmp::Reverse<usize>)>| {
            for &s in &succs[b] {
                let s = res[s];
                if pending[s] > 0 {
                    pending[s] -= 1;
                    if pending[s] == 0 {
                        heap.push((OrdF(freq[s]), core::cmp::Reverse(s)));
                    }
                }
            }
        };
        for b in 0..n {
            if placed_here[b] {
                note_placed(b, &mut pending, &mut heap);
            }
        }
        loop {
            let tail = *chains[cur].last().unwrap();
            let mut best: Option<(usize, (f64, bool, bool))> = None;
            for (k, &s) in succs[tail].iter().enumerate() {
                let s = res[s];
                if cold[s] || empty[s] || !in_region(s) {
                    continue;
                }
                let c = chain_of[s];
                if c == cur || !free[c] || chains[c][0] != s {
                    continue;
                }
                let w = edge_w(tail, k);
                // A hotter unplaced predecessor should get `s` as its
                // fallthrough instead (MachineBlockPlacement's rule).
                let mut better_pred = false;
                for &p in &preds[s] {
                    if p == tail || cold[p] || !in_region(p) || placed_here[p] {
                        continue;
                    }
                    if chain_of[p] == c {
                        continue;
                    }
                    let pw: f64 = succs[p]
                        .iter()
                        .enumerate()
                        .filter(|&(_, &t)| res[t] == s)
                        .map(|(kk, _)| edge_w(p, kk))
                        .sum();
                    if pw > w {
                        better_pred = true;
                        break;
                    }
                }
                if better_pred {
                    continue;
                }
                // Ties go to a successor none of whose other predecessors
                // is unplaced (the arm of an if, not its join).
                let unplaced_other = preds[s].iter().any(|&p| p != tail && !placed_here[p] && !cold[p] && in_region(p) && chain_of[p] != c);
                let key = (w, reach_loop[s], !unplaced_other);
                if best.map_or(true, |(_, bk)| key > bk) {
                    best = Some((s, key));
                }
            }
            let next = match best {
                Some((s, _)) => Some(s),
                None => {
                    let ok = |h: usize, chain_of: &Vec<usize>, chains: &Vec<Vec<usize>>, free: &Vec<bool>| {
                        let c = chain_of[h];
                        c != cur && free[c] && chains[c][0] == h && in_region(h)
                    };
                    let mut pick = None;
                    while let Some((_, core::cmp::Reverse(h))) = heap.pop() {
                        if ok(h, chain_of, chains, free) {
                            pick = Some(h);
                            break;
                        }
                    }
                    if pick.is_none() {
                        while let Some(h) = rpo_cands.pop() {
                            if ok(h, chain_of, chains, free) {
                                pick = Some(h);
                                break;
                            }
                        }
                    }
                    pick
                }
            };
            let Some(s) = next else { break };
            let c = chain_of[s];
            free[c] = false;
            let tail_chain = core::mem::take(&mut chains[c]);
            for &b in &tail_chain {
                chain_of[b] = cur;
                placed_here[b] = true;
                note_placed(b, &mut pending, &mut heap);
            }
            chains[cur].extend(tail_chain);
        }
        free[cur] = true;
        cur
    };
    // Expected branch cost of laying `order` out contiguously; `entry_mass`
    // is charged when the first block is not `header`.
    let layout_cost = |order: &[usize], header: usize, entry_mass: f64| -> f64 {
        let mut cost = 0.0;
        let mut in_chain = vec![false; n];
        for &b in order {
            in_chain[b] = true;
        }
        for (i, &b) in order.iter().enumerate() {
            let mut next = order.get(i + 1).copied();
            let ss: Vec<usize> = succs[b].iter().map(|&s| res[s]).collect();
            if next.is_none() {
                // The chain that follows this one will take the hottest
                // outside successor as its head.
                let mut best: Option<(usize, f64)> = None;
                for (k, &s) in ss.iter().enumerate() {
                    if !in_chain[s] && !cold[s] {
                        let w = prob[b][k];
                        if best.map_or(true, |(_, bw)| w > bw) {
                            best = Some((s, w));
                        }
                    }
                }
                next = best.map(|(s, _)| s);
            }
            let distinct: Vec<usize> = {
                let mut d: Vec<usize> = ss.clone();
                d.sort_unstable();
                d.dedup();
                d
            };
            match distinct.len() {
                0 => {}
                1 => {
                    if Some(distinct[0]) != next {
                        cost += freq[b] * (1.0 + COND_INST_COST);
                    }
                }
                2 => {
                    cost += freq[b] * COND_INST_COST;
                    let mut both = true;
                    for (k, &s) in ss.iter().enumerate() {
                        if Some(s) != next {
                            cost += freq[b] * prob[b][k];
                        } else {
                            both = false;
                        }
                    }
                    if both {
                        cost += freq[b] * COND_INST_COST;
                    }
                }
                _ => cost += freq[b],
            }
        }
        if order.first() != Some(&header) {
            cost += entry_mass;
        }
        cost
    };
    // Loop rotation at emission (`PLIRON_PLACE_ROT=1`): costs a taken jump
    // per loop entry that IR-level rotation (`looprot`) avoids with a
    // fallthrough guard, so it is off by default.
    static ROT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let rotate = *ROT.get_or_init(|| std::env::var("PLIRON_PLACE_ROT").as_deref() == Ok("1"));
    let mut heads = vec![false; n];
    for &id in &asc {
        let h = loops[id].header;
        let c = build(Some(id), h, &mut chain_of, &mut chains, &mut free);
        // Rotate to the cheapest layout.
        let len = chains[c].len();
        if rotate && len >= 2 && len <= MAX_ROTATE && h != entry {
            let entry_mass: f64 = preds[h]
                .iter()
                .filter(|&&p| !in_loop[id][p])
                .map(|&p| succs[p].iter().enumerate().filter(|&(_, &t)| t == h).map(|(k, _)| edge_w(p, k)).sum::<f64>())
                .sum();
            let base = layout_cost(&chains[c], h, entry_mass);
            let mut best = (base, 0usize);
            let mut rot = chains[c].clone();
            for r in 1..len {
                rot.rotate_left(1);
                let cost = layout_cost(&rot, h, entry_mass);
                if cost < best.0 - 1e-9 {
                    best = (cost, r);
                }
            }
            if best.1 != 0 {
                chains[c].rotate_left(best.1);
            }
        }
        if let Some(&first) = chains[c].first() {
            heads[first] = true;
        }
    }
    let root = build(None, entry, &mut chain_of, &mut chains, &mut free);
    let mut order = core::mem::take(&mut chains[root]);
    // Anything left (unreachable from the entry through hot edges).
    let mut rest: Vec<usize> = (0..n).filter(|&b| free[b] && b != root).collect();
    rest.sort_by(|&a, &b| freq[chains[b][0]].partial_cmp(&freq[chains[a][0]]).unwrap_or(core::cmp::Ordering::Equal));
    for c in rest {
        order.extend(core::mem::take(&mut chains[c]));
    }
    order.extend((0..n).filter(|&b| empty[b] && !cold[b] && rpo_idx[b] != usize::MAX));
    if order.first() != Some(&entry) {
        return None;
    }
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *DEBUG.get_or_init(|| std::env::var("PLIRON_PLACE_DEBUG").as_deref() == Ok("1")) {
        let mut s = alloc::string::String::new();
        for b in 0..n {
            let ss: Vec<alloc::string::String> = succs[b]
                .iter()
                .enumerate()
                .map(|(k, &t)| alloc::format!("{}@{:.2}", t, prob[b][k]))
                .collect();
            s.push_str(&alloc::format!(
                "place: b{} freq={:.3} loop={:?}{}{} -> {}\n",
                b,
                freq[b],
                innermost[b],
                if cold[b] { " cold" } else { "" },
                if empty[b] { " empty" } else { "" },
                ss.join(" ")
            ));
        }
        s.push_str(&alloc::format!("place: order {:?}\n", order));
        std::eprint!("{}", s);
    }
    Some((order, heads))
}


//! PLIRON: exact register lower/upper bounds on an interference graph.
//!
//! The assembly-line view of regalloc: at each program point the live set
//! is a clique (those values must sit in distinct registers). The size of
//! the largest such clique is a *proof* that you need at least that many
//! registers of that class — no heuristic can beat it without spilling.
//! A colouring with K colours is a proof that K suffice (ignoring FixedReg
//! / clobber / ABI pins; those only raise the real need).
//!
//! Binary-search the gap: if ion used R and we prove lower = upper = K with
//! K < R, the heuristic left register pressure on the table. If lower = R,
//! the heuristic is optimal on this abstraction.
//!
//! Caps: exact clique + colouring for ≤ `MAX_EXACT` vertices of one class;
//! beyond that we still report a greedy-clique lower bound.

use crate::{
    Allocation, Block, Edit, Function, OperandKind, Output, PReg, RegClass, VReg,
};
use alloc::vec;
use alloc::vec::Vec;

/// Hard cap for exact (exponential) search. Above this, lower bound only.
pub const MAX_EXACT: usize = 48;

#[derive(Clone, Debug)]
pub struct IGraph {
    pub n: usize,
    /// Sorted adjacency lists (undirected).
    pub adj: Vec<Vec<usize>>,
    pub class: Vec<RegClass>,
}

#[derive(Clone, Debug)]
pub struct CliqueCert {
    pub nodes: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct ColourCert {
    /// `colour[v]` in `0..K`, or `u32::MAX` if v is not in the subgraph.
    pub colour: Vec<u32>,
    pub k: u32,
}

#[derive(Clone, Debug)]
pub struct Bound {
    pub lower: CliqueCert,
    pub lower_k: u32,
    pub upper: Option<ColourCert>,
    /// `None` if the exact colouring search was skipped or timed out.
    pub upper_k: Option<u32>,
    pub exact: bool,
    pub n_class: usize,
}

fn bit_words(n: usize) -> usize {
    (n + 63) / 64
}
fn bit_set(bits: &mut [u64], i: usize) {
    bits[i / 64] |= 1u64 << (i % 64);
}
fn bit_get(bits: &[u64], i: usize) -> bool {
    bits[i / 64] & (1u64 << (i % 64)) != 0
}
fn bit_or_into(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d |= *s;
    }
}

/// Build an interference graph from SSA-ish liveness (same shape as greedy).
pub fn interfere_graph<F: Function>(func: &F) -> IGraph {
    let n = func.num_vregs();
    let mut class = vec![RegClass::Int; n];
    let mut note = |v: VReg| {
        let i = v.vreg();
        if i < n {
            class[i] = v.class();
        }
    };

    let nb = func.num_blocks();
    let w = bit_words(n.max(1));
    let mut ue_use = vec![0u64; nb * w.max(1)];
    let mut defs = vec![0u64; nb * w.max(1)];

    if n == 0 {
        return IGraph {
            n: 0,
            adj: Vec::new(),
            class,
        };
    }

    for b in 0..nb {
        let block = Block::new(b);
        let ue = &mut ue_use[b * w..(b + 1) * w];
        let df = &mut defs[b * w..(b + 1) * w];
        let mut seen_def = vec![0u64; w];
        for &v in func.block_params(block) {
            note(v);
            let i = v.vreg();
            if i < n {
                bit_set(df, i);
                bit_set(&mut seen_def, i);
            }
        }
        for inst in func.block_insns(block).iter() {
            for op in func.inst_operands(inst) {
                if op.as_fixed_nonallocatable().is_some() {
                    continue;
                }
                note(op.vreg());
                let i = op.vreg().vreg();
                if i >= n {
                    continue;
                }
                if op.kind() == OperandKind::Use && !bit_get(&seen_def, i) {
                    bit_set(ue, i);
                }
            }
            for op in func.inst_operands(inst) {
                if op.as_fixed_nonallocatable().is_some() {
                    continue;
                }
                if op.kind() == OperandKind::Def {
                    let i = op.vreg().vreg();
                    if i < n {
                        bit_set(df, i);
                        bit_set(&mut seen_def, i);
                    }
                }
            }
        }
        let term = func.block_insns(block).last();
        for si in 0..func.block_succs(block).len() {
            for &v in func.branch_blockparams(block, term, si) {
                note(v);
                let i = v.vreg();
                if i < n && !bit_get(&seen_def, i) {
                    bit_set(ue, i);
                }
            }
        }
    }

    let mut live_in = vec![0u64; nb * w];
    let mut live_out = vec![0u64; nb * w];
    let mut changed = true;
    while changed {
        changed = false;
        for b in 0..nb {
            let block = Block::new(b);
            let mut new_out = vec![0u64; w];
            for &succ in func.block_succs(block) {
                let s = succ.index();
                bit_or_into(&mut new_out, &live_in[s * w..(s + 1) * w]);
            }
            let mut new_in = ue_use[b * w..(b + 1) * w].to_vec();
            for i in 0..n {
                if bit_get(&new_out, i) && !bit_get(&defs[b * w..(b + 1) * w], i) {
                    bit_set(&mut new_in, i);
                }
            }
            if new_out.as_slice() != &live_out[b * w..(b + 1) * w]
                || new_in.as_slice() != &live_in[b * w..(b + 1) * w]
            {
                live_out[b * w..(b + 1) * w].copy_from_slice(&new_out);
                live_in[b * w..(b + 1) * w].copy_from_slice(&new_in);
                changed = true;
            }
        }
    }

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mark = |adj: &mut Vec<Vec<usize>>, live: &[u64]| {
        let mut members = Vec::new();
        for i in 0..n {
            if bit_get(live, i) {
                members.push(i);
            }
        }
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let a = members[i];
                let b = members[j];
                if class[a] != class[b] {
                    continue;
                }
                if let Err(pos) = adj[a].binary_search(&b) {
                    adj[a].insert(pos, b);
                }
                if let Err(pos) = adj[b].binary_search(&a) {
                    adj[b].insert(pos, a);
                }
            }
        }
    };

    for b in 0..nb {
        let block = Block::new(b);
        let mut live = live_in[b * w..(b + 1) * w].to_vec();
        for &v in func.block_params(block) {
            let i = v.vreg();
            if i < n {
                bit_set(&mut live, i);
            }
        }
        mark(&mut adj, &live);
        for inst in func.block_insns(block).iter() {
            for op in func.inst_operands(inst) {
                if op.as_fixed_nonallocatable().is_some() {
                    continue;
                }
                let i = op.vreg().vreg();
                if i < n && op.kind() == OperandKind::Use {
                    bit_set(&mut live, i);
                }
            }
            mark(&mut adj, &live);
            for op in func.inst_operands(inst) {
                if op.as_fixed_nonallocatable().is_some() {
                    continue;
                }
                if op.kind() == OperandKind::Def {
                    let i = op.vreg().vreg();
                    if i < n {
                        bit_set(&mut live, i);
                    }
                }
            }
            mark(&mut adj, &live);
        }
        let term = func.block_insns(block).last();
        for si in 0..func.block_succs(block).len() {
            for &v in func.branch_blockparams(block, term, si) {
                let i = v.vreg();
                if i < n {
                    bit_set(&mut live, i);
                }
            }
        }
        mark(&mut adj, &live);
    }

    IGraph { n, adj, class }
}

/// Induced subgraph of one register class → remapped 0..m indices.
fn class_nodes(g: &IGraph, class: RegClass) -> (Vec<usize>, Vec<Vec<usize>>) {
    // Isolated vertices do not raise chromatic number above 1; drop them.
    let nodes: Vec<usize> = (0..g.n)
        .filter(|&i| g.class[i] == class && !g.adj[i].is_empty())
        .collect();
    let m = nodes.len();
    let mut map = vec![usize::MAX; g.n.max(1)];
    for (j, &i) in nodes.iter().enumerate() {
        map[i] = j;
    }
    let mut adj = vec![Vec::new(); m];
    for (j, &i) in nodes.iter().enumerate() {
        for &k in &g.adj[i] {
            if k < map.len() && map[k] != usize::MAX && g.class[k] == class {
                adj[j].push(map[k]);
            }
        }
        adj[j].sort_unstable();
        adj[j].dedup();
    }
    (nodes, adj)
}

fn is_adj(adj: &[Vec<usize>], a: usize, b: usize) -> bool {
    adj[a].binary_search(&b).is_ok()
}

/// Greedy clique from each vertex (fast lower bound).
fn greedy_clique(adj: &[Vec<usize>]) -> Vec<usize> {
    let m = adj.len();
    let mut best = Vec::new();
    for start in 0..m {
        let mut clique = vec![start];
        let mut cand: Vec<usize> = adj[start].clone();
        while !cand.is_empty() {
            // pick highest degree into remaining cand
            let mut best_i = 0;
            let mut best_d = 0;
            for (i, &v) in cand.iter().enumerate() {
                let d = cand.iter().filter(|&&u| is_adj(adj, v, u)).count();
                if d >= best_d {
                    best_d = d;
                    best_i = i;
                }
            }
            let v = cand.swap_remove(best_i);
            clique.push(v);
            cand.retain(|&u| is_adj(adj, v, u));
        }
        if clique.len() > best.len() {
            best = clique;
        }
    }
    best
}

/// Exact max clique via Bron–Kerbosch with pivot (small m).
fn exact_clique(adj: &[Vec<usize>], budget: &mut usize) -> Vec<usize> {
    let m = adj.len();
    let mut best = greedy_clique(adj);
    fn bk(
        adj: &[Vec<usize>],
        r: &mut Vec<usize>,
        p: &mut Vec<usize>,
        x: &mut Vec<usize>,
        best: &mut Vec<usize>,
        budget: &mut usize,
    ) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        if p.is_empty() && x.is_empty() {
            if r.len() > best.len() {
                *best = r.clone();
            }
            return;
        }
        if r.len() + p.len() <= best.len() {
            return;
        }
        // pivot: vertex in P∪X of max degree into P
        let mut pivot = None;
        let mut pivot_d = 0;
        for &u in p.iter().chain(x.iter()) {
            let d = p.iter().filter(|&&v| is_adj(adj, u, v)).count();
            if d >= pivot_d {
                pivot_d = d;
                pivot = Some(u);
            }
        }
        let mut p_copy = p.clone();
        if let Some(u) = pivot {
            p_copy.retain(|&v| !is_adj(adj, u, v));
        }
        while let Some(v) = p_copy.pop() {
            r.push(v);
            let mut p2: Vec<usize> = p.iter().copied().filter(|&u| is_adj(adj, v, u)).collect();
            let mut x2: Vec<usize> = x.iter().copied().filter(|&u| is_adj(adj, v, u)).collect();
            bk(adj, r, &mut p2, &mut x2, best, budget);
            r.pop();
            if let Some(i) = p.iter().position(|&u| u == v) {
                p.swap_remove(i);
            }
            x.push(v);
            if *budget == 0 {
                return;
            }
        }
    }
    let mut r = Vec::new();
    let mut p: Vec<usize> = (0..m).collect();
    let mut x = Vec::new();
    bk(adj, &mut r, &mut p, &mut x, &mut best, budget);
    best
}

/// Exact K-colourability via backtracking (MRV). `k` must be ≤ 64.
fn colourable(adj: &[Vec<usize>], k: u32, budget: &mut usize) -> Option<Vec<u32>> {
    let m = adj.len();
    if m == 0 {
        return Some(Vec::new());
    }
    if k == 0 {
        return None;
    }
    if k > 64 {
        return None;
    }
    let mut colour = vec![u32::MAX; m];
    let mut remaining: Vec<usize> = (0..m).collect();
    remaining.sort_by(|&a, &b| adj[b].len().cmp(&adj[a].len()));

    fn rec(
        adj: &[Vec<usize>],
        remaining: &mut Vec<usize>,
        k: u32,
        colour: &mut [u32],
        budget: &mut usize,
    ) -> bool {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if remaining.is_empty() {
            return true;
        }
        let mut best_i = 0;
        let mut best_opts = (k as usize) + 1;
        let mut best_mask = 0u64;
        for (i, &v) in remaining.iter().enumerate() {
            let mut used = 0u64;
            for &u in &adj[v] {
                let c = colour[u];
                if c != u32::MAX && c < 64 {
                    used |= 1u64 << c;
                }
            }
            let opts = (0..k).filter(|&c| used & (1u64 << c) == 0).count();
            if opts < best_opts {
                best_opts = opts;
                best_i = i;
                best_mask = used;
            }
        }
        if best_opts == 0 {
            return false;
        }
        let v = remaining.swap_remove(best_i);
        for c in 0..k {
            if best_mask & (1u64 << c) != 0 {
                continue;
            }
            colour[v] = c;
            if rec(adj, remaining, k, colour, budget) {
                return true;
            }
            colour[v] = u32::MAX;
            if *budget == 0 {
                remaining.push(v);
                return false;
            }
        }
        remaining.push(v);
        false
    }

    if rec(adj, &mut remaining, k, &mut colour, budget) {
        Some(colour)
    } else {
        None
    }
}

/// Prove chromatic bounds for one register class.
pub fn prove_class(g: &IGraph, class: RegClass) -> Bound {
    let (nodes, adj) = class_nodes(g, class);
    let n_class = nodes.len();
    if n_class == 0 {
        return Bound {
            lower: CliqueCert { nodes: Vec::new() },
            lower_k: 0,
            upper: Some(ColourCert {
                colour: vec![u32::MAX; g.n],
                k: 0,
            }),
            upper_k: Some(0),
            exact: true,
            n_class: 0,
        };
    }

    let exact = n_class <= MAX_EXACT;
    let mut budget = if exact { 2_000_000usize } else { 50_000 };
    let clique_local = if exact {
        exact_clique(&adj, &mut budget)
    } else {
        greedy_clique(&adj)
    };
    let lower_k = clique_local.len() as u32;
    let lower = CliqueCert {
        nodes: clique_local.iter().map(|&j| nodes[j]).collect(),
    };

    let mut upper = None;
    let mut upper_k = None;
    if exact {
        // Binary search chromatic number in [lower_k, Δ+1].
        let max_deg = adj.iter().map(|a| a.len()).max().unwrap_or(0) as u32;
        let mut lo = lower_k;
        let mut hi = (max_deg + 1).max(lo);
        let mut best_colour = None;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mut b = 2_000_000usize;
            match colourable(&adj, mid, &mut b) {
                Some(col) => {
                    hi = mid;
                    best_colour = Some(col);
                }
                None => {
                    if b == 0 && mid > lo {
                        // timeout — accept hi as soft upper via greedy
                        break;
                    }
                    lo = mid + 1;
                }
            }
        }
        if best_colour.is_none() {
            let mut b = 2_000_000usize;
            best_colour = colourable(&adj, hi, &mut b);
        }
        if let Some(col) = best_colour {
            let k = col.iter().copied().filter(|&c| c != u32::MAX).max().map(|c| c + 1).unwrap_or(0).max(lo);
            let mut full = vec![u32::MAX; g.n];
            for (j, &i) in nodes.iter().enumerate() {
                full[i] = col[j];
            }
            upper_k = Some(k);
            upper = Some(ColourCert { colour: full, k });
        }
    }

    Bound {
        lower,
        lower_k,
        upper,
        upper_k,
        exact,
        n_class,
    }
}

/// Count distinct physical registers used in an allocation output (per class).
pub fn preg_pressure(out: &Output, class: RegClass) -> usize {
    let mut seen = Vec::new();
    for a in &out.allocs {
        if let Some(r) = a.as_reg() {
            if r.class() == class && is_alloc_looking(r) {
                if !seen.iter().any(|x| *x == r) {
                    seen.push(r);
                }
            }
        }
    }
    // Edits can introduce temps too.
    for (_, e) in &out.edits {
        let Edit::Move { from, to } = e;
        for a in [*from, *to] {
            if let Some(r) = a.as_reg() {
                if r.class() == class && is_alloc_looking(r) {
                    if !seen.iter().any(|x| *x == r) {
                        seen.push(r);
                    }
                }
            }
        }
    }
    let _ = Allocation::none();
    seen.len()
}

fn is_alloc_looking(r: PReg) -> bool {
    // Non-allocatable ABI pins sit at high encodings; count only typical
    // allocatable hw indices. Conservative: count everything — the
    // comparison is still informative.
    let _ = r;
    true
}

/// Compare a heuristic's distinct-preg footprint against the certificates.
///
/// `preg_pressure` counts distinct physical registers touched anywhere in
/// the allocation (including spill/reload edits). That is a *footprint*,
/// not simultaneous pressure: spilling can drop the footprint below the
/// clique lower bound (expected — those live values share spill slots),
/// and a colouring-optimal no-spill assignment uses exactly `upper_k`
/// registers. `heuristic_above_optimum` means the allocator sprinkled
/// values across more registers than a chromatic colouring needs; that
/// can still be right for move cost / callee-saves, but it is the gap a
/// proof-guided search would try to close.
pub fn verdict(bound: &Bound, heuristic_pregs: usize) -> &'static str {
    match (
        bound.lower_k as usize,
        bound.upper_k.map(|k| k as usize),
        heuristic_pregs,
    ) {
        (lo, Some(hi), r) if lo == hi && r > hi => "heuristic_above_optimum",
        (lo, Some(hi), r) if lo == hi && r == hi => "heuristic_matches_optimum",
        (lo, Some(hi), r) if lo == hi && r < hi => "heuristic_spills_under_chromatic",
        (lo, _, r) if r < lo => "heuristic_spills_under_clique",
        (lo, None, r) if r == lo => "matches_clique_lower",
        (lo, None, r) if r > lo => "gap_above_clique_unresolved",
        _ => "inconclusive",
    }
}

/// Independent check: every pair in `nodes` is adjacent (a true clique).
pub fn check_clique(g: &IGraph, nodes: &[usize]) -> Result<(), &'static str> {
    for (i, &a) in nodes.iter().enumerate() {
        if a >= g.n {
            return Err("clique node out of range");
        }
        for &b in &nodes[i + 1..] {
            if g.adj[a].binary_search(&b).is_err() {
                return Err("clique pair not adjacent");
            }
            if g.class[a] != g.class[b] {
                return Err("clique spans register classes");
            }
        }
    }
    Ok(())
}

/// Independent check: `colour[v] < k` for coloured verts, adjacent verts differ.
pub fn check_colouring(g: &IGraph, colour: &[u32], k: u32) -> Result<(), &'static str> {
    if colour.len() < g.n {
        return Err("colour vector short");
    }
    for v in 0..g.n {
        let c = colour[v];
        if c == u32::MAX {
            continue;
        }
        if c >= k {
            return Err("colour ≥ k");
        }
        for &u in &g.adj[v] {
            if u < v {
                continue;
            }
            let d = colour[u];
            if d != u32::MAX && d == c {
                return Err("adjacent vertices share a colour");
            }
        }
    }
    Ok(())
}

/// Re-verify a `Bound` produced by `prove_class` (instrumentation / learning).
pub fn check_bound(g: &IGraph, bound: &Bound) -> Result<(), &'static str> {
    check_clique(g, &bound.lower.nodes)?;
    if bound.lower.nodes.len() as u32 != bound.lower_k {
        return Err("lower_k ≠ clique len");
    }
    if let (Some(cert), Some(k)) = (&bound.upper, bound.upper_k) {
        if cert.k != k {
            return Err("ColourCert.k ≠ upper_k");
        }
        check_colouring(g, &cert.colour, k)?;
        if k < bound.lower_k {
            return Err("upper_k < lower_k");
        }
    }
    Ok(())
}

/// A colouring that uses strictly fewer registers than the heuristic footprint.
#[derive(Clone, Debug)]
pub struct FewerCert {
    /// Target K the search coloured with (`K < heuristic_pregs`).
    pub k: u32,
    pub colouring: ColourCert,
    /// True when we also failed to colour with `k - 1` (or `k == lower_k`
    /// via the clique), so K is tight on this abstraction.
    pub tight: bool,
}

/// "Prove you need ≥ heuristic, or hand me a counterexample with one fewer."
///
/// Walks `heuristic_pregs - 1` down toward the clique lower bound looking for
/// a valid colouring. A hit is the constructive proof the heuristic left
/// register footprint on the table; spending more `budget` is the
/// trade-compute-for-quality dial.
pub fn try_fewer(
    g: &IGraph,
    class: RegClass,
    heuristic_pregs: usize,
    budget: &mut usize,
) -> Option<FewerCert> {
    if heuristic_pregs == 0 {
        return None;
    }
    let (nodes, adj) = class_nodes(g, class);
    if nodes.is_empty() || nodes.len() > MAX_EXACT {
        return None;
    }
    let mut b = *budget;
    let clique = exact_clique(&adj, &mut b);
    let lower_k = clique.len() as u32;
    *budget = b;

    let start = (heuristic_pregs as u32).saturating_sub(1);
    if start < lower_k {
        return None;
    }
    // Ascend from the clique: first hit is the strongest counterexample
    // (fewest registers that still colour).
    for k in lower_k..=start {
        let mut b = *budget;
        if let Some(col) = colourable(&adj, k, &mut b) {
            *budget = b;
            let mut full = vec![u32::MAX; g.n];
            for (j, &i) in nodes.iter().enumerate() {
                full[i] = col[j];
            }
            // Clique of size k, or failed colourability at k-1 with budget
            // left, means K is tight on this abstraction.
            let tight = k == lower_k;
            return Some(FewerCert {
                k,
                colouring: ColourCert { colour: full, k },
                tight,
            });
        }
        *budget = b;
        if *budget == 0 {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangle_needs_three() {
        let g = IGraph {
            n: 3,
            adj: vec![vec![1, 2], vec![0, 2], vec![0, 1]],
            class: vec![RegClass::Int; 3],
        };
        let b = prove_class(&g, RegClass::Int);
        assert_eq!(b.lower_k, 3);
        assert_eq!(b.upper_k, Some(3));
        assert_eq!(verdict(&b, 3), "heuristic_matches_optimum");
        assert_eq!(verdict(&b, 4), "heuristic_above_optimum");
        check_bound(&g, &b).unwrap();
    }

    #[test]
    fn empty_is_zero() {
        let g = IGraph {
            n: 0,
            adj: Vec::new(),
            class: Vec::new(),
        };
        let b = prove_class(&g, RegClass::Int);
        assert_eq!(b.lower_k, 0);
        assert_eq!(b.upper_k, Some(0));
    }

    #[test]
    fn try_fewer_beats_fat_heuristic() {
        // Path of 3: chromatic 2. Heuristic claiming 4 leaves footprint.
        let g = IGraph {
            n: 3,
            adj: vec![vec![1], vec![0, 2], vec![1]],
            class: vec![RegClass::Int; 3],
        };
        let mut budget = 1_000_000;
        let f = try_fewer(&g, RegClass::Int, 4, &mut budget).expect("fewer");
        assert!(f.k <= 2);
        check_colouring(&g, &f.colouring.colour, f.k).unwrap();
        assert!(f.tight);
    }

    #[test]
    fn bad_clique_rejected() {
        let g = IGraph {
            n: 3,
            adj: vec![vec![1], vec![0, 2], vec![1]],
            class: vec![RegClass::Int; 3],
        };
        assert!(check_clique(&g, &[0, 2]).is_err());
    }
}

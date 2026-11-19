//! PLIRON: Gumbel-flavoured UCT over register homes.
//!
//! Inspired by `/Users/wow/code/tourn/training/burn` (Gumbel-MCTS) and
//! `hextoe/mcts-rs` (arena UCT) — stripped to a no-`rand`/no-net core that
//! fits regalloc2's `no_std`+alloc world. Ideas taken from the tournament
//! search (see `docs/history/2026-11-18-ra-mcts.md`):
//!
//! - **Prior shaping**: prefer pregs already used in the partial assign
//!   (lower footprint), deprioritize fresh pregs, mild penalty on spill;
//!   FixedReg pin boost + clobber/foreign-fixed forbid lists from greedy.
//! - **Root Gumbel**: sample Gumbel(0,1) once per root action (LCG); root
//!   selection / expansion ranks by `g + ln(prior)`. Below root: PUCT
//!   with the same prior (`Q + c·P·√N/(1+n)`).
//! - **Branching cap**: only the top-`M` free pregs by prior, plus spill
//!   (`PLIRON_RA_MCTS_TOPM`, default 8) — tourn's `max_considered`.
//!
//! - State = partial assignment of eligible vregs (single-block non-param,
//!   same gate as `GREEDY_COLOR`).
//! - Action = place the next vreg in a free non-interfering preg, or spill.
//! - Rollout policy = first-fit (lowest index free preg), else spill.
//! - Reward = `−(preg_footprint + SPILL_W · spills)` — the simple, total
//!   register-count measure. Higher (less negative) is better.
//!
//! Compute dial: `PLIRON_RA_MCTS_SIMS` (default 64). Opt-in via
//! `PLIRON_RA_ALGO=mcts` (implies colouring) or `PLIRON_RA_MCTS=1` under
//! greedy+COLOR. `PLIRON_RA_MCTS_GUMBEL=0` turns off root Gumbel.

use crate::{Allocation, MachineEnv, PReg, RegClass, SpillSlot};
use alloc::vec;
use alloc::vec::Vec;

const SPILL_W: i32 = 8;
const NONE: u8 = 0xff;
const SPILL: u8 = 0xfe;

/// Logit bonuses for the hand-shaped prior (before softmax).
const PRIOR_USED: f64 = 2.0;
const PRIOR_NEW: f64 = 0.0;
const PRIOR_SPILL: f64 = -1.5;
const PRIOR_PIN: f64 = 3.0;

/// Search knobs. `from_env` is what the allocator path uses; tests pass
/// an explicit struct so they do not depend on process environment.
#[derive(Clone, Debug)]
pub struct Params {
    pub sims: usize,
    /// Cap free-preg candidates considered at each expand (plus spill).
    pub top_m: usize,
    pub gumbel_root: bool,
    pub cpuct: f64,
    /// Rollout work budget (about `m * (degree + registers)` per rollout)
    /// that caps `sims` on big functions (`PLIRON_RA_MCTS_WORK`).
    pub work: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            sims: 64,
            top_m: 8,
            gumbel_root: true,
            cpuct: 1.5,
            work: 4_000_000,
        }
    }
}

impl Params {
    pub fn from_env() -> Self {
        let mut p = Params::default();
        p.sims = env_usize("PLIRON_RA_MCTS_SIMS", p.sims).max(1);
        p.top_m = env_usize("PLIRON_RA_MCTS_TOPM", p.top_m).max(1);
        p.gumbel_root = env_flag_default_on("PLIRON_RA_MCTS_GUMBEL");
        p.cpuct = env_f64("PLIRON_RA_MCTS_CPUCT", p.cpuct);
        p.work = env_usize("PLIRON_RA_MCTS_WORK", p.work).max(1);
        p
    }
}

#[derive(Clone)]
struct Node {
    /// Partial assign for eligible slots: NONE / SPILL / preg-index in `regs`.
    partial: Vec<u8>,
    /// Next eligible index to decide (0..eligible.len()).
    next: usize,
    parent: u32,
    action: u8, // action that produced this node from parent
    /// Gumbel noise sampled once when this child was expanded from the root.
    gumbel: f64,
    /// Prior mass of `action` under the parent's shaped distribution.
    prior: f64,
    children: Vec<u32>,
    n: u32,
    w: f64, // sum of rewards
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / ((1u64 << 53) as f64)
    }
    fn gen_range(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() as usize) % n
        }
    }
}

/// Inverse-CDF Gumbel(0,1): −ln(−ln(U)).
fn gumbel(rng: &mut Rng) -> f64 {
    let u = rng.f().clamp(1e-12, 1.0 - 1e-12);
    -(-u.ln()).ln()
}

fn env_usize(key: &str, default: usize) -> usize {
    #[cfg(feature = "std")]
    {
        std::env::var(key)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = key;
        default
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        std::env::var(key)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = key;
        default
    }
}

/// `=0` off; unset or any other value → on (knob lives inside an opt-in algo).
fn env_flag_default_on(key: &str) -> bool {
    #[cfg(feature = "std")]
    {
        match std::env::var(key).as_deref() {
            Ok("0") => false,
            _ => true,
        }
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = key;
        true
    }
}

fn env_mcts_on() -> bool {
    #[cfg(feature = "std")]
    {
        matches!(std::env::var("PLIRON_RA_ALGO").as_deref(), Ok("mcts"))
            || std::env::var("PLIRON_RA_MCTS").as_deref() == Ok("1")
    }
    #[cfg(not(feature = "std"))]
    {
        false
    }
}

/// Public probe used by greedy / algo dispatch.
pub fn enabled() -> bool {
    env_mcts_on()
}

fn regs_for(env: &MachineEnv, class: RegClass) -> Vec<PReg> {
    let idx = class as usize;
    let mut out = Vec::new();
    for r in env.preferred_regs_by_class[idx] {
        out.push(r);
    }
    for r in env.non_preferred_regs_by_class[idx] {
        if !out.iter().any(|x| *x == r) {
            out.push(r);
        }
    }
    out
}

fn ranges_overlap(a0: usize, a1: usize, b0: usize, b1: usize) -> bool {
    a0 < b1 && b0 < a1
}

fn conflicts(
    eligible: &[usize],
    interfere: &[Vec<usize>],
    first: &[usize],
    last: &[usize],
    forbid: &[Vec<u8>],
    partial: &[u8],
    slot: usize,
    preg_i: u8,
) -> bool {
    if let Some(fb) = forbid.get(slot) {
        if fb.iter().any(|&x| x == preg_i) {
            return true;
        }
    }
    let v = eligible[slot];
    if let Some(nbrs) = interfere.get(v) {
        for &u in nbrs {
            if let Some(j) = eligible.iter().position(|&x| x == u) {
                if partial[j] == preg_i {
                    return true;
                }
            }
        }
    }
    // Live-range belt-and-suspenders when the adjacency list is incomplete.
    if slot < first.len() && slot < last.len() {
        let (f0, l0) = (first[slot], last[slot]);
        for (j, &a) in partial.iter().enumerate() {
            if j == slot || a != preg_i {
                continue;
            }
            if j < first.len()
                && j < last.len()
                && ranges_overlap(f0, l0, first[j], last[j])
            {
                return true;
            }
        }
    }
    false
}

fn used_mask(partial: &[u8]) -> [bool; 256] {
    let mut used = [false; 256];
    for &a in partial {
        if a != NONE && a != SPILL {
            used[a as usize] = true;
        }
    }
    used
}

/// Hand prior logit for one legal action given the partial's used set and
/// an optional FixedReg pin for the vreg about to be coloured.
fn action_logit(a: u8, used: &[bool; 256], pin: Option<u8>) -> f64 {
    if a == SPILL {
        return PRIOR_SPILL;
    }
    let mut logit = if used[a as usize] {
        PRIOR_USED
    } else {
        PRIOR_NEW
    };
    if pin == Some(a) {
        logit += PRIOR_PIN;
    }
    logit
}

fn softmax_priors(logits: &[f64]) -> Vec<f64> {
    let mx = logits.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let mut exps: Vec<f64> = logits.iter().map(|l| (l - mx).exp()).collect();
    let z: f64 = exps.iter().sum::<f64>().max(1e-12);
    for e in &mut exps {
        *e /= z;
    }
    exps
}

/// Legal actions ranked by prior, capped to `top_m` free pregs + spill.
/// Returns (action, prior_mass) pairs in descending prior order; spill is
/// always retained even when it would fall outside the top-M free set.
fn considered_actions(
    eligible: &[usize],
    interfere: &[Vec<usize>],
    first: &[usize],
    last: &[usize],
    forbid: &[Vec<u8>],
    partial: &[u8],
    next: usize,
    n_regs: usize,
    top_m: usize,
    pin: Option<u8>,
) -> Vec<(u8, f64)> {
    if next >= eligible.len() {
        return Vec::new();
    }
    let used = used_mask(partial);
    let mut free: Vec<(u8, f64)> = Vec::new();
    for i in 0..n_regs.min(254) {
        let a = i as u8;
        if !conflicts(eligible, interfere, first, last, forbid, partial, next, a) {
            free.push((a, action_logit(a, &used, pin)));
        }
    }
    // Prefer used pregs (higher logit), then lower index for stability.
    free.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    free.truncate(top_m);

    let mut acts: Vec<(u8, f64)> = free;
    acts.push((SPILL, action_logit(SPILL, &used, pin)));
    let logits: Vec<f64> = acts.iter().map(|(_, l)| *l).collect();
    let priors = softmax_priors(&logits);
    for (i, p) in priors.into_iter().enumerate() {
        acts[i].1 = p;
    }
    // Re-sort by prior mass so expansion walks hottest first.
    acts.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal));
    acts
}

fn reward(partial: &[u8]) -> f64 {
    let mut seen = [false; 256];
    let mut footprint = 0i32;
    let mut spills = 0i32;
    for &a in partial {
        if a == SPILL {
            spills += 1;
        } else if a != NONE {
            let i = a as usize;
            if i < seen.len() && !seen[i] {
                seen[i] = true;
                footprint += 1;
            }
        }
    }
    -((footprint + SPILL_W * spills) as f64)
}

fn first_fit_action(
    eligible: &[usize],
    interfere: &[Vec<usize>],
    first: &[usize],
    last: &[usize],
    forbid: &[Vec<u8>],
    partial: &[u8],
    next: usize,
    n_regs: usize,
    top_m: usize,
    pin: Option<u8>,
) -> u8 {
    let acts = considered_actions(
        eligible, interfere, first, last, forbid, partial, next, n_regs, top_m, pin,
    );
    acts.iter()
        .map(|(a, _)| *a)
        .find(|&x| x != SPILL)
        .unwrap_or(SPILL)
}

fn rollout(
    eligible: &[usize],
    interfere: &[Vec<usize>],
    first: &[usize],
    last: &[usize],
    forbid: &[Vec<u8>],
    mut partial: Vec<u8>,
    mut next: usize,
    n_regs: usize,
    top_m: usize,
    pins: &[Option<u8>],
    rng: &mut Rng,
) -> f64 {
    while next < eligible.len() {
        let pin = pins.get(next).copied().flatten();
        let acts = considered_actions(
            eligible, interfere, first, last, forbid, &partial, next, n_regs, top_m, pin,
        );
        // First-fit bias with occasional random (ε=0.15) over considered set.
        let a = if rng.f() < 0.15 {
            acts[rng.gen_range(acts.len())].0
        } else {
            acts.iter()
                .map(|(a, _)| *a)
                .find(|&x| x != SPILL)
                .unwrap_or(SPILL)
        };
        partial[next] = a;
        next += 1;
    }
    reward(&partial)
}

/// Linear first-fit completion (no search) — baseline for the triangle test.
pub fn first_fit_partial(
    eligible: &[usize],
    interfere: &[Vec<usize>],
    n_regs: usize,
    top_m: usize,
) -> Vec<u8> {
    let m = eligible.len();
    let first = vec![0usize; m];
    let last = vec![1usize; m];
    let forbid: Vec<Vec<u8>> = vec![Vec::new(); m];
    let mut partial = vec![NONE; m];
    for next in 0..m {
        partial[next] = first_fit_action(
            eligible, interfere, &first, &last, &forbid, &partial, next, n_regs, top_m, None,
        );
    }
    partial
}

fn pin_at(pins: &[Option<u8>], next: usize) -> Option<u8> {
    pins.get(next).copied().flatten()
}

/// PUCT below the root; at the root, optionally fold in the stored Gumbel.
fn pick_child(nodes: &[Node], parent: u32, cpuct: f64, gumbel_root: bool, rng: &mut Rng) -> u32 {
    let p = &nodes[parent as usize];
    let is_root = p.parent == u32::MAX;
    let mut best = p.children[0];
    let mut best_score = f64::NEG_INFINITY;
    let pn = p.n.max(1) as f64;
    let sqrt_n = pn.sqrt();
    for &ch in &p.children {
        let n = &nodes[ch as usize];
        let q = if n.n == 0 {
            0.0
        } else {
            n.w / n.n as f64
        };
        let u = cpuct * n.prior * sqrt_n / (1.0 + n.n as f64);
        let mut sc = q + u;
        if n.n == 0 {
            // Force expand unvisited before deep exploitation; tiny jitter
            // breaks ties without drowning the prior.
            sc += 1e6;
            sc += rng.f() * 1e-6;
        }
        if is_root && gumbel_root {
            sc += n.gumbel + (n.prior.max(1e-12)).ln();
        }
        if sc > best_score {
            best_score = sc;
            best = ch;
        }
    }
    best
}

fn partial_preg_count(partial: &[u8]) -> usize {
    let mut seen = [false; 256];
    let mut n = 0usize;
    for &a in partial {
        if a != NONE && a != SPILL {
            let i = a as usize;
            if i < seen.len() && !seen[i] {
                seen[i] = true;
                n += 1;
            }
        }
    }
    n
}

/// Search homes for `eligible` vreg indices (into the global assign table).
/// `first`/`last` are parallel live ranges (inst indices); `fixed[i]` is an
/// optional preferred preg index (FixedReg pin); `forbid[i]` lists preg
/// indices clobbered or foreign-fixed across the live range.
pub fn search_class(
    eligible: &[usize],
    first: &[usize],
    last: &[usize],
    fixed: &[Option<u8>],
    forbid: &[Vec<u8>],
    interfere: &[Vec<usize>],
    env: &MachineEnv,
    class: RegClass,
    spill_base: usize,
    seed: u64,
) -> (Vec<Allocation>, usize) {
    search_class_with(
        eligible,
        first,
        last,
        fixed,
        forbid,
        interfere,
        env,
        class,
        spill_base,
        seed,
        &Params::from_env(),
    )
}

/// Like [`search_class`] with explicit [`Params`].
pub fn search_class_with(
    eligible: &[usize],
    first: &[usize],
    last: &[usize],
    fixed: &[Option<u8>],
    forbid: &[Vec<u8>],
    interfere: &[Vec<usize>],
    env: &MachineEnv,
    class: RegClass,
    spill_base: usize,
    seed: u64,
    params: &Params,
) -> (Vec<Allocation>, usize) {
    let regs = regs_for(env, class);
    let n_regs = regs.len();
    let m = eligible.len();
    if m == 0 {
        return (Vec::new(), spill_base);
    }

    let partial = search_partial(
        eligible, first, last, fixed, forbid, interfere, n_regs, seed, params,
    );
    allocs_from_partial(&partial, &regs, spill_base)
}

fn search_partial(
    eligible: &[usize],
    first: &[usize],
    last: &[usize],
    fixed: &[Option<u8>],
    forbid: &[Vec<u8>],
    interfere: &[Vec<usize>],
    n_regs: usize,
    seed: u64,
    params: &Params,
) -> Vec<u8> {
    let m = eligible.len();
    let root_partial = vec![NONE; m];
    let mut nodes = vec![Node {
        partial: root_partial,
        next: 0,
        parent: u32::MAX,
        action: NONE,
        gumbel: 0.0,
        prior: 1.0,
        children: Vec::new(),
        n: 0,
        w: 0.0,
    }];
    let mut rng = Rng(seed ^ 0x4d43_5453u64 ^ (m as u64).wrapping_mul(0x9e37));
    let top_m = params.top_m;
    let cpuct = params.cpuct;

    for sim in 0..params.sims {
        // Sequential halving at the root (tourn): after half the budget,
        // drop the worse half of root actions so the rest of the sims
        // concentrate on the promising homes.
        if sim == params.sims / 2 && nodes[0].children.len() > 2 {
            let mut kids = nodes[0].children.clone();
            kids.sort_by(|&a, &b| {
                let score = |id: u32| {
                    let n = &nodes[id as usize];
                    let q = if n.n == 0 { f64::NEG_INFINITY } else { n.w / n.n as f64 };
                    q + n.gumbel
                };
                score(b).partial_cmp(&score(a)).unwrap_or(core::cmp::Ordering::Equal)
            });
            kids.truncate((kids.len() + 1) / 2);
            nodes[0].children = kids;
        }
        // select
        let mut id = 0u32;
        loop {
            let node = &nodes[id as usize];
            if node.children.is_empty() || node.next >= m {
                break;
            }
            let pin = pin_at(fixed, node.next);
            let considered = considered_actions(
                eligible,
                interfere,
                first,
                last,
                forbid,
                &node.partial,
                node.next,
                n_regs,
                top_m,
                pin,
            );
            if node.children.len() < considered.len() {
                break; // still have an unexpanded considered action
            }
            id = pick_child(&nodes, id, cpuct, params.gumbel_root, &mut rng);
        }
        // expand one child if not terminal
        if nodes[id as usize].next < m {
            let pin = pin_at(fixed, nodes[id as usize].next);
            let acts = considered_actions(
                eligible,
                interfere,
                first,
                last,
                forbid,
                &nodes[id as usize].partial,
                nodes[id as usize].next,
                n_regs,
                top_m,
                pin,
            );
            let expanded: Vec<u8> = nodes[id as usize]
                .children
                .iter()
                .map(|&ch| nodes[ch as usize].action)
                .collect();
            if let Some(&(a, prior)) = acts.iter().find(|(a, _)| !expanded.iter().any(|e| e == a)) {
                let is_root = nodes[id as usize].parent == u32::MAX;
                let g = if is_root && params.gumbel_root {
                    gumbel(&mut rng)
                } else {
                    0.0
                };
                let mut partial = nodes[id as usize].partial.clone();
                let next = nodes[id as usize].next;
                partial[next] = a;
                let child = Node {
                    partial,
                    next: next + 1,
                    parent: id,
                    action: a,
                    gumbel: g,
                    prior,
                    children: Vec::new(),
                    n: 0,
                    w: 0.0,
                };
                let cid = nodes.len() as u32;
                nodes.push(child);
                nodes[id as usize].children.push(cid);
                id = cid;
            }
        }
        // simulate
        let r = rollout(
            eligible,
            interfere,
            first,
            last,
            forbid,
            nodes[id as usize].partial.clone(),
            nodes[id as usize].next,
            n_regs,
            top_m,
            fixed,
            &mut rng,
        );
        // backprop
        let mut cur = id;
        loop {
            let n = &mut nodes[cur as usize];
            n.n += 1;
            n.w += r;
            if n.parent == u32::MAX {
                break;
            }
            cur = n.parent;
        }
    }

    // Extract best complete assignment: follow most-visited child, then
    // finish with first-fit rollout.
    let mut id = 0u32;
    while !nodes[id as usize].children.is_empty() && nodes[id as usize].next < m {
        let best = *nodes[id as usize]
            .children
            .iter()
            .max_by_key(|&&ch| nodes[ch as usize].n)
            .unwrap();
        id = best;
    }
    let mut partial = nodes[id as usize].partial.clone();
    let mut next = nodes[id as usize].next;
    while next < m {
        let pin = pin_at(fixed, next);
        partial[next] = first_fit_action(
            eligible, interfere, first, last, forbid, &partial, next, n_regs, top_m, pin,
        );
        next += 1;
    }
    partial
}

fn allocs_from_partial(
    partial: &[u8],
    regs: &[PReg],
    spill_base: usize,
) -> (Vec<Allocation>, usize) {
    let mut out = Vec::with_capacity(partial.len());
    let mut spillslots = spill_base;
    for &a in partial {
        if a == SPILL || a == NONE {
            out.push(Allocation::stack(SpillSlot::new(spillslots)));
            spillslots += 1;
        } else {
            let i = a as usize;
            if i < regs.len() {
                out.push(Allocation::reg(regs[i]));
            } else {
                out.push(Allocation::stack(SpillSlot::new(spillslots)));
                spillslots += 1;
            }
        }
    }
    (out, spillslots)
}

// ---------------------------------------------------------------------------
// Costed problem (`PLIRON_RA_ALGO=mcts`, whole-function homes).
//
// The footprint measure above is what the proof engine certifies, but
// minimising distinct registers *causes* moves and shuffles (half of the
// instructions of a 2026-11-18 run were `mov`). This search minimises what
// the code pays for instead, as an additive cost over the decisions:
//
// - a spilled vreg costs its operand count weighted by loop depth,
// - a vreg with `FixedReg(p)` operands costs one move per such operand not
//   already in `p`,
// - copy-like pairs (block-param edges, `Reuse`, `mov`) cost a move when
//   their homes differ,
// - the first use of a callee-saved register costs a save and a restore.
//
// Additive costs make every decision's delta exact when it is made, so a
// rollout is a greedy pass and the tree only has to find better first
// choices. The best complete assignment ever seen is the answer, which can
// never be worse than the first (greedy) rollout.
// ---------------------------------------------------------------------------

/// One class's costed assignment problem. Indices are positions in the
/// eligible list (already ordered hottest first).
pub struct Problem {
    pub n_regs: usize,
    /// Interference, in eligible indices.
    pub adj: Vec<Vec<u32>>,
    /// Registers (bit per index into the class's register list) a vreg
    /// cannot take: clobbers and foreign `FixedReg`s where it is live.
    pub forbid: Vec<u64>,
    pub spill_cost: Vec<f64>,
    pub pins: Vec<Vec<(u8, f64)>>,
    /// Symmetric copy affinities: (partner, weight).
    pub aff: Vec<Vec<(u32, f64)>>,
    pub callee_saved: u64,
    pub cs_cost: f64,
}

impl Problem {
    /// Briggs/Chaitin colouring order: repeatedly remove a node of fewer
    /// than `n_regs` remaining neighbours (it is always colourable, so it is
    /// coloured last), else the cheapest-per-neighbour node as an optimistic
    /// spill candidate; the colouring order is the reverse of removal. Long
    /// lived, high-degree values are coloured first and the temporaries that
    /// can use any free register go last, which cost-descending order gets
    /// backwards (the temporaries scatter over every colour first).
    /// Returns `None` when the graph is too big for the quadratic spill scan.
    pub fn simplify_order(&self) -> Option<Vec<usize>> {
        let m = self.len();
        if m > 4096 {
            return None;
        }
        let k = self.n_regs;
        let mut deg: Vec<usize> = self.adj.iter().map(|a| a.len()).collect();
        let mut removed = vec![false; m];
        let mut queued = vec![false; m];
        let mut work: Vec<usize> = Vec::new();
        for i in 0..m {
            if deg[i] < k {
                queued[i] = true;
                work.push(i);
            }
        }
        let mut stack: Vec<usize> = Vec::with_capacity(m);
        while stack.len() < m {
            let i = if let Some(i) = work.pop() {
                i
            } else {
                // Optimistic spill candidate: least cost per neighbour.
                let mut best = usize::MAX;
                let mut bs = f64::INFINITY;
                for j in 0..m {
                    if !removed[j] {
                        let sc = self.spill_cost[j] / (deg[j] as f64 + 1.0);
                        if sc < bs {
                            bs = sc;
                            best = j;
                        }
                    }
                }
                best
            };
            if removed[i] {
                continue;
            }
            removed[i] = true;
            stack.push(i);
            for &j in &self.adj[i] {
                let j = j as usize;
                if !removed[j] {
                    deg[j] -= 1;
                    if deg[j] < k && !queued[j] {
                        queued[j] = true;
                        work.push(j);
                    }
                }
            }
        }
        stack.reverse();
        Some(stack)
    }

    /// The same problem with node `new` being old node `order[new]`.
    pub fn permuted(&self, order: &[usize]) -> Problem {
        let m = self.len();
        let mut inv = vec![0u32; m];
        for (n, &o) in order.iter().enumerate() {
            inv[o] = n as u32;
        }
        Problem {
            n_regs: self.n_regs,
            adj: order.iter().map(|&o| self.adj[o].iter().map(|&j| inv[j as usize]).collect()).collect(),
            forbid: order.iter().map(|&o| self.forbid[o]).collect(),
            spill_cost: order.iter().map(|&o| self.spill_cost[o]).collect(),
            pins: order.iter().map(|&o| self.pins[o].clone()).collect(),
            aff: order
                .iter()
                .map(|&o| self.aff[o].iter().map(|&(j, w)| (inv[j as usize], w)).collect())
                .collect(),
            callee_saved: self.callee_saved,
            cs_cost: self.cs_cost,
        }
    }

    fn len(&self) -> usize {
        self.adj.len()
    }

    fn legal(&self, i: usize, assign: &[u8]) -> u64 {
        let full = if self.n_regs >= 64 { !0u64 } else { (1u64 << self.n_regs) - 1 };
        let mut m = full & !self.forbid[i];
        for &j in &self.adj[i] {
            let a = assign[j as usize];
            if a < SPILL {
                m &= !(1u64 << a);
            }
        }
        m
    }

    /// Cost of giving `i` register `a` (or `SPILL`) given what is assigned.
    fn delta(&self, i: usize, a: u8, assign: &[u8], used: u64) -> f64 {
        if a == SPILL {
            return self.spill_cost[i];
        }
        let mut c = 0.0;
        for &(p, w) in &self.pins[i] {
            if p != a {
                c += w;
            }
        }
        for &(j, w) in &self.aff[i] {
            let o = assign[j as usize];
            if o != NONE && o != a {
                c += w;
            }
        }
        if self.callee_saved & (1u64 << a) != 0 && used & (1u64 << a) == 0 {
            c += self.cs_cost;
        }
        c
    }

    /// The cheapest action for `i` (what `actions(..)[0]` is), without
    /// building the list: ties go to the lower register, a spill only when
    /// strictly cheaper.
    fn best(&self, i: usize, assign: &[u8], used: u64) -> (u8, f64) {
        let mut m = self.legal(i, assign);
        let mut pick = (SPILL, self.spill_cost[i]);
        let mut best_reg: Option<(u8, f64)> = None;
        while m != 0 {
            let a = m.trailing_zeros() as u8;
            m &= m - 1;
            let d = self.delta(i, a, assign, used);
            if best_reg.is_none_or(|(_, bd)| d < bd) {
                best_reg = Some((a, d));
            }
        }
        if let Some((a, d)) = best_reg {
            if d <= pick.1 {
                pick = (a, d);
            }
        }
        pick
    }

    /// Candidate actions for `i` with their deltas, cheapest first, at most
    /// `top_m` registers plus `SPILL`.
    fn actions(&self, i: usize, assign: &[u8], used: u64, top_m: usize) -> Vec<(u8, f64)> {
        let mut m = self.legal(i, assign);
        let mut v: Vec<(u8, f64)> = Vec::new();
        while m != 0 {
            let a = m.trailing_zeros() as u8;
            m &= m - 1;
            v.push((a, self.delta(i, a, assign, used)));
        }
        v.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(core::cmp::Ordering::Equal).then(x.0.cmp(&y.0)));
        v.truncate(top_m);
        v.push((SPILL, self.spill_cost[i]));
        v.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(core::cmp::Ordering::Equal));
        v
    }
}

fn apply(a: u8, used: &mut u64) {
    if a < SPILL {
        *used |= 1u64 << a;
    }
}

/// Greedy-with-noise completion from `next`; returns the cost added.
fn cost_rollout(p: &Problem, assign: &mut [u8], used: &mut u64, next: usize, eps: f64, rng: &mut Rng) -> f64 {
    let mut total = 0.0;
    for i in next..p.len() {
        // The cheapest action; a spill is only taken when it is. With
        // probability `eps` one of the three cheapest instead.
        let pick = if eps > 0.0 && rng.f() < eps {
            let acts = p.actions(i, assign, *used, 4);
            acts[rng.gen_range(acts.len().min(3))]
        } else {
            p.best(i, assign, *used)
        };
        assign[i] = pick.0;
        apply(pick.0, used);
        total += pick.1;
    }
    total
}

struct CNode {
    partial: Vec<u8>,
    used: u64,
    next: usize,
    cost: f64,
    parent: u32,
    prior: f64,
    children: Vec<u32>,
    expanded: bool,
    n: u32,
    w: f64,
}

/// Search the problem; returns the best assignment (`NONE` never appears:
/// each entry is a register index or `SPILL`) and its cost.
pub fn search_problem(p: &Problem, seed: u64, params: &Params) -> (Vec<u8>, f64) {
    let m = p.len();
    let mut rng = Rng(seed ^ 0x5851_f42d_4c95_7f2d);
    // Baseline: one greedy pass (also fixes the reward scale).
    let mut base = vec![NONE; m];
    let mut used = 0u64;
    let base_cost = cost_rollout(p, &mut base, &mut used, 0, 0.0, &mut rng);
    let mut best = (base, base_cost);
    if m == 0 || params.sims <= 1 {
        return best;
    }
    // Work budget: a rollout is about m * (degree + registers).
    let deg: usize = p.adj.iter().map(|a| a.len()).sum::<usize>() / m.max(1);
    let per = (m * (deg + p.n_regs + 4)).max(1);
    let sims = params.sims.min((params.work / per).max(1));
    let norm = base_cost.max(1.0);
    let mut nodes: Vec<CNode> = Vec::with_capacity(sims + 1);
    nodes.push(CNode {
        partial: vec![NONE; m],
        used: 0,
        next: 0,
        cost: 0.0,
        parent: u32::MAX,
        prior: 1.0,
        children: Vec::new(),
        expanded: false,
        n: 0,
        w: 0.0,
    });
    // The tree only has to settle the early, expensive decisions.
    let depth_cap = 16usize.min(m);
    for _ in 0..sims {
        // Select.
        let mut cur = 0u32;
        loop {
            let nd = &nodes[cur as usize];
            if nd.next >= depth_cap || !nd.expanded || nd.children.is_empty() {
                break;
            }
            let sqrt_n = (nd.n.max(1) as f64).sqrt();
            let mut pick = nd.children[0];
            let mut bs = f64::NEG_INFINITY;
            for &ch in &nd.children {
                let c = &nodes[ch as usize];
                let q = if c.n == 0 { 0.0 } else { c.w / c.n as f64 };
                let mut sc = q + params.cpuct * c.prior * sqrt_n / (1.0 + c.n as f64);
                if c.n == 0 {
                    sc += 1e6 + c.prior;
                }
                if sc > bs {
                    bs = sc;
                    pick = ch;
                }
            }
            cur = pick;
        }
        // Expand.
        let nd = &nodes[cur as usize];
        if !nd.expanded && nd.next < depth_cap {
            let (partial, used, next, cost) = (nd.partial.clone(), nd.used, nd.next, nd.cost);
            let acts = p.actions(next, &partial, used, params.top_m);
            let mx = acts.iter().map(|a| -a.1).fold(f64::NEG_INFINITY, f64::max);
            let z: f64 = acts.iter().map(|a| ((-a.1 - mx) / 2.0).exp()).sum::<f64>().max(1e-12);
            let mut kids = Vec::new();
            for (a, d) in acts {
                let mut pa = partial.clone();
                pa[next] = a;
                let mut u = used;
                apply(a, &mut u);
                kids.push(nodes.len() as u32);
                nodes.push(CNode {
                    partial: pa,
                    used: u,
                    next: next + 1,
                    cost: cost + d,
                    parent: cur,
                    prior: ((-d - mx) / 2.0).exp() / z,
                    children: Vec::new(),
                    expanded: false,
                    n: 0,
                    w: 0.0,
                });
            }
            nodes[cur as usize].children = kids.clone();
            nodes[cur as usize].expanded = true;
            if let Some(&first) = kids.first() {
                cur = first;
            }
        }
        // Evaluate by rollout from `cur`.
        let (mut pa, mut u, nx, c0) = {
            let c = &nodes[cur as usize];
            (c.partial.clone(), c.used, c.next, c.cost)
        };
        let add = cost_rollout(p, &mut pa, &mut u, nx, 0.12, &mut rng);
        let total = c0 + add;
        if total < best.1 {
            best = (pa, total);
        }
        // Back up.
        let r = -total / norm;
        let mut at = cur;
        loop {
            let c = &mut nodes[at as usize];
            c.n += 1;
            c.w += r;
            if c.parent == u32::MAX {
                break;
            }
            at = c.parent;
        }
    }
    let (mut homes, _) = best;
    refine(p, &mut homes, 4);
    let cost = total_cost(p, &homes);
    (homes, cost)
}

/// Exact cost of a complete assignment (each affinity pair once).
pub fn total_cost(p: &Problem, assign: &[u8]) -> f64 {
    let mut c = 0.0;
    let mut used = 0u64;
    for i in 0..p.len() {
        let a = assign[i];
        if a == SPILL {
            c += p.spill_cost[i];
            continue;
        }
        used |= 1u64 << a;
        for &(q, w) in &p.pins[i] {
            if q != a {
                c += w;
            }
        }
        for &(j, w) in &p.aff[i] {
            if (j as usize) > i && assign[j as usize] != a {
                c += w;
            }
        }
    }
    c + p.cs_cost * (used & p.callee_saved).count_ones() as f64
}

/// Local search over a complete assignment: re-place each node, given
/// everything else, on the register that costs least (or off the stack if a
/// legal register is cheaper than its spill). The greedy order chooses a
/// node's register before its later affinity partners exist, so chains of
/// tied operands (a vector accumulator) end up in different registers and
/// pay a copy per link; this pulls them together.
pub fn refine(p: &Problem, assign: &mut [u8], rounds: usize) {
    let m = p.len();
    for _ in 0..rounds {
        let mut changed = false;
        let mut used = 0u64;
        for &a in assign.iter() {
            if a < SPILL {
                used |= 1u64 << a;
            }
        }
        for i in 0..m {
            let cur = assign[i];
            // Legal registers given every other node's current home.
            let legal = {
                let full = if p.n_regs >= 64 { !0u64 } else { (1u64 << p.n_regs) - 1 };
                let mut mk = full & !p.forbid[i];
                for &j in &p.adj[i] {
                    let a = assign[j as usize];
                    if a < SPILL {
                        mk &= !(1u64 << a);
                    }
                }
                mk
            };
            let mut best_a = cur;
            let mut best_c = if cur == SPILL { p.spill_cost[i] } else { p.delta(i, cur, assign, used) };
            let mut m2 = legal;
            while m2 != 0 {
                let a = m2.trailing_zeros() as u8;
                m2 &= m2 - 1;
                if a == cur {
                    continue;
                }
                let c = p.delta(i, a, assign, used);
                if c + 1e-9 < best_c {
                    best_c = c;
                    best_a = a;
                }
            }
            if best_a != cur {
                assign[i] = best_a;
                if best_a < SPILL {
                    used |= 1u64 << best_a;
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Total physical-register footprint of an assignment slice (simple measure).
pub fn preg_count(assign: &[Option<Allocation>]) -> usize {
    let mut seen = Vec::new();
    for a in assign.iter().flatten() {
        if let Some(r) = a.as_reg() {
            if !seen.iter().any(|x| *x == r) {
                seen.push(r);
            }
        }
    }
    seen.len()
}

/// Footprint of a concrete Allocation slice (no Option wrapper).
pub fn preg_count_allocs(homes: &[Allocation]) -> usize {
    let mut seen = Vec::new();
    for a in homes {
        if let Some(r) = a.as_reg() {
            if !seen.iter().any(|x| *x == r) {
                seen.push(r);
            }
        }
    }
    seen.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PRegSet;

    fn tiny_env(n_regs: usize) -> MachineEnv {
        MachineEnv {
            preferred_regs_by_class: [
                (0..n_regs)
                    .map(|no| PReg::new(no, RegClass::Int))
                    .collect(),
                PRegSet::empty(),
                PRegSet::empty(),
            ],
            non_preferred_regs_by_class: [PRegSet::empty(); 3],
            scratch_by_class: [None, None, None],
            fixed_stack_slots: vec![],
        }
    }

    #[test]
    fn reward_counts_footprint_and_spills() {
        assert_eq!(reward(&[0, 0, SPILL]), -((1 + SPILL_W) as f64));
        assert_eq!(reward(&[0, 1, 2]), -3.0);
        let _ = crate::VReg::new(0, RegClass::Int);
    }

    #[test]
    fn prior_prefers_used_preg_over_new() {
        let mut used = [false; 256];
        used[1] = true;
        let lu = action_logit(1, &used, None);
        let ln = action_logit(2, &used, None);
        let ls = action_logit(SPILL, &used, None);
        assert!(lu > ln);
        assert!(ln > ls);
        assert!(action_logit(2, &used, Some(2)) > ln);
    }

    #[test]
    fn top_m_caps_branching() {
        // No interference → every preg is free; top_m=2 → 2 pregs + spill.
        let eligible = [0usize];
        let interfere: Vec<Vec<usize>> = vec![vec![]];
        let partial = [NONE];
        let first = [0usize];
        let last = [1usize];
        let forbid: [Vec<u8>; 1] = [Vec::new()];
        let acts = considered_actions(
            &eligible, &interfere, &first, &last, &forbid, &partial, 0, 16, 2, None,
        );
        assert_eq!(acts.len(), 3);
        assert!(acts.iter().any(|(a, _)| *a == SPILL));
        assert_eq!(acts.iter().filter(|(a, _)| *a != SPILL).count(), 2);
    }

    #[test]
    fn triangle_mcts_footprint_le_first_fit() {
        // K3: three pairwise-interfering vregs → chromatic 3. First-fit uses
        // 3; MCTS with the regcount reward must not do worse.
        let eligible = vec![0usize, 1, 2];
        let interfere = vec![vec![1, 2], vec![0, 2], vec![0, 1]];
        let n_regs = 4;
        let top_m = 8;
        let ff = first_fit_partial(&eligible, &interfere, n_regs, top_m);
        let ff_fp = partial_preg_count(&ff);
        assert_eq!(ff_fp, 3, "first-fit on K3 should use 3 pregs");

        let params = Params {
            sims: 128,
            top_m,
            gumbel_root: true,
            cpuct: 1.5,
            ..Params::default()
        };
        let env = tiny_env(n_regs);
        let first = vec![0usize, 0, 0];
        let last = vec![1usize, 1, 1];
        let fixed = vec![None, None, None];
        let forbid: Vec<Vec<u8>> = vec![Vec::new(); 3];
        let (homes, _) = search_class_with(
            &eligible,
            &first,
            &last,
            &fixed,
            &forbid,
            &interfere,
            &env,
            RegClass::Int,
            0,
            0x7e14_a91e,
            &params,
        );
        let mcts_fp = preg_count_allocs(&homes);
        assert!(
            mcts_fp <= ff_fp,
            "mcts footprint {} > first-fit {}",
            mcts_fp,
            ff_fp
        );
        // Also exercise the Option-slice helper used by callers.
        let wrapped: Vec<Option<Allocation>> = homes.iter().copied().map(Some).collect();
        assert_eq!(preg_count(&wrapped), mcts_fp);
    }

    #[test]
    fn gumbel_samples_are_finite() {
        let mut rng = Rng(42);
        for _ in 0..32 {
            let g = gumbel(&mut rng);
            assert!(g.is_finite());
        }
    }

    /// K4 with 3 registers: the cheapest node is the optimistic spill
    /// candidate, so it is coloured last and is the one that spills.
    #[test]
    fn simplify_order_spills_cheapest_clique_member() {
        let m = 4usize;
        let mut adj = vec![Vec::new(); m];
        for a in 0..m {
            for b in 0..m {
                if a != b {
                    adj[a].push(b as u32);
                }
            }
        }
        let p = Problem {
            n_regs: 3,
            adj,
            forbid: vec![0; m],
            spill_cost: vec![9.0, 8.0, 1.0, 7.0],
            pins: vec![Vec::new(); m],
            aff: vec![Vec::new(); m],
            callee_saved: 0,
            cs_cost: 0.0,
        };
        let order = p.simplify_order().unwrap();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3]);
        assert_eq!(*order.last().unwrap(), 2, "cheapest is coloured last: {:?}", order);
        let q = p.permuted(&order);
        let (homes, cost) = search_problem(&q, 1, &Params::default());
        let spilled: Vec<usize> = (0..m).filter(|&i| homes[i] == SPILL).collect();
        assert_eq!(spilled, vec![m - 1], "{:?}", homes);
        assert!((cost - 1.0).abs() < 1e-9, "{}", cost);
    }
}

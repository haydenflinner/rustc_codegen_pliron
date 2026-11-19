//! PLIRON: greedy register allocator (LLVM SplitKit-shaped control loop).
//!
//! v1 (A/B via `PLIRON_RA_ALGO=greedy`):
//! - Default: every vreg gets its own spill slot; Reg/FixedReg sites get
//!   temp reload/spill edits; branches get parallel arg→param moves.
//!   Checker-clean on rafuzz (20k seeds).
//! - Opt-in `PLIRON_RA_GREEDY_COLOR=1`: per-block linear scan promoting
//!   single-block non-param vregs (branch args OK → reg→stack edge moves;
//!   block params stay spilled). Clobber- and foreign-FixedReg-aware.
//!   Multi-block stays spilled. Edge parallel-move edits are unlabelled.
//!   Checker-clean on rafuzz (10k seeds); not yet measured on held-out
//!   — do not flip default.
//! - Opt-in `PLIRON_RA_PROOFCOLOR=1` (needs COLOR): when a class has
//!   ≤ `prove::MAX_EXACT` verts and `try_fewer` finds a colouring with
//!   K < the linear-scan home footprint, rematch single-block non-param
//!   homes onto that certificate (FixedReg / clobber / foreign-FixedReg
//!   aware). Default off; fitness loop in `harness/raproof_learn.py`.
//! - `PLIRON_RA_ALGO=mcts` (or `PLIRON_RA_MCTS=1` with COLOR): Gumbel-root
//!   / prior-shaped PUCT over homes minimizing preg footprint + spill
//!   weight (`mcts_ra`, `PLIRON_RA_MCTS_SIMS` / `_TOPM` / `_GUMBEL`). Patterns from `~/code/tourn` Gumbel-MCTS.
//! - FixedReg pins (allocatable only), clobber-forbid for temps,
//!   Reuse-use-as-Reg (tied use gets a throwaway temp when still live),
//!   fixed-nonalloc operands, stack↔fixed-slot breaks.
//!
//! Default remains ion until colouring wins held-out.

use crate::cfg::CFGInfo;
use crate::ion::Stats;
use crate::moves::{MoveAndScratchResolver, ParallelMoves};
use crate::prove::{
    check_colouring, interfere_graph, prove_class, MAX_EXACT,
};
use crate::{
    ssa::validate_ssa, Allocation, Block, Edit, Function, Inst, MachineEnv, OperandConstraint,
    OperandKind, OperandPos, Output, PReg, PRegSet, ProgPoint, RegAllocError, RegClass, SpillSlot, VReg,
};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// `NAME=0` turns a default-on knob off (std only).
fn env_usize(name: &str, default: usize) -> usize {
    #[cfg(feature = "std")]
    {
        if let Ok(v) = std::env::var(name) {
            if let Ok(x) = v.parse() {
                return x;
            }
        }
    }
    let _ = name;
    default
}

/// `TooManyLiveRegs` with a trace of the emit site that ran out of temps.
fn too_many(_line: u32, _inst: usize) -> RegAllocError {
    #[cfg(feature = "std")]
    if crate::explain_on() || env_flag("PLIRON_RA_FALLBACK_LOG") {
        std::eprintln!("ra2x: greedy emit out of temps at greedy/mod.rs:{} inst {}", _line, _inst);
    }
    RegAllocError::TooManyLiveRegs
}

fn env_off(name: &str) -> bool {
    #[cfg(feature = "std")]
    {
        std::env::var(name).as_deref() == Ok("0")
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = name;
        false
    }
}

/// PLIRON: env on/off helpers (std only; no-std keeps knobs off).
fn env_flag(name: &str) -> bool {
    #[cfg(feature = "std")]
    {
        std::env::var(name).as_deref() == Ok("1")
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = name;
        false
    }
}

#[derive(Clone, Debug)]
struct Interval {
    vreg: VReg,
    class: RegClass,
    first: usize,
    last: usize,
    weight: u32,
    /// Preferred physical register from a FixedReg constraint, if any.
    fixed: Option<PReg>,
}

impl Interval {
    fn overlaps(&self, o: &Interval) -> bool {
        self.first < o.last && o.first < self.last && self.class == o.class
    }
}

/// Dense bitset: `words = (n + 63) / 64`, bit `i` in word `i/64`.
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

/// Block-level liveness: bit `v` of `live_in[b*w..]` / `live_out[b*w..]`.
struct Liveness {
    w: usize,
    live_in: Vec<u64>,
    live_out: Vec<u64>,
}

/// Block-param moves and branch args count as uses at the terminator.
fn build_liveness<F: Function>(func: &F, n: usize) -> Liveness {
    if n == 0 {
        return Liveness { w: 0, live_in: Vec::new(), live_out: Vec::new() };
    }
    let nb = func.num_blocks();
    let w = bit_words(n);
    let mut ue_use = vec![0u64; nb * w];
    let mut defs = vec![0u64; nb * w];

    for b in 0..nb {
        let block = Block::new(b);
        let ue = &mut ue_use[b * w..(b + 1) * w];
        let df = &mut defs[b * w..(b + 1) * w];
        let mut seen_def = vec![0u64; w];
        for &v in func.block_params(block) {
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
                let i = v.vreg();
                if i < n && !bit_get(&seen_def, i) {
                    bit_set(ue, i);
                }
            }
        }
    }

    // Backward dataflow, word-wise: out = union of successors' in,
    // in = use-before-def | (out & !def). Blocks are visited last to first
    // (most successors are later), so a loop-free region settles in one
    // sweep; buffers are reused.
    let mut live_in = vec![0u64; nb * w];
    let mut live_out = vec![0u64; nb * w];
    let mut new_out = vec![0u64; w];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..nb).rev() {
            new_out.iter_mut().for_each(|x| *x = 0);
            for &succ in func.block_succs(Block::new(b)) {
                let s = succ.index();
                for (o, i) in new_out.iter_mut().zip(&live_in[s * w..(s + 1) * w]) {
                    *o |= *i;
                }
            }
            let range = b * w..(b + 1) * w;
            let mut any = false;
            for (k, idx) in range.clone().enumerate() {
                let new_in = ue_use[idx] | (new_out[k] & !defs[idx]);
                if new_in != live_in[idx] {
                    live_in[idx] = new_in;
                    any = true;
                }
                if new_out[k] != live_out[idx] {
                    live_out[idx] = new_out[k];
                    any = true;
                }
            }
            changed |= any;
        }
    }
    Liveness { w, live_in, live_out }
}

/// SSA-ish liveness → interference pairs. Over-approx is fine (extra spills);
/// under-approx yields IncorrectValues. `interfere[v]` lists conflicting vregs.
fn build_interfere<F: Function>(func: &F, n: usize, lv: &Liveness) -> Vec<Vec<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let nb = func.num_blocks();
    let mut pairs: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut edge = |a: usize, b: usize| {
        if a != b {
            pairs[a].push(b as u32);
            pairs[b].push(a as u32);
        }
    };
    let mut members: Vec<usize> = Vec::new();
    let share_off = env_off("PLIRON_RA_SHARE_DEF_USE");
    // Backward walk of every block: a definition interferes with everything
    // live just after it (and with the uses of its own instruction: a use
    // that dies there could share the register, but the emitter does not
    // rely on it). A value dies at its last use, not at the end of the
    // block: the old forward walk kept it live to the block's end and made
    // the graph close to a clique in a big loop.
    for b in 0..nb {
        let block = Block::new(b);
        let mut live = block_end_live(func, lv, b, n);
        for inst in func.block_insns(block).iter().rev() {
            members.clear();
            for op in func.inst_operands(inst) {
                if op.as_fixed_nonallocatable().is_some() {
                    continue;
                }
                let i = op.vreg().vreg();
                if i < n && op.kind() == OperandKind::Def {
                    members.push(i);
                }
            }
            if !members.is_empty() {
                let ndefs = members.len();
                for k in 0..ndefs {
                    let d = members[k];
                    for k2 in (k + 1)..ndefs {
                        edge(d, members[k2]);
                    }
                    for (wi, &word) in live.iter().enumerate() {
                        let mut word = word;
                        while word != 0 {
                            let bit = word.trailing_zeros() as usize;
                            word &= word - 1;
                            edge(d, wi * 64 + bit);
                        }
                    }
                }
                // A def interferes with the uses of its own instruction,
                // except that a Late def may share a register with an Early
                // use that dies here (ion's rule; it is what lets a tied
                // `Reuse` operand cost no move). A `Reuse` def is written
                // (copied into) before the instruction reads, so it shares
                // only with its own source, and a source that some def
                // reuses is never shared with another def (its register
                // is that def's).
                let ops = func.inst_operands(inst);
                let reuse_src = |j: usize| -> Option<usize> {
                    ops.get(j).filter(|o| o.as_fixed_nonallocatable().is_none()).map(|o| o.vreg().vreg())
                };
                for (di, dop) in ops.iter().enumerate() {
                    if dop.as_fixed_nonallocatable().is_some() || dop.kind() != OperandKind::Def {
                        continue;
                    }
                    let d = dop.vreg().vreg();
                    if d >= n {
                        continue;
                    }
                    let tied = match dop.constraint() {
                        OperandConstraint::Reuse(j) => Some(reuse_src(j)),
                        _ => None,
                    };
                    for uop in ops.iter() {
                        if uop.as_fixed_nonallocatable().is_some() || uop.kind() != OperandKind::Use {
                            continue;
                        }
                        let u = uop.vreg().vreg();
                        if u >= n {
                            continue;
                        }
                        let mut share = !share_off
                            && !bit_get(&live, u)
                            && dop.pos() == OperandPos::Late
                            && uop.pos() == OperandPos::Early;
                        if share {
                            if let Some(src) = tied {
                                share = src == Some(u);
                            } else {
                                share = !ops.iter().enumerate().any(|(oi, o)| {
                                    oi != di
                                        && o.kind() == OperandKind::Def
                                        && matches!(o.constraint(), OperandConstraint::Reuse(j) if reuse_src(j) == Some(u))
                                });
                            }
                        }
                        if !share {
                            edge(d, u);
                        }
                    }
                }
            }
            step_back(func, inst, n, &mut live, |_| {}, |_| {});
        }
        // Block entry: parameters are written together with the live-in set.
        members.clear();
        for (wi, &word) in live.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                members.push(wi * 64 + bit);
            }
        }
        for &v in func.block_params(block) {
            if v.vreg() < n && !bit_get(&live, v.vreg()) {
                members.push(v.vreg());
            }
        }
        for x in 0..members.len() {
            for y in (x + 1)..members.len() {
                edge(members[x], members[y]);
            }
        }
    }
    pairs
        .into_iter()
        .map(|mut v| {
            v.sort_unstable();
            v.dedup();
            v.into_iter().map(|x| x as usize).collect()
        })
        .collect()
}

/// Live set at the end of block `b`: its live-out plus the branch args the
/// terminator reads.
fn block_end_live<F: Function>(func: &F, lv: &Liveness, b: usize, n: usize) -> Vec<u64> {
    let mut live = lv.live_out[b * lv.w..(b + 1) * lv.w].to_vec();
    let block = Block::new(b);
    let term = func.block_insns(block).last();
    for si in 0..func.block_succs(block).len() {
        for &v in func.branch_blockparams(block, term, si) {
            if v.vreg() < n {
                bit_set(&mut live, v.vreg());
            }
        }
    }
    live
}

/// Backward transfer over one instruction: defs die, uses become live.
/// Calls `on_add`/`on_del` for each change of the live set.
fn step_back<F: Function>(
    func: &F,
    inst: Inst,
    n: usize,
    live: &mut [u64],
    mut on_add: impl FnMut(usize),
    mut on_del: impl FnMut(usize),
) {
    for op in func.inst_operands(inst) {
        if op.as_fixed_nonallocatable().is_some() || op.kind() != OperandKind::Def {
            continue;
        }
        let i = op.vreg().vreg();
        if i < n && bit_get(live, i) {
            live[i / 64] &= !(1u64 << (i % 64));
            on_del(i);
        }
    }
    for op in func.inst_operands(inst) {
        if op.as_fixed_nonallocatable().is_some() || op.kind() != OperandKind::Use {
            continue;
        }
        let i = op.vreg().vreg();
        if i < n && !bit_get(live, i) {
            bit_set(live, i);
            on_add(i);
        }
    }
}

/// Work budget for `build_forbid` (member visits); a function over it keeps
/// colouring to single-block vregs.
const FORBID_BUDGET: u64 = 40_000_000;

/// PLIRON: per vreg, the registers it must not live in: every clobber and
/// every other vreg's `FixedReg` at an instruction where it is live before,
/// live after or touched (what `clobbers_in_range` and `foreign_fixed_in_range`
/// approximate with the linear first..last window, wrong around loops and
/// across blocks). `None` over `FORBID_BUDGET`.
fn build_forbid<F: Function>(func: &F, lv: &Liveness, n: usize) -> Option<Vec<PRegSet>> {
    let mut forbid = vec![PRegSet::empty(); n];
    let mut visits = 0u64;
    for b in 0..func.num_blocks() {
        let mut live = block_end_live(func, lv, b, n);
        for inst in func.block_insns(Block::new(b)).iter().rev() {
            let clob: PRegSet = func.inst_clobbers(inst);
            let mut fixed: Vec<(PReg, usize)> = Vec::new();
            for op in func.inst_operands(inst) {
                if op.as_fixed_nonallocatable().is_some() {
                    continue;
                }
                if let OperandConstraint::FixedReg(p) = op.constraint() {
                    fixed.push((p, op.vreg().vreg()));
                }
            }
            let any_clob = clob != PRegSet::empty();
            if any_clob || !fixed.is_empty() {
                // present = live after ∪ the operands' vregs. A clobber does
                // not touch a vreg that is only read here (Early uses, dead
                // after): the instruction reads it before it clobbers, so a
                // call argument may be homed in the register it is passed in.
                let mut apply = |v: usize, clobbered: bool, visits: &mut u64| {
                    *visits += 1;
                    if clobbered {
                        forbid[v].union_from(clob);
                    }
                    for &(p, w) in &fixed {
                        if w != v {
                            forbid[v].add(p);
                        }
                    }
                };
                for (wi, &word) in live.iter().enumerate() {
                    let mut m = word;
                    while m != 0 {
                        let v = wi * 64 + m.trailing_zeros() as usize;
                        m &= m - 1;
                        apply(v, any_clob, &mut visits);
                    }
                }
                let ops = func.inst_operands(inst);
                for op in ops {
                    if op.as_fixed_nonallocatable().is_some() || op.vreg().vreg() >= n {
                        continue;
                    }
                    let v = op.vreg().vreg();
                    if bit_get(&live, v) {
                        continue;
                    }
                    let read_only = !env_off("PLIRON_RA_FORBID_USEONLY")
                        && ops.iter().all(|o| {
                            o.as_fixed_nonallocatable().is_some()
                                || o.vreg().vreg() != v
                                || (o.kind() == OperandKind::Use && o.pos() == OperandPos::Early)
                        });
                    apply(v, any_clob && !read_only, &mut visits);
                }
                if visits > FORBID_BUDGET {
                    return None;
                }
            }
            step_back(func, inst, n, &mut live, |_| {}, |_| {});
        }
    }
    Some(forbid)
}

/// What `emit` asks of liveness once homes exist.
struct HomeLive {
    /// Per instruction: registers holding a home that is live before, after
    /// or touched there (`emit`'s `forbidden` start; dead defs included).
    present: Vec<PRegSet>,
    /// Per instruction with a `Reuse` def: whether each operand's vreg is
    /// still live after the instruction.
    live_after: crate::FxHashMap<usize, Vec<bool>>,
}

/// PLIRON: `HomeLive` by a backward walk with a count of live vregs per
/// register. Replaces the linear-interval `live_regs_at`, which misses a
/// home that is live around a loop's back edge or across blocks.
fn build_home_live<F: Function>(
    func: &F,
    lv: &Liveness,
    n: usize,
    assign: &[Option<Allocation>],
) -> HomeLive {
    let mut present = vec![PRegSet::empty(); func.num_insts()];
    let mut live_after = crate::FxHashMap::default();
    let home_reg = |v: usize| -> Option<PReg> {
        assign.get(v).copied().flatten().and_then(|a| a.as_reg())
    };
    for b in 0..func.num_blocks() {
        let mut live = block_end_live(func, lv, b, n);
        let mut cnt = vec![0u32; PReg::NUM_INDEX];
        let mut set = PRegSet::empty();
        for (wi, &word) in live.iter().enumerate() {
            let mut m = word;
            while m != 0 {
                let v = wi * 64 + m.trailing_zeros() as usize;
                m &= m - 1;
                if let Some(r) = home_reg(v) {
                    cnt[r.index()] += 1;
                    set.add(r);
                }
            }
        }
        for inst in func.block_insns(Block::new(b)).iter().rev() {
            let ops = func.inst_operands(inst);
            if ops.iter().any(|o| matches!(o.constraint(), OperandConstraint::Reuse(_))) {
                let flags = ops
                    .iter()
                    .map(|o| {
                        o.as_fixed_nonallocatable().is_none()
                            && o.vreg().vreg() < n
                            && bit_get(&live, o.vreg().vreg())
                    })
                    .collect();
                live_after.insert(inst.index(), flags);
            }
            let mut here = set;
            for op in ops {
                if op.as_fixed_nonallocatable().is_none() {
                    if let Some(r) = home_reg(op.vreg().vreg()) {
                        here.add(r);
                    }
                }
            }
            present[inst.index()] = here;
            // Both closures update the same counts; `RefCell`-free: collect.
            let mut added: Vec<usize> = Vec::new();
            let mut deleted: Vec<usize> = Vec::new();
            step_back(func, inst, n, &mut live, |v| added.push(v), |v| deleted.push(v));
            for v in deleted {
                if let Some(r) = home_reg(v) {
                    let c = &mut cnt[r.index()];
                    *c -= 1;
                    if *c == 0 {
                        set.remove(r);
                    }
                }
            }
            for v in added {
                if let Some(r) = home_reg(v) {
                    cnt[r.index()] += 1;
                    set.add(r);
                }
            }
        }
    }
    HomeLive { present, live_after }
}

fn build_intervals<F: Function>(func: &F, env: &MachineEnv) -> Vec<Interval> {
    let n = func.num_vregs();
    let mut first = vec![usize::MAX; n];
    let mut last = vec![0usize; n];
    let mut uses = vec![0usize; n];
    let mut class = vec![None; n];
    // Conflict → None; single FixedReg → Some(p); none seen → unset (None in fixed_seen sense)
    let mut fixed: Vec<Option<Option<PReg>>> = vec![None; n];

    let note_fixed = |fixed: &mut [Option<Option<PReg>>], i: usize, p: PReg| {
        match fixed[i] {
            None => fixed[i] = Some(Some(p)),
            Some(Some(q)) if q == p => {}
            Some(Some(_)) => fixed[i] = Some(None), // conflicting fixed regs
            Some(None) => {}
        }
    };

    for b in 0..func.num_blocks() {
        let block = Block::new(b);
        for &v in func.block_params(block) {
            let i = v.vreg();
            if i >= n {
                continue;
            }
            let at = func.block_insns(block).first().index();
            first[i] = first[i].min(at);
            last[i] = last[i].max(at + 1);
            uses[i] += 1;
            class[i] = Some(v.class());
        }
        for inst in func.block_insns(block).iter() {
            for op in func.inst_operands(inst) {
                if op.as_fixed_nonallocatable().is_some() {
                    continue;
                }
                let v = op.vreg();
                let i = v.vreg();
                if i >= n {
                    continue;
                }
                first[i] = first[i].min(inst.index());
                last[i] = last[i].max(inst.index() + 1);
                uses[i] += 1;
                class[i] = Some(v.class());
                if let OperandConstraint::FixedReg(p) = op.constraint() {
                    // Only pin allocatable pregs as homes; non-allocatable
                    // FixedRegs (ABI temps) are handled by edits at the site.
                    if is_alloc_reg(env, p) {
                        note_fixed(&mut fixed, i, p);
                    }
                }
            }
        }
        // Branch args are not in `inst_operands` — extend live ranges so
        // colouring does not reuse a source preg before the edge move.
        let term = func.block_insns(block).last();
        let nsucc = func.block_succs(block).len();
        for si in 0..nsucc {
            for &v in func.branch_blockparams(block, term, si) {
                let i = v.vreg();
                if i >= n {
                    continue;
                }
                first[i] = first[i].min(term.index());
                last[i] = last[i].max(term.index() + 1);
                uses[i] += 1;
                class[i] = Some(v.class());
            }
        }
    }

    let mut out = Vec::new();
    for i in 0..n {
        if first[i] == usize::MAX {
            continue;
        }
        let len = last[i].saturating_sub(first[i]).max(1);
        let u = uses[i].max(1) as u64;
        let weight = ((u << 16) / (len as u64).max(1).isqrt().max(1)).min(u32::MAX as u64) as u32;
        out.push(Interval {
            vreg: VReg::new(i, class[i].unwrap_or(RegClass::Int)),
            class: class[i].unwrap_or(RegClass::Int),
            first: first[i],
            last: last[i],
            weight,
            fixed: fixed[i].unwrap_or(None),
        });
    }
    out.sort_by(|a, b| {
        // Fixed pins first (they must win the preg), then longer, then heavier.
        match (a.fixed.is_some(), b.fixed.is_some()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => match b
                .last
                .saturating_sub(b.first)
                .cmp(&a.last.saturating_sub(a.first))
            {
                Ordering::Equal => b.weight.cmp(&a.weight),
                o => o,
            },
        }
    });
    out
}

fn regs_for_class(env: &MachineEnv, class: RegClass) -> Vec<PReg> {
    let idx = class as usize;
    let mut regs = Vec::new();
    for r in env.preferred_regs_by_class[idx] {
        regs.push(r);
    }
    for r in env.non_preferred_regs_by_class[idx] {
        regs.push(r);
    }
    regs
}

fn is_alloc_reg(env: &MachineEnv, p: PReg) -> bool {
    regs_for_class(env, p.class()).iter().any(|r| *r == p)
}

/// Indices of intervals already coloured onto `r` that interfere with `iv`.
fn conflicts_on(
    intervals: &[Interval],
    interfere: &[Vec<usize>],
    assign: &[Option<Allocation>],
    iv: &Interval,
    r: PReg,
) -> Vec<usize> {
    let mut out = Vec::new();
    let v = iv.vreg.vreg();
    let foes = interfere.get(v).map(|s| s.as_slice()).unwrap_or(&[]);
    for (j, other) in intervals.iter().enumerate() {
        let u = other.vreg.vreg();
        if u == v || other.class != iv.class {
            continue;
        }
        // Union of SSA liveness and first/last (either may miss a case).
        if foes.binary_search(&u).is_err() && !iv.overlaps(other) {
            continue;
        }
        if assign
            .get(u)
            .copied()
            .flatten()
            .and_then(|a| a.as_reg())
            == Some(r)
        {
            out.push(j);
        }
    }
    out
}

/// PRegs clobbered by any inst in `iv`'s first/last window.
fn clobbers_in_range<F: Function>(func: &F, iv: &Interval) -> Vec<PReg> {
    let mut out = Vec::new();
    let last = iv.last.min(func.num_insts());
    for idx in iv.first..last {
        for r in func.inst_clobbers(Inst::new(idx)) {
            if !out.iter().any(|x| *x == r) {
                out.push(r);
            }
        }
    }
    out
}

/// Foreign FixedReg(p) sites inside `iv`'s window — `p` is a temp there.
fn foreign_fixed_in_range<F: Function>(func: &F, iv: &Interval) -> Vec<PReg> {
    let mut out = Vec::new();
    let last = iv.last.min(func.num_insts());
    let self_v = iv.vreg.vreg();
    for idx in iv.first..last {
        for op in func.inst_operands(Inst::new(idx)) {
            if op.vreg().vreg() == self_v {
                continue;
            }
            if let OperandConstraint::FixedReg(p) = op.constraint() {
                if !out.iter().any(|x| *x == p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Loop depth per block from layout back-edges (a successor at or before the
/// block closes a loop over the blocks between them).
fn block_depths<F: Function>(func: &F) -> Vec<u32> {
    let nb = func.num_blocks();
    let mut depth = vec![0u32; nb];
    for b in 0..nb {
        for &t in func.block_succs(Block::new(b)) {
            if t.index() <= b {
                for d in depth[t.index()..=b].iter_mut() {
                    *d += 1;
                }
            }
        }
    }
    depth
}

/// Cost units: one register move is 1. A spilled operand is a load or store
/// (several moves' worth of ports and latency); a rematerialized one is a
/// single ALU instruction. (`PLIRON_RA_MEMCOST` could tune it; fixed for now.)
const MEM_OP_COST: f64 = 3.0;
const REMAT_OP_COST: f64 = 1.0;

/// What a vreg's home decides, summed over its operands: the search's cost
/// model (see `mcts_ra::Problem`).
struct Costs {
    /// Memory operations a spilled vreg pays (operands, loop-weighted).
    spill: Vec<f64>,
    /// `FixedReg` pins with their loop-weighted operand counts.
    pins: Vec<Vec<(PReg, f64)>>,
    /// Copy-like pairs: block-param edges, `Reuse`, plain `mov`s.
    aff: Vec<(usize, usize, f64)>,
}

fn build_costs<F: Function>(func: &F, n: usize) -> Costs {
    let depth = block_depths(func);
    let mut c = Costs { spill: vec![0.0; n], pins: vec![Vec::new(); n], aff: Vec::new() };
    for b in 0..func.num_blocks() {
        let w = 8f64.powi(depth[b].min(4) as i32);
        let block = Block::new(b);
        for &v in func.block_params(block) {
            if v.vreg() < n {
                c.spill[v.vreg()] += w;
            }
        }
        for inst in func.block_insns(block).iter() {
            let ops = func.inst_operands(inst);
            for (k, op) in ops.iter().enumerate() {
                if op.as_fixed_nonallocatable().is_some() || op.vreg().vreg() >= n {
                    continue;
                }
                let v = op.vreg().vreg();
                c.spill[v] += w;
                match op.constraint() {
                    OperandConstraint::FixedReg(p) => {
                        match c.pins[v].iter_mut().find(|(q, _)| *q == p) {
                            Some(e) => e.1 += w,
                            None => c.pins[v].push((p, w)),
                        }
                    }
                    OperandConstraint::Reuse(ui) => {
                        if let Some(u) = ops.get(ui) {
                            if u.as_fixed_nonallocatable().is_none()
                                && u.vreg().vreg() < n
                                && u.vreg().class() == op.vreg().class()
                            {
                                c.aff.push((v, u.vreg().vreg(), w));
                            }
                        }
                    }
                    _ => {}
                }
                let _ = k;
            }
            // A two-operand use+def of one class with no clobbers is a copy.
            if ops.len() == 2 && func.inst_clobbers(inst) == PRegSet::empty() {
                let (a, d) = (&ops[0], &ops[1]);
                let plain = |o: &crate::Operand| {
                    o.as_fixed_nonallocatable().is_none()
                        && matches!(o.constraint(), OperandConstraint::Any | OperandConstraint::Reg)
                };
                if a.kind() == OperandKind::Use
                    && d.kind() == OperandKind::Def
                    && plain(a)
                    && plain(d)
                    && a.vreg().class() == d.vreg().class()
                    && a.vreg().vreg() < n
                    && d.vreg().vreg() < n
                {
                    c.aff.push((a.vreg().vreg(), d.vreg().vreg(), 0.9 * w));
                }
            }
        }
        let term = func.block_insns(block).last();
        for si in 0..func.block_succs(block).len() {
            let succ = func.block_succs(block)[si];
            let args = func.branch_blockparams(block, term, si);
            let params = func.block_params(succ);
            for (&arg, &param) in args.iter().zip(params.iter()) {
                if arg.vreg() < n && param.vreg() < n {
                    c.spill[arg.vreg()] += w;
                    c.aff.push((arg.vreg(), param.vreg(), w));
                }
            }
        }
    }
    // A rematerializable vreg (a constant) reloads as one ALU instruction,
    // not a memory access, and its definition needs no store: spilling it is
    // cheap, which is how ion fits a loop's dozens of constants in registers.
    for v in 0..n {
        c.spill[v] *= if func.is_remat(v) { REMAT_OP_COST } else { MEM_OP_COST };
    }
    c
}

/// Preferred temps per regclass from a proof colouring (empty = no preference).
type ProofPrefs = [Vec<PReg>; 3];

fn empty_prefs() -> ProofPrefs {
    [Vec::new(), Vec::new(), Vec::new()]
}

fn color<F: Function>(
    func: &F,
    intervals: &[Interval],
    interfere: &[Vec<usize>],
    env: &MachineEnv,
    force_mcts: bool,
    allow_color: bool,
    forbid_sets: Option<&[PRegSet]>,
    reserve: usize,
    spilled_cost: &mut f64,
) -> (Vec<Option<Allocation>>, usize, ProofPrefs) {
    let n = intervals
        .iter()
        .map(|i| i.vreg.vreg())
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);
    let mut assign = vec![None; n];
    let mut spillslots = 0usize;
    let mut prefs = empty_prefs();

    let spill = |assign: &mut [Option<Allocation>], spillslots: &mut usize, v: usize| {
        assign[v] = Some(Allocation::stack(SpillSlot::new(*spillslots)));
        *spillslots += 1;
    };

    // Default all-spill. `PLIRON_RA_GREEDY_COLOR=1` or `ALGO=mcts` promotes.
    let mcts_on = force_mcts || crate::mcts_ra::enabled();
    let color_on = env_flag("PLIRON_RA_GREEDY_COLOR") || mcts_on;
    // Always start spilled (checker-clean baseline). Colouring promotes.
    for iv in intervals {
        spill(&mut assign, &mut spillslots, iv.vreg.vreg());
    }
    if !color_on || !allow_color {
        return (assign, spillslots, prefs);
    }

    // Inst → block map for the single-block colouring gate.
    let mut inst_block_idx = vec![0usize; func.num_insts()];
    for b in 0..func.num_blocks() {
        for inst in func.block_insns(Block::new(b)).iter() {
            if inst.index() < inst_block_idx.len() {
                inst_block_idx[inst.index()] = b;
            }
        }
    }
    let block_of = |idx: usize| -> usize {
        if inst_block_idx.is_empty() {
            0
        } else {
            inst_block_idx[idx.min(inst_block_idx.len() - 1)]
        }
    };
    let single_block = |iv: &Interval| -> bool {
        if iv.last == 0 || func.num_insts() == 0 {
            return true;
        }
        let last_inst = iv.last.min(func.num_insts()).saturating_sub(1);
        block_of(iv.first) == block_of(last_inst)
    };

    // Block params stay spilled (colouring them still fails the checker —
    // see history/2026-11-18-ra-greedy-color.md). Branch args may colour.
    let mut is_param = vec![false; n];
    for b in 0..func.num_blocks() {
        for &v in func.block_params(Block::new(b)) {
            if v.vreg() < n {
                is_param[v.vreg()] = true;
            }
        }
    }

    let costs = if mcts_on && forbid_sets.is_some() { Some(build_costs(func, n)) } else { None };
    if mcts_on {
        // UCT over all single-block non-param vregs of each class.
        // Order by weight desc so the tree decides hot values first.
        for &class in &[RegClass::Int, RegClass::Float, RegClass::Vector] {
            let mut elig: Vec<(u32, usize)> = intervals
                .iter()
                .filter(|iv| {
                    iv.class == class
                        && (forbid_sets.is_some() || single_block(iv))
                        && (!is_param[iv.vreg.vreg()]
                            || (forbid_sets.is_some() && !env_off("PLIRON_RA_COLOR_PARAMS")))
                })
                .map(|iv| (iv.weight, iv.vreg.vreg()))
                .collect();
            elig.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let eligible: Vec<usize> = elig.into_iter().map(|(_, v)| v).collect();
            #[cfg(feature = "std")]
            if crate::explain_on() {
                std::eprintln!(
                    "ra2x: mcts class={:?} intervals={} eligible={} forbid_sets={}",
                    class,
                    intervals.iter().filter(|iv| iv.class == class).count(),
                    eligible.len(),
                    forbid_sets.is_some()
                );
            }
            if eligible.is_empty() {
                continue;
            }
            let regs = regs_for_class(env, class);
            let preg_ix = |p: PReg| -> Option<u8> {
                regs.iter().position(|r| *r == p).map(|i| i as u8)
            };
            // Whole-function costed search (needs exact forbid sets and at
            // most 64 registers in the class); else the footprint search.
            if let (Some(sets), Some(costs)) = (forbid_sets, costs.as_ref()) {
                if regs.len() <= 64 {
                    let mut eligible = eligible;
                    eligible.sort_by(|&a, &b| {
                        costs.spill[b].partial_cmp(&costs.spill[a]).unwrap_or(Ordering::Equal).then(a.cmp(&b))
                    });
                    let mut ix = vec![u32::MAX; n];
                    for (i, &v) in eligible.iter().enumerate() {
                        ix[v] = i as u32;
                    }
                    let m = eligible.len();
                    let mut prob = crate::mcts_ra::Problem {
                        n_regs: regs.len(),
                        adj: vec![Vec::new(); m],
                        forbid: vec![0u64; m],
                        spill_cost: vec![0.0; m],
                        pins: vec![Vec::new(); m],
                        aff: vec![Vec::new(); m],
                        callee_saved: 0,
                        cs_cost: 2.0,
                    };
                    for (i, r) in regs.iter().enumerate() {
                        if env.non_preferred_regs_by_class[class as usize].contains(*r) {
                            prob.callee_saved |= 1u64 << i;
                        }
                    }
                    for (i, &v) in eligible.iter().enumerate() {
                        prob.adj[i] = interfere[v]
                            .iter()
                            .filter_map(|&u| (ix[u] != u32::MAX).then_some(ix[u]))
                            .collect();
                        for (k, &r) in regs.iter().enumerate() {
                            if sets[v].contains(r) {
                                prob.forbid[i] |= 1u64 << k;
                            }
                        }
                        prob.spill_cost[i] = costs.spill[v];
                        for &(p, w) in &costs.pins[v] {
                            if let Some(k) = preg_ix(p) {
                                prob.pins[i].push((k, w));
                            }
                        }
                    }
                    for &(a, b, w) in &costs.aff {
                        if ix[a] != u32::MAX && ix[b] != u32::MAX && a != b {
                            prob.aff[ix[a] as usize].push((ix[b], w));
                            prob.aff[ix[b] as usize].push((ix[a], w));
                        }
                    }
                    // Emit needs temporaries (operand copies, edge scratch) at
                    // the busiest instruction: homes never take the last few
                    // registers of the class, or the whole function falls
                    // back to all-spill (`TooManyLiveRegs`).
                    let reserve = reserve.min(regs.len().saturating_sub(1));
                    for f in prob.forbid.iter_mut() {
                        for k in 0..reserve {
                            *f |= 1u64 << (regs.len() - 1 - k);
                        }
                    }
                    for a in prob.adj.iter_mut() {
                        a.sort_unstable();
                        a.dedup();
                    }
                    let seed = m as u64 ^ ((class as u64) << 32) ^ (func.num_insts() as u64);
                    let params = crate::mcts_ra::Params::from_env();
                    // Colour long-lived, high-degree values first (Briggs order).
                    let homes = match (!env_off("PLIRON_RA_SIMPLIFY"))
                        .then(|| prob.simplify_order())
                        .flatten()
                    {
                        Some(order) => {
                            let (h, _) = crate::mcts_ra::search_problem(&prob.permuted(&order), seed, &params);
                            let mut out = vec![0u8; m];
                            for (n, &o) in order.iter().enumerate() {
                                out[o] = h[n];
                            }
                            out
                        }
                        None => crate::mcts_ra::search_problem(&prob, seed, &params).0,
                    };
                    #[cfg(feature = "std")]
                    if crate::explain_on() {
                        for (i, &v) in eligible.iter().enumerate() {
                            let mut nb = 0u64;
                            for &j in &prob.adj[i] {
                                if (homes[j as usize] as usize) < regs.len() {
                                    nb |= 1u64 << homes[j as usize];
                                }
                            }
                            std::eprintln!(
                                "ra2x: mcts #{} v{} class={:?} home={} forbid={:#x} spill={:.0} nbr_regs={} nregs={} pins={:?} aff={:?}",
                                i, v, class, homes[i], prob.forbid[i], prob.spill_cost[i], nb.count_ones(), regs.len(), prob.pins[i], prob.aff[i]
                            );
                        }
                    }
                    let mut used = Vec::new();
                    for (i, &v) in eligible.iter().enumerate() {
                        let h = homes[i];
                        if (h as usize) >= regs.len() {
                            *spilled_cost += prob.spill_cost[i];
                        }
                        if (h as usize) < regs.len() {
                            assign[v] = Some(Allocation::reg(regs[h as usize]));
                            if !used.iter().any(|x| *x == regs[h as usize]) {
                                used.push(regs[h as usize]);
                            }
                        }
                        // SPILL keeps the all-spill home `color` started with.
                    }
                    prefs[class as usize] = used;
                    continue;
                }
            }
            let mut first = Vec::with_capacity(eligible.len());
            let mut last = Vec::with_capacity(eligible.len());
            let mut fixed = Vec::with_capacity(eligible.len());
            let mut forbid = Vec::with_capacity(eligible.len());
            for &v in &eligible {
                let iv = intervals.iter().find(|i| i.vreg.vreg() == v).unwrap();
                first.push(iv.first);
                last.push(iv.last);
                fixed.push(iv.fixed.and_then(preg_ix));
                let mut fb = Vec::new();
                if let Some(sets) = forbid_sets {
                    // Precise: where the vreg is live, not its linear window.
                    for (i, &r) in regs.iter().enumerate() {
                        if sets[v].contains(r) {
                            fb.push(i as u8);
                        }
                    }
                } else {
                    for p in clobbers_in_range(func, iv)
                        .into_iter()
                        .chain(foreign_fixed_in_range(func, iv))
                    {
                        if let Some(i) = preg_ix(p) {
                            fb.push(i);
                        }
                    }
                }
                forbid.push(fb);
            }
            let seed = eligible.len() as u64
                ^ ((class as u64) << 32)
                ^ (func.num_insts() as u64);
            let (homes, next_spill) = crate::mcts_ra::search_class(
                &eligible,
                &first,
                &last,
                &fixed,
                &forbid,
                interfere,
                env,
                class,
                spillslots,
                seed,
            );
            spillslots = next_spill;
            let mut used = Vec::new();
            for (i, &v) in eligible.iter().enumerate() {
                assign[v] = Some(homes[i]);
                if let Some(r) = homes[i].as_reg() {
                    if !used.iter().any(|x| *x == r) {
                        used.push(r);
                    }
                }
            }
            // Keep emit temps inside the searched footprint (same gap
            // PROOFCOLOR closed: homes were fine, temps escaped).
            prefs[class as usize] = used;
        }
    } else {
        // Per-block linear scan: single-block non-param intervals.
        for b in 0..func.num_blocks() {
            for &class in &[RegClass::Int, RegClass::Float, RegClass::Vector] {
                let mut local: Vec<&Interval> = intervals
                    .iter()
                    .filter(|iv| {
                        iv.class == class
                            && single_block(iv)
                            && block_of(iv.first) == b
                            && !is_param[iv.vreg.vreg()]
                    })
                    .collect();
                local.sort_by(|a, c| a.first.cmp(&c.first).then(a.last.cmp(&c.last)));

                let mut active: Vec<(usize, PReg)> = Vec::new();
                let regs = regs_for_class(env, class);

                for iv in local {
                    active.retain(|(last, _)| *last > iv.first);
                    let clobbered = clobbers_in_range(func, iv);
                    let foreign_fixed = foreign_fixed_in_range(func, iv);
                    let taken: Vec<PReg> = active.iter().map(|(_, p)| *p).collect();

                    let mut candidates = regs.clone();
                    if let Some(p) = iv.fixed {
                        if let Some(pos) = candidates.iter().position(|r| *r == p) {
                            candidates.swap(0, pos);
                        }
                    }
                    let chosen = candidates.into_iter().find(|r| {
                        !taken.iter().any(|t| t == r)
                            && !clobbered.iter().any(|c| c == r)
                            && !foreign_fixed.iter().any(|c| c == r)
                    });
                    if let Some(r) = chosen {
                        assign[iv.vreg.vreg()] = Some(Allocation::reg(r));
                        active.push((iv.last, r));
                    }
                }
            }
        }
    }

    // PLIRON: proof-guided rematch + temp prefs (`PLIRON_RA_PROOFCOLOR=1`).
    if env_flag("PLIRON_RA_PROOFCOLOR") {
        prefs = proof_recolor(
            func,
            intervals,
            &mut assign,
            &mut spillslots,
            env,
            &is_param,
            |iv| single_block(iv),
        );
    }

    // Safety net against interfere∪overlap conflicts (e.g. reuse ties).
    let _ = interfere;
    // PLIRON: with exact liveness (`forbid_sets`) the interference graph is
    // the whole truth; the linear first..last overlap below would undo every
    // multi-block home (a loop-carried value overlaps half the function).
    let mut progress = forbid_sets.is_none();
    if forbid_sets.is_some() {
        let mut iv_of = vec![usize::MAX; n];
        for (j, iv) in intervals.iter().enumerate() {
            iv_of[iv.vreg.vreg()] = j;
        }
        for iv in intervals.iter() {
            let v = iv.vreg.vreg();
            let Some(r) = assign.get(v).copied().flatten().and_then(|a| a.as_reg()) else {
                continue;
            };
            let mut loser = v;
            let mut conflict = false;
            for &u in interfere.get(v).map(|s| s.as_slice()).unwrap_or(&[]) {
                if u >= n || iv_of[u] == usize::MAX || intervals[iv_of[u]].class != iv.class {
                    continue;
                }
                if assign[u].and_then(|a| a.as_reg()) == Some(r) {
                    conflict = true;
                    if intervals[iv_of[u]].weight < intervals[iv_of[loser]].weight {
                        loser = u;
                    }
                }
            }
            if conflict {
                spill(&mut assign, &mut spillslots, loser);
            }
        }
    }
    while progress {
        progress = false;
        for iv in intervals.iter() {
            let v = iv.vreg.vreg();
            let Some(r) = assign.get(v).copied().flatten().and_then(|a| a.as_reg()) else {
                continue;
            };
            let conf = conflicts_on(intervals, interfere, &assign, iv, r);
            if conf.is_empty() {
                continue;
            }
            let mut spill_v = v;
            let mut spill_w = iv.weight;
            for &j in &conf {
                if intervals[j].weight < spill_w {
                    spill_w = intervals[j].weight;
                    spill_v = intervals[j].vreg.vreg();
                }
            }
            spill(&mut assign, &mut spillslots, spill_v);
            progress = true;
        }
    }

    (assign, spillslots, prefs)
}

/// Distinct physical homes currently assigned to `class` (spill slots ignored).
fn home_footprint(assign: &[Option<Allocation>], intervals: &[Interval], class: RegClass) -> usize {
    let mut seen = Vec::new();
    for iv in intervals {
        if iv.class != class {
            continue;
        }
        if let Some(r) = assign
            .get(iv.vreg.vreg())
            .copied()
            .flatten()
            .and_then(|a| a.as_reg())
        {
            if !seen.iter().any(|x| *x == r) {
                seen.push(r);
            }
        }
    }
    seen.len()
}

/// Map abstract colours `0..k` onto allocatable PRegs, honouring FixedReg pins
/// among `eligible` vregs. Returns `None` when pins conflict.
fn map_colours_to_pregs(
    colour: &[u32],
    k: u32,
    class: RegClass,
    env: &MachineEnv,
    intervals: &[Interval],
    eligible: &[usize],
) -> Option<Vec<PReg>> {
    let mut map: Vec<Option<PReg>> = vec![None; k as usize];
    let iv_of = |v: usize| -> Option<&Interval> {
        intervals.iter().find(|iv| iv.vreg.vreg() == v)
    };
    for &v in eligible {
        let c = *colour.get(v).unwrap_or(&u32::MAX);
        if c == u32::MAX || c >= k {
            continue;
        }
        let Some(iv) = iv_of(v) else { continue };
        let Some(p) = iv.fixed else { continue };
        if p.class() != class || !is_alloc_reg(env, p) {
            return None;
        }
        match map[c as usize] {
            None => map[c as usize] = Some(p),
            Some(q) if q == p => {}
            Some(_) => return None, // same colour, two FixedRegs
        }
    }
    let mut used: Vec<PReg> = map.iter().filter_map(|x| *x).collect();
    for i in 0..used.len() {
        for j in (i + 1)..used.len() {
            if used[i] == used[j] {
                return None;
            }
        }
    }
    let regs = regs_for_class(env, class);
    for slot in map.iter_mut() {
        if slot.is_some() {
            continue;
        }
        let r = *regs.iter().find(|r| !used.iter().any(|u| u == *r))?;
        used.push(r);
        *slot = Some(r);
    }
    Some(map.into_iter().map(|x| x.unwrap()).collect())
}

/// PLIRON: apply a chromatic colouring when the linear-scan home footprint
/// exceeds `upper_k`, and always hand emit a preferred temp set of those
/// K pregs so reload/spill edits do not inflate the preg footprint past
/// the certificate. Class size must be ≤ `MAX_EXACT`; FixedReg / clobber /
/// foreign-FixedReg aware; params / multi-block stay spilled.
fn proof_recolor<F: Function>(
    func: &F,
    intervals: &[Interval],
    assign: &mut [Option<Allocation>],
    spillslots: &mut usize,
    env: &MachineEnv,
    is_param: &[bool],
    single_block: impl Fn(&Interval) -> bool,
) -> ProofPrefs {
    let spill_one = |assign: &mut [Option<Allocation>], spillslots: &mut usize, v: usize| {
        assign[v] = Some(Allocation::stack(SpillSlot::new(*spillslots)));
        *spillslots += 1;
    };
    let mut prefs = empty_prefs();
    let g = interfere_graph(func);
    for &class in &[RegClass::Int, RegClass::Float, RegClass::Vector] {
        let bound = prove_class(&g, class);
        if bound.n_class == 0 || !bound.exact || bound.n_class > MAX_EXACT {
            continue;
        }
        let (Some(cert), Some(k)) = (&bound.upper, bound.upper_k) else {
            continue;
        };
        if check_colouring(&g, &cert.colour, k).is_err() {
            continue;
        }
        // Eligible = what COLOR promotes: single-block non-param of this class.
        let mut eligible: Vec<usize> = Vec::new();
        for iv in intervals {
            if iv.class != class {
                continue;
            }
            let v = iv.vreg.vreg();
            if v < is_param.len() && is_param[v] {
                continue;
            }
            if !single_block(iv) {
                continue;
            }
            let c = *cert.colour.get(v).unwrap_or(&u32::MAX);
            if c == u32::MAX {
                continue;
            }
            eligible.push(v);
        }
        if eligible.is_empty() {
            continue;
        }
        let Some(cmap) =
            map_colours_to_pregs(&cert.colour, k, class, env, intervals, &eligible)
        else {
            continue;
        };
        // Temps prefer the chromatic preg set (fall back if all forbidden).
        prefs[class as usize] = cmap.clone();

        let before = home_footprint(assign, intervals, class);
        if before <= k as usize {
            continue; // homes already at/under chromatic; prefs still help edits
        }

        let touched: Vec<(usize, Option<Allocation>)> = eligible
            .iter()
            .map(|&v| (v, assign.get(v).copied().flatten()))
            .collect();

        for &v in &eligible {
            let c = cert.colour[v] as usize;
            let p = cmap[c];
            let Some(iv) = intervals.iter().find(|iv| iv.vreg.vreg() == v) else {
                continue;
            };
            let clobbered = clobbers_in_range(func, iv);
            let foreign = foreign_fixed_in_range(func, iv);
            if clobbered.iter().any(|x| *x == p) || foreign.iter().any(|x| *x == p) {
                if assign
                    .get(v)
                    .copied()
                    .flatten()
                    .and_then(|a| a.as_reg())
                    .is_some()
                {
                    spill_one(assign, spillslots, v);
                }
                continue;
            }
            assign[v] = Some(Allocation::reg(p));
        }

        let after = home_footprint(assign, intervals, class);
        if after >= before {
            for (v, old) in touched {
                assign[v] = old;
            }
        }
    }
    prefs
}

fn home_of(assign: &[Option<Allocation>], v: VReg) -> Allocation {
    assign
        .get(v.vreg())
        .copied()
        .flatten()
        .unwrap_or(Allocation::none())
}

fn live_regs_at(intervals: &[Interval], assign: &[Option<Allocation>], at: usize) -> Vec<PReg> {
    let mut live = Vec::new();
    for iv in intervals {
        if iv.first > at || at >= iv.last {
            continue;
        }
        if let Some(r) = assign
            .get(iv.vreg.vreg())
            .copied()
            .flatten()
            .and_then(|a| a.as_reg())
        {
            live.push(r);
        }
    }
    live
}

fn pick_temp(
    env: &MachineEnv,
    class: RegClass,
    forbidden: &[PReg],
    prefer: &[PReg],
) -> Option<PReg> {
    // PLIRON: proof-colour prefs first so edits stay inside the chromatic set.
    for r in prefer {
        if !forbidden.iter().any(|f| f == r) {
            return Some(*r);
        }
    }
    for r in regs_for_class(env, class) {
        if !forbidden.iter().any(|f| *f == r) {
            return Some(r);
        }
    }
    env.scratch_by_class[class as usize].filter(|r| !forbidden.iter().any(|f| f == r))
}

/// Like `pick_temp`, but if every allocable preg is forbidden, steal a home
/// whose interval has already ended (`last <= at`) or not yet started.
fn pick_temp_or_dead_home(
    env: &MachineEnv,
    class: RegClass,
    forbidden: &[PReg],
    _intervals: &[Interval],
    _assign: &[Option<Allocation>],
    _at: usize,
    prefer: &[PReg],
) -> Option<PReg> {
    for r in prefer {
        if !forbidden.iter().any(|f| f == r) {
            return Some(*r);
        }
    }
    for r in regs_for_class(env, class) {
        if !forbidden.iter().any(|f| *f == r) {
            return Some(r);
        }
    }
    // `forbidden` holds every live home (`build_home_live`), so a register
    // outside it is free; the old "dead home by interval" scan could hand
    // out a home that is live around a loop.
    None
}

fn is_stack_like(env: &MachineEnv, a: Allocation) -> bool {
    a.is_stack()
        || a.as_reg()
            .map(|r| env.fixed_stack_slots.iter().any(|s| *s == r))
            .unwrap_or(false)
}

fn push_edit(
    edits: &mut Vec<(ProgPoint, Edit)>,
    edit_vregs: &mut Vec<Option<VReg>>,
    pp: ProgPoint,
    from: Allocation,
    to: Allocation,
    v: Option<VReg>,
) {
    if from != to && !from.is_none() && !to.is_none() {
        #[cfg(feature = "std")]
        if std::env::var("PLIRON_RA_BT").is_ok() && to.is_stack() {
            std::eprintln!("BT {:?} {:?}->{:?}\n{}", pp, from, to, std::backtrace::Backtrace::force_capture());
        }
        edits.push((pp, Edit::Move { from, to }));
        edit_vregs.push(v);
    }
}

/// Like `push_edit`, but breaks stack↔stack (including fixed stack-slot
/// pregs) through an allocatable temp so the checker stays happy.
fn push_move(
    env: &MachineEnv,
    intervals: &[Interval],
    assign: &[Option<Allocation>],
    at: usize,
    edits: &mut Vec<(ProgPoint, Edit)>,
    edit_vregs: &mut Vec<Option<VReg>>,
    pp: ProgPoint,
    from: Allocation,
    to: Allocation,
    v: Option<VReg>,
    class: RegClass,
    forbidden: &mut Vec<PReg>,
    prefer: &[PReg],
) -> Result<(), RegAllocError> {
    if from == to || from.is_none() || to.is_none() {
        return Ok(());
    }
    if is_stack_like(env, from) && is_stack_like(env, to) {
        let tmp = pick_temp_or_dead_home(env, class, forbidden, intervals, assign, at, prefer)
            .ok_or_else(|| too_many(line!(), at))?;
        forbidden.push(tmp);
        let t = Allocation::reg(tmp);
        push_edit(edits, edit_vregs, pp, from, t, v);
        push_edit(edits, edit_vregs, pp, t, to, v);
    } else {
        push_edit(edits, edit_vregs, pp, from, to, v);
    }
    Ok(())
}

/// PLIRON: where work for the edge `from -> to` goes, as ion's
/// `choose_move_location` decides: before the terminator when `to` has other
/// predecessors and `from` has one way out, at the head of `to` when `to` has
/// a single predecessor (the in-edge flag), `None` for a critical edge.
fn edge_point<F: Function>(func: &F, from: Block, to: Block) -> Option<(ProgPoint, bool)> {
    let last = func.block_insns(from).last();
    let first = func.block_insns(to).first();
    let from_outs = func.block_succs(from).len() + usize::from(func.is_ret(last));
    let to_ins = func.block_preds(to).len() + usize::from(func.entry_block() == to);
    if to_ins > 1 && from_outs <= 1 {
        Some((ProgPoint::before(last), false))
    } else if to_ins <= 1 {
        Some((ProgPoint::before(first), true))
    } else {
        None
    }
}

/// PLIRON: the points at which a def of `inst` lands in its home. A
/// terminator that defines values (`try_call`) finishes before any edit
/// after it can run, so the store goes at the head of each single-predecessor
/// successor, like ion's edge moves. (A def interferes with everything live
/// out of its block, so its home is free there whether or not it is live.)
fn def_points<F: Function>(func: &F, inst: Inst, block: Block) -> Vec<(ProgPoint, bool)> {
    if !func.is_branch(inst) || func.is_ret(inst) {
        return vec![(ProgPoint::after(inst), false)];
    }
    let mut pts = Vec::new();
    for &succ in func.block_succs(block) {
        if let Some((pp, true)) = edge_point(func, block, succ) {
            pts.push((pp, true));
        }
    }
    if pts.is_empty() {
        pts.push((ProgPoint::after(inst), false));
    }
    pts
}

/// `push_move` at each of `points`, recording the in-edge ones (they run
/// ahead of the successor's own operand edits at the same program point).
#[allow(clippy::too_many_arguments)]
fn push_move_pts(
    points: &[(ProgPoint, bool)],
    in_edge: &mut Vec<(usize, usize)>,
    env: &MachineEnv,
    intervals: &[Interval],
    assign: &[Option<Allocation>],
    at: usize,
    edits: &mut Vec<(ProgPoint, Edit)>,
    edit_vregs: &mut Vec<Option<VReg>>,
    from: Allocation,
    to: Allocation,
    v: Option<VReg>,
    class: RegClass,
    forbidden: &mut Vec<PReg>,
    prefer: &[PReg],
) -> Result<(), RegAllocError> {
    for &(pp, edge) in points {
        let start = edits.len();
        push_move(env, intervals, assign, at, edits, edit_vregs, pp, from, to, v, class, forbidden, prefer)?;
        if edge {
            in_edge.push((start, edits.len()));
        }
    }
    Ok(())
}

/// What `emit` produces: allocs, per-inst offsets, edits, edit labels, the
/// logical slot count and the class of each scratch slot it added.
type EmitOut = (
    Vec<Allocation>,
    Vec<u32>,
    Vec<(ProgPoint, Edit)>,
    Vec<Option<VReg>>,
    usize,
    Vec<(usize, RegClass)>,
);

/// Is `r` already an operand register of this instruction for a vreg other
/// than `v`?
/// A Late def does not count an Early use's register as taken: the use is
/// read before the def writes (the colouring only lets them share when the
/// use dies there).
fn taken_by_other(taken: &[(PReg, usize, bool)], r: PReg, v: usize, late_def: bool) -> bool {
    taken.iter().any(|&(t, tv, early_use)| t == r && tv != v && !(late_def && early_use))
}

fn emit<F: Function>(
    func: &F,
    env: &MachineEnv,
    intervals: &[Interval],
    assign: &mut [Option<Allocation>],
    mut num_spillslots: usize,
    prefs: &ProofPrefs,
    lv: &Liveness,
    n_vregs: usize,
) -> Result<EmitOut, RegAllocError> {
    let prefer_of = |c: RegClass| prefs[c as usize].as_slice();
    let home_live = build_home_live(func, lv, n_vregs, assign);
    let mut allocs = Vec::new();
    let mut inst_alloc_offsets = Vec::with_capacity(func.num_insts());
    let mut edits = Vec::new();
    let mut edit_vregs = Vec::new();
    // Ranges of `edits` that are in-edge work (see `push_move_pts`).
    let mut in_edge: Vec<(usize, usize)> = Vec::new();
    let mut scratch_tags: Vec<(usize, RegClass)> = Vec::new();

    // Find the block each instruction belongs to (for branch edge moves).
    let mut inst_block = vec![Block::new(0); func.num_insts()];
    for b in 0..func.num_blocks() {
        let block = Block::new(b);
        for inst in func.block_insns(block).iter() {
            inst_block[inst.index()] = block;
        }
    }

    for inst_idx in 0..func.num_insts() {
        let inst = Inst::new(inst_idx);
        inst_alloc_offsets.push(allocs.len() as u32);
        let ops: Vec<_> = func.inst_operands(inst).iter().copied().collect();
        let mut op_alloc = vec![Allocation::none(); ops.len()];
        let mut forbidden: Vec<PReg> = home_live.present[inst_idx].into_iter().collect();
        // Temps must not land in the instruction's clobber set — a Def into a
        // clobbered preg is gone before the After spill edit runs.
        let clobbers: Vec<PReg> = func.inst_clobbers(inst).into_iter().collect();
        for &r in &clobbers {
            if !forbidden.iter().any(|f| *f == r) {
                forbidden.push(r);
            }
        }
        // PLIRON: work for a branch's edges runs at the head of a
        // single-predecessor successor (`edge_point`), where that block's
        // live homes and its parameters' homes must also survive any temp.
        if func.is_branch(inst) && !func.is_ret(inst) {
            let blk = inst_block[inst_idx];
            for &succ in func.block_succs(blk) {
                if let Some((_, true)) = edge_point(func, blk, succ) {
                    let first = func.block_insns(succ).first();
                    let mut extra: Vec<PReg> = home_live.present[first.index()].into_iter().collect();
                    for &pv in func.block_params(succ) {
                        if let Some(r) = home_of(assign, pv).as_reg() {
                            extra.push(r);
                        }
                    }
                    for r in extra {
                        if !forbidden.iter().any(|f| *f == r) {
                            forbidden.push(r);
                        }
                    }
                }
            }
        }
        // Clobber survival is a colouring rule (`clobbers_in_range`); no
        // emit-time vacate — that was a source of EditVRegMismatch.

        // Uses that a Reuse-def points at must live in a register (the def
        // overwrites that same alloc) — even when the use says Any/Stack.
        // If the use vreg is still live after this inst, the tied location
        // must be a throwaway temp so the use's home survives.
        let mut reuse_use = vec![false; ops.len()];
        let mut reuse_use_live_after = vec![false; ops.len()];
        for op in &ops {
            if let OperandConstraint::Reuse(ui) = op.constraint() {
                if ui < reuse_use.len() {
                    reuse_use[ui] = true;
                    // Exact liveness (the linear interval's `last` misses a
                    // use that stays live around a loop).
                    if home_live
                        .live_after
                        .get(&inst_idx)
                        .is_some_and(|f| f.get(ui).copied().unwrap_or(false))
                    {
                        reuse_use_live_after[ui] = true;
                    }
                }
            }
        }

        // A tied use that must survive the instruction is copied before it.
        // Copy it straight into the def's own register when no other operand
        // of the instruction holds that register: the def then writes its
        // home and no move follows (a scratch temp costs a second `mov`).
        let mut reuse_def_home: Vec<Option<PReg>> = vec![None; ops.len()];
        for (di, op) in ops.iter().enumerate() {
            let OperandConstraint::Reuse(ui) = op.constraint() else { continue };
            if ui >= ops.len()
                || !reuse_use_live_after[ui]
                || op.as_fixed_nonallocatable().is_some()
                || ops[ui].constraint() != OperandConstraint::Reg
            {
                continue;
            }
            let Some(r) = home_of(assign, op.vreg()).as_reg() else { continue };
            let clash = ops.iter().enumerate().any(|(oi, o)| {
                if oi == di || oi == ui {
                    return false;
                }
                if let Some(p) = o.as_fixed_nonallocatable() {
                    return p == r;
                }
                if let OperandConstraint::FixedReg(p) = o.constraint() {
                    if p == r {
                        return true;
                    }
                }
                home_of(assign, o.vreg()).as_reg() == Some(r)
            });
            if !clash && !is_stack_like(env, Allocation::reg(r)) {
                reuse_def_home[ui] = Some(r);
            }
        }

        // FixedRegs first (reserve their pregs), then other uses, then defs.
        let order: Vec<usize> = {
            let mut fixed = Vec::new();
            let mut u = Vec::new();
            let mut d = Vec::new();
            for (i, op) in ops.iter().enumerate() {
                if op.as_fixed_nonallocatable().is_some()
                    || matches!(op.constraint(), OperandConstraint::FixedReg(_))
                {
                    fixed.push(i);
                } else if op.kind() == OperandKind::Use {
                    u.push(i);
                } else {
                    d.push(i);
                }
            }
            fixed.extend(u);
            fixed.extend(d);
            fixed
        };

        // Registers the operands of this instruction already hold, with the
        // vreg that holds each (see `taken_by_other`).
        let mut taken: Vec<(PReg, usize, bool)> = Vec::new();
        for &i in &order {
            let op = ops[i];
            if let Some(p) = op.as_fixed_nonallocatable() {
                op_alloc[i] = Allocation::reg(p);
                continue;
            }
            let home = home_of(assign, op.vreg());
            let constraint = match op.constraint() {
                OperandConstraint::Any | OperandConstraint::Stack if reuse_use[i] => {
                    OperandConstraint::Reg
                }
                c => c,
            };
            // Tied use still live after this inst → never sit in home (def clobbers it).
            let force_reuse_temp = reuse_use[i] && reuse_use_live_after[i];
            let a = match constraint {
                OperandConstraint::FixedReg(p) => {
                    let preg = Allocation::reg(p);
                    let pts = if op.kind() == OperandKind::Use {
                        vec![(ProgPoint::before(inst), false)]
                    } else {
                        def_points(func, inst, inst_block[inst_idx])
                    };
                    // Colouring refuses homes that collide with foreign
                    // FixedReg(p) sites, so no emit-time vacate.
                    let home = home_of(assign, op.vreg());
                    if home != preg {
                        let (from, to) = if op.kind() == OperandKind::Use {
                            (home, preg)
                        } else {
                            (preg, home)
                        };
                        push_move_pts(
                            &pts,
                            &mut in_edge,
                            env,
                            intervals,
                            assign,
                            inst_idx,
                            &mut edits,
                            &mut edit_vregs,
                            from,
                            to,
                            Some(op.vreg()),
                            op.class(),
                            &mut forbidden,
                            prefer_of(op.class()),
                        )?;
                    }
                    if !is_stack_like(env, preg) && !forbidden.iter().any(|f| *f == p) {
                        forbidden.push(p);
                    }
                    preg
                }
                OperandConstraint::Reuse(ui) => {
                    let base = op_alloc.get(ui).copied().unwrap_or(Allocation::none());
                    let home = home_of(assign, op.vreg());
                    if base != home && !base.is_none() && !home.is_none() {
                        let pts = def_points(func, inst, inst_block[inst_idx]);
                        push_move_pts(
                            &pts,
                            &mut in_edge,
                            env,
                            intervals,
                            assign,
                            inst_idx,
                            &mut edits,
                            &mut edit_vregs,
                            base,
                            home,
                            Some(op.vreg()),
                            op.class(),
                            &mut forbidden,
                            prefer_of(op.class()),
                        )?;
                    }
                    // Operand allocation is the tied location (def writes here).
                    base
                }
                OperandConstraint::Stack | OperandConstraint::Any => {
                    // Any may sit in home even if another operand took that
                    // preg — checker allows the same alloc on one vreg only;
                    // if home's preg is already forbidden by a *different*
                    // vreg's operand, reload into a temp when the constraint
                    // will be read as a use alongside it. Stack/Any on a
                    // free home stays put.
                    if let Some(hr) = home.as_reg() {
                        if taken_by_other(&taken, hr, op.vreg().vreg(), false) && op.kind() == OperandKind::Use {
                            let tmp = pick_temp_or_dead_home(
                                env,
                                op.class(),
                                &forbidden,
                                intervals,
                                assign,
                                inst_idx,
                                prefer_of(op.class()),
                            )
                            .ok_or_else(|| too_many(line!(), inst_idx))?;
                            forbidden.push(tmp);
                            let t = Allocation::reg(tmp);
                            push_move(
                                env,
                                intervals,
                                assign,
                                inst_idx,
                                &mut edits,
                                &mut edit_vregs,
                                ProgPoint::before(inst),
                                home,
                                t,
                                Some(op.vreg()),
                                op.class(),
                                &mut forbidden,
                                prefer_of(op.class()),
                            )?;
                            t
                        } else {
                            home
                        }
                    } else {
                        home
                    }
                }
                OperandConstraint::Reg | OperandConstraint::Limit(_) => {
                    // Blocked only by a *different* operand of this inst:
                    // `forbidden` also holds every live home, this operand's
                    // own included (a `mov` for each register-homed operand).
                    let home_blocked = home
                        .as_reg()
                        .map(|hr| {
                            taken_by_other(
                                &taken,
                                hr,
                                op.vreg().vreg(),
                                op.kind() == OperandKind::Def && op.pos() == OperandPos::Late,
                            )
                        })
                        .unwrap_or(false);
                    let need_temp = force_reuse_temp
                        || home.is_stack()
                        || home_blocked
                        || matches!(op.constraint(), OperandConstraint::Limit(n) if home.as_reg().map(|r| (r.hw_enc() as usize) >= n).unwrap_or(true));
                    if !need_temp && home.is_reg() {
                        home
                    } else if home.is_none() {
                        home
                    } else {
                        let pref = prefer_of(op.class());
                        let tmp = if let Some(r) = reuse_def_home[i].filter(|_| force_reuse_temp) {
                            r
                        } else if let OperandConstraint::Limit(n) = op.constraint() {
                            regs_for_class(env, op.class())
                                .into_iter()
                                .find(|r| {
                                    (r.hw_enc() as usize) < n
                                        && !forbidden.iter().any(|f| f == r)
                                })
                                .or_else(|| {
                                    pick_temp_or_dead_home(
                                        env,
                                        op.class(),
                                        forbidden.as_slice(),
                                        intervals,
                                        assign,
                                        inst_idx,
                                        pref,
                                    )
                                    .filter(|r| (r.hw_enc() as usize) < n)
                                })
                                .ok_or_else(|| too_many(line!(), inst_idx))?
                        } else {
                            pick_temp_or_dead_home(
                                env,
                                op.class(),
                                &forbidden,
                                intervals,
                                assign,
                                inst_idx,
                                pref,
                            )
                            .ok_or_else(|| too_many(line!(), inst_idx))?
                        };
                        forbidden.push(tmp);
                        let t = Allocation::reg(tmp);
                        let pts = if op.kind() == OperandKind::Use {
                            vec![(ProgPoint::before(inst), false)]
                        } else {
                            def_points(func, inst, inst_block[inst_idx])
                        };
                        let (from, to) = if op.kind() == OperandKind::Use {
                            (home, t)
                        } else {
                            (t, home)
                        };
                        push_move_pts(
                            &pts,
                            &mut in_edge,
                            env,
                            intervals,
                            assign,
                            inst_idx,
                            &mut edits,
                            &mut edit_vregs,
                            from,
                            to,
                            Some(op.vreg()),
                            op.class(),
                            &mut forbidden,
                            pref,
                        )?;
                        t
                    }
                }
            };
            op_alloc[i] = a;
            if let Some(r) = a.as_reg() {
                taken.push((r, op.vreg().vreg(), op.kind() == OperandKind::Use && op.pos() == OperandPos::Early));
                if !forbidden.iter().any(|f| *f == r) {
                    forbidden.push(r);
                }
            }
        }

        // Branch → successor block-param parallel moves.
        if func.is_branch(inst) {
            let block = inst_block[inst_idx];
            let succs = func.block_succs(block);
            for (si, &succ) in succs.iter().enumerate() {
                let args = func.branch_blockparams(block, inst, si);
                let params = func.block_params(succ);
                if args.len() != params.len() {
                    continue;
                }
                // The edge's own sources and destinations are not scratch
                // candidates (the resolver asserts it), whichever side of the
                // branch the moves land on.
                for (&arg, &param) in args.iter().zip(params.iter()) {
                    for v in [arg, param] {
                        if let Some(r) = home_of(assign, v).as_reg() {
                            if !forbidden.iter().any(|f| *f == r) {
                                forbidden.push(r);
                            }
                        }
                    }
                }
                // PLIRON: one parallel move per register class, each with its
                // own scratch class: a vector spill moved through an integer
                // temp is cut to 64 bits (contains_rng's v128 loop state).
                for class_for_scratch in [RegClass::Int, RegClass::Float, RegClass::Vector] {
                let mut pm = ParallelMoves::<Option<VReg>>::new();
                for (&arg, &param) in args.iter().zip(params.iter()) {
                    if arg.class() != class_for_scratch {
                        continue;
                    }
                    let from = home_of(assign, arg);
                    let to = home_of(assign, param);
                    if from != to && !from.is_none() && !to.is_none() {
                        pm.add(from, to, Some(arg));
                    }
                }
                let resolved = pm.resolve();
                let mut scratch_slots = num_spillslots;
                // Prefer proof-coloured pregs, then the rest of the class.
                let mut avail: Vec<PReg> = prefer_of(class_for_scratch)
                    .iter()
                    .copied()
                    .filter(|r| !forbidden.iter().any(|f| f == r))
                    .collect();
                for r in regs_for_class(env, class_for_scratch) {
                    if !forbidden.iter().any(|f| f == &r) && !avail.iter().any(|a| *a == r) {
                        avail.push(r);
                    }
                }
                let mut avail_i = 0usize;
                let scratch_reg = env.scratch_by_class[class_for_scratch as usize];
                let resolver = MoveAndScratchResolver {
                    find_free_reg: || {
                        if avail_i < avail.len() {
                            let r = avail[avail_i];
                            avail_i += 1;
                            Some(Allocation::reg(r))
                        } else if let Some(r) = scratch_reg {
                            Some(Allocation::reg(r))
                        } else {
                            None
                        }
                    },
                    get_stackslot: || {
                        #[cfg(feature = "std")]
                        if env_flag("PLIRON_RA_GREEDY_DUMP") {
                            std::eprintln!(
                                "ra2x: edge scratch on stack: inst {} {:?} forbidden={} avail={}",
                                inst_idx,
                                class_for_scratch,
                                forbidden.len(),
                                avail.len()
                            );
                        }
                        let slot = Allocation::stack(SpillSlot::new(scratch_slots));
                        scratch_tags.push((scratch_slots, class_for_scratch));
                        scratch_slots += 1;
                        slot
                    },
                    is_stack_alloc: |a: Allocation| is_stack_like(env, a),
                    borrowed_scratch_reg: regs_for_class(env, class_for_scratch)
                        .into_iter()
                        .next()
                        .unwrap_or(PReg::new(0, class_for_scratch)),
                };
                // Forward order: MoveAndScratchResolver emits in execution order
                // (fastalloc/ion `.rev()` because they prepend; we append).
                let seq = resolver.compute(resolved);
                num_spillslots = scratch_slots;
                // PLIRON: ion's edge placement (head of a single-predecessor
                // successor); a call's own results are defined by then.
                let pts = [edge_point(func, block, succ).unwrap_or((ProgPoint::before(inst), false))];
                for (from, to, _v) in seq {
                    // Unlabelled: the checker's edge ParallelMove already pairs
                    // arg↔param; a label on a scratch save is often wrong.
                    push_move_pts(
                        &pts,
                        &mut in_edge,
                        env,
                        intervals,
                        assign,
                        inst_idx,
                        &mut edits,
                        &mut edit_vregs,
                        from,
                        to,
                        None,
                        class_for_scratch,
                        &mut forbidden,
                        prefer_of(class_for_scratch),
                    )?;
                }
                }
            }
        }

        allocs.extend(op_alloc);
    }

    // In-edge work first among the edits at one program point.
    let mut edge_flag = vec![false; edits.len()];
    for &(a, b) in &in_edge {
        edge_flag[a..b].iter_mut().for_each(|f| *f = true);
    }
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| (edits[i].0.to_index(), !edge_flag[i]));
    let edits: Vec<_> = order.iter().map(|&i| edits[i].clone()).collect();
    let edit_vregs: Vec<_> = order.iter().map(|&i| edit_vregs[i]).collect();

    Ok((allocs, inst_alloc_offsets, edits, edit_vregs, num_spillslots, scratch_tags))
}

pub fn run<F: Function>(
    func: &F,
    env: &MachineEnv,
    _verbose: bool,
    check_ssa: bool,
) -> Result<Output, RegAllocError> {
    run_inner(func, env, check_ssa, false)?.ok_or(RegAllocError::TooManyLiveRegs)
}

/// PLIRON: greedy hands out one logical slot per spill; a vector value
/// needs `spillslot_size` aligned units (Cranelift: 16 bytes = 2), and ion
/// names a multi-unit slot as that class's size and alignment require.
/// Renumber every stack allocation accordingly (a v128 spill next to an
/// 8-byte one overwrote its neighbour: `contains_rng`).
#[allow(clippy::too_many_arguments)]
fn renumber_slots<F: Function>(
    func: &F,
    intervals: &[Interval],
    assign: &[Option<Allocation>],
    scratch_tags: &[(usize, RegClass)],
    allocs: &mut [Allocation],
    edits: &mut [(ProgPoint, Edit)],
    n_old: usize,
) -> usize {
    let mut tags: Vec<Option<RegClass>> = vec![None; n_old];
    for iv in intervals {
        if let Some(Some(a)) = assign.get(iv.vreg.vreg()) {
            if let Some(sl) = a.as_stack() {
                if let Some(t) = tags.get_mut(sl.index()) {
                    *t = Some(iv.class);
                }
            }
        }
    }
    for &(i, c) in scratch_tags {
        if let Some(t) = tags.get_mut(i) {
            *t = Some(c);
        }
    }
    // Only slots something touches get frame space: colouring turns most of
    // the all-spill baseline's slots into registers, and a slot per vreg ever
    // created made a 2.7 KiB frame for 25 real spills.
    let mut used = vec![false; n_old];
    for a in allocs.iter() {
        if let Some(sl) = a.as_stack() {
            if let Some(u) = used.get_mut(sl.index()) {
                *u = true;
            }
        }
    }
    for (_, e) in edits.iter() {
        let Edit::Move { from, to } = e;
        for a in [from, to] {
            if let Some(sl) = a.as_stack() {
                if let Some(u) = used.get_mut(sl.index()) {
                    *u = true;
                }
            }
        }
    }
    let by_last = func.multi_spillslot_named_by_last_slot();
    let mut map = vec![0usize; n_old];
    let mut cur = 0usize;
    for i in 0..n_old {
        if !used[i] {
            continue;
        }
        let size = func.spillslot_size(tags[i].unwrap_or(RegClass::Int)).max(1);
        let off = cur.div_ceil(size) * size;
        map[i] = if by_last { off + size - 1 } else { off };
        cur = off + size;
    }
    let fix = |a: &mut Allocation| {
        if let Some(sl) = a.as_stack() {
            if let Some(&n) = map.get(sl.index()) {
                *a = Allocation::stack(SpillSlot::new(n));
            }
        }
    };
    allocs.iter_mut().for_each(fix);
    for (_, e) in edits.iter_mut() {
        let Edit::Move { from, to } = e;
        fix(from);
        fix(to);
    }
    cur
}

/// PLIRON: content hash of a function (instruction count, operand bits), for
/// bisecting colouring by function (`PLIRON_RA_COLOR_FNS`).
#[cfg(feature = "std")]
pub fn fn_hash<F: Function>(func: &F) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |x: u64| {
        h = (h ^ x).wrapping_mul(0x0100_0000_01b3);
        h ^= h >> 29;
    };
    mix(func.num_insts() as u64);
    mix(func.num_vregs() as u64);
    for i in 0..func.num_insts() {
        for op in func.inst_operands(Inst::new(i)) {
            mix(op.bits() as u64);
        }
    }
    h
}

/// PLIRON: `PLIRON_RA_COLOR_FNS=lo..hi` (per-mille of the hash space, `0..1000`)
/// limits colouring (greedy COLOR / MCTS) to the functions whose hash lands in
/// that range, the rest allocate all-spill. Halve it to find a function that
/// miscompiles; deterministic across processes.
fn color_allowed<F: Function>(func: &F) -> bool {
    #[cfg(feature = "std")]
    if let Ok(v) = std::env::var("PLIRON_RA_COLOR_FNS") {
        if let Some((lo, hi)) = v.split_once("..") {
            if let (Ok(lo), Ok(hi)) = (lo.trim().parse::<u64>(), hi.trim().parse::<u64>()) {
                let m = fn_hash(func) % 1000;
                return lo <= m && m < hi;
            }
        }
    }
    let _ = func;
    true
}

/// Like `run`, but forces the MCTS home search (`PLIRON_RA_ALGO=mcts`).
/// `None`: the whole-function homes would spill too much (see
/// `PLIRON_RA_MCTS_FALLBACK`); the caller runs ion on this function.
pub fn run_mcts<F: Function>(
    func: &F,
    env: &MachineEnv,
    _verbose: bool,
    check_ssa: bool,
) -> Result<Option<Output>, RegAllocError> {
    run_inner(func, env, check_ssa, true)
}

fn run_inner<F: Function>(
    func: &F,
    env: &MachineEnv,
    check_ssa: bool,
    force_mcts: bool,
) -> Result<Option<Output>, RegAllocError> {
    if check_ssa {
        validate_ssa(func, &CFGInfo::new(func)?)?;
    }

    #[cfg(feature = "std")]
    let t0 = std::time::Instant::now();
    let intervals = build_intervals(func, env);
    let nv = func.num_vregs();
    #[cfg(feature = "std")]
    let t_iv = t0.elapsed();
    let lv = build_liveness(func, nv);
    #[cfg(feature = "std")]
    let t_lv = t0.elapsed();
    let interfere = build_interfere(func, nv, &lv);
    #[cfg(feature = "std")]
    let t_if = t0.elapsed();
    // PLIRON: `PLIRON_RA_COLOR_MULTI=0` keeps colouring to single-block
    // vregs (and the linear clobber windows); default is whole-function
    // homes with liveness-exact forbid sets.
    let forbid = if !env_off("PLIRON_RA_COLOR_MULTI") { build_forbid(func, &lv, nv) } else { None };
    // PLIRON: a colouring can leave emit no free temp (`TooManyLiveRegs`)
    // at the busiest instruction. Retry with more registers held back from
    // homes before giving up to the all-spill baseline (which always fits).
    #[cfg(feature = "std")]
    let t_fb = t0.elapsed();
    let reserve0 = env_usize("PLIRON_RA_MCTS_RESERVE", 1);
    // Whole-function homes cannot split a live range, which is what ion does
    // when pressure exceeds the registers: spilling a vreg that is used in a
    // loop costs a reload per iteration. Past this much weighted spill cost
    // the function goes back to ion (`run_mcts` returns `None`).
    let fallback_at = env_usize("PLIRON_RA_MCTS_FALLBACK", 50) as f64;
    let mut emitted = None;
    let mut assign = Vec::new();
    for extra in [0usize, 1, 3, 6] {
        let mut spilled = 0.0f64;
        let (mut a, spillslots, prefs) = color(
            func,
            &intervals,
            &interfere,
            env,
            force_mcts,
            color_allowed(func),
            forbid.as_deref(),
            reserve0 + extra,
            &mut spilled,
        );
        if force_mcts && forbid.is_some() && fallback_at > 0.0 && spilled > fallback_at {
            #[cfg(feature = "std")]
            if env_flag("PLIRON_RA_TIME") && t0.elapsed().as_millis() >= 50 {
                std::eprintln!(
                    "ra2time: DELEGATED insts={} vregs={} intervals={:?} liveness={:?} interfere={:?} forbid={:?} colour={:?}",
                    func.num_insts(), nv, t_iv, t_lv - t_iv, t_if - t_lv, t_fb - t_if, t0.elapsed() - t_fb
                );
            }
            return Ok(None);
        }
        match emit(func, env, &intervals, &mut a, spillslots, &prefs, &lv, nv) {
            Err(RegAllocError::TooManyLiveRegs) if forbid.is_some() && extra < 6 => continue,
            Err(RegAllocError::TooManyLiveRegs) => {
                if force_mcts && fallback_at > 0.0 {
                    #[cfg(feature = "std")]
            if env_flag("PLIRON_RA_TIME") && t0.elapsed().as_millis() >= 50 {
                std::eprintln!(
                    "ra2time: DELEGATED insts={} vregs={} intervals={:?} liveness={:?} interfere={:?} forbid={:?} colour={:?}",
                    func.num_insts(), nv, t_iv, t_lv - t_iv, t_if - t_lv, t_fb - t_if, t0.elapsed() - t_fb
                );
            }
            return Ok(None);
                }
                let mut sp = 0.0;
                let (mut a2, spillslots, prefs) =
                    color(func, &intervals, &interfere, env, force_mcts, false, None, 0, &mut sp);
                let r = emit(func, env, &intervals, &mut a2, spillslots, &prefs, &lv, nv)?;
                assign = a2;
                emitted = Some(r);
                break;
            }
            r => {
                let r = r?;
                assign = a;
                emitted = Some(r);
                break;
            }
        }
    }
    let (mut allocs, inst_alloc_offsets, mut edits, edit_vregs, num_slots, scratch_tags) =
        emitted.expect("greedy emit");
    #[cfg(feature = "std")]
    if env_flag("PLIRON_RA_TIME") && t0.elapsed().as_millis() >= 50 {
        std::eprintln!(
            "ra2time: insts={} vregs={} intervals={:?} liveness={:?} interfere={:?} forbid={:?} colour+emit={:?}",
            func.num_insts(),
            nv,
            t_iv,
            t_lv - t_iv,
            t_if - t_lv,
            t_fb - t_if,
            t0.elapsed() - t_fb
        );
    }
    let num_spillslots = renumber_slots(
        func,
        &intervals,
        &assign,
        &scratch_tags,
        &mut allocs,
        &mut edits,
        num_slots,
    );

    let n_edits = edits.len();
    #[cfg(feature = "std")]
    if std::env::var("PLIRON_RA_GREEDY_DUMP").is_ok() {
        for (pp, e) in edits.iter() {
            std::eprintln!("greedy-edit {:?} {:?}", pp, e);
        }
        for (i, a) in allocs.iter().enumerate() {
            std::eprintln!("greedy-alloc[{}] {:?}", i, a);
        }
    }
    let mut output = Output {
        num_spillslots,
        edits,
        edit_vregs,
        allocs,
        inst_alloc_offsets,
        debug_locations: Vec::new(),
        stats: Stats::default(),
    };
    output.stats.final_bundle_count = intervals.len();
    output.stats.spill_bundle_count = num_spillslots;

    if crate::explain_on() || crate::ra_stats_on() {
        #[cfg(feature = "std")]
        {
            std::eprintln!(
                "ra2stats: algo=greedy intervals={} spills={} edits={}",
                intervals.len(),
                num_spillslots,
                n_edits
            );
        }
    }

    Ok(Some(output))
}

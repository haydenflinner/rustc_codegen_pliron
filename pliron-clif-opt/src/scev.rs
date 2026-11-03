//! SCEV-lite (W2 of `docs/codegen-plan.md`): one analysis of a loop's
//! induction variables that the IV passes can share instead of each
//! matching its own shape.
//!
//! Per loop: every header param is classified as an add recurrence
//! `{entry, +, step}` (constant or loop-invariant step, the same on every
//! latch), invariant (passed back unchanged), or other. Every exit `brif`
//! on an `icmp` whose sides are linear forms over header params and
//! invariants becomes an [`ExitTest`] with the net step `d` of
//! `lhs - rhs` per iteration, from which [`LoopScev::trip`] gives a
//! symbolic trip count for the unit-step shapes.
//!
//! Analysis only; the `scev` step (`PLIRON_SCEV=1`) runs it over every
//! loop and reports a census at `PLIRON_TRACE=pliron::scev` (same-step IV
//! pairs, multi-exit loops with affine tests, exits with a trip count)
//! so consumers are built on evidence. Linear forms are 64-bit wrapping
//! sums (`ivptr::lin`), so a trip count is exact only in the ring; the
//! consumers add the guards (`a0 <= n`) that make it a real count.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Type, Value, ValueDef};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::FxHashSet;

use crate::ivptr::{Lin, lin};
use crate::loopidiom::{Edge, def_block, edge_args};
use crate::pipeline::{Analysis, CfgDtLa};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    K(i64),
    /// A loop-invariant value.
    V(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rec {
    /// Passed back unchanged on every latch.
    Inv,
    /// `p' = p + step` on every latch.
    Add(Step),
    Other,
}

pub struct ExitTest {
    pub inst: Inst,
    /// Arm of `inst` that leaves the loop.
    pub slot: usize,
    /// `lhs stay rhs` holds while the loop continues. Both sides are linear
    /// forms whose leaves are header params or invariants.
    pub lhs: Lin,
    pub rhs: Lin,
    pub stay: IntCC,
    pub ty: Type,
    /// Net change of `lhs - rhs` per iteration when every stepping param
    /// in it has a constant step.
    pub d: Option<i64>,
    /// The stepping params `lhs - rhs` reads (indices into `params`).
    pub ivs: Vec<usize>,
}

/// Symbolic trip count of one exit: the loop leaves through it after
/// `count` iterations, where `count` is a linear form over invariants and
/// the entry values of the params, valid when `guard` holds at entry
/// (`None`: unconditionally, e.g. an `ne` test that is exact in the ring).
pub struct Trip {
    pub count: Lin,
    pub guard: Option<(Lin, IntCC, Lin)>,
}

pub struct LoopScev {
    pub lp: Loop,
    pub h: Block,
    pub region: FxHashSet<Block>,
    pub entries: Vec<Edge>,
    pub latches: Vec<Edge>,
    pub params: Vec<Value>,
    pub recs: Vec<Rec>,
    /// Exits whose test is affine, in region block order.
    pub exits: Vec<ExitTest>,
    /// All edges leaving the loop.
    pub n_exits: usize,
}

impl LoopScev {
    pub fn is_invariant(&self, func: &Function, dt: &DominatorTree, v: Value) -> bool {
        let v = func.dfg.resolve_aliases(v);
        if let ValueDef::Param(b, i) = func.dfg.value_def(v)
            && b == self.h
        {
            return matches!(self.recs[i], Rec::Inv);
        }
        match def_block(func, v) {
            Some(b) => !self.region.contains(&b) && dt.block_dominates(b, self.h),
            None => false,
        }
    }

    fn param_index(&self, v: Value) -> Option<usize> {
        self.params.iter().position(|&p| p == v)
    }

    /// `v` as a linear form over stepping params and invariants, or `None`
    /// when it reads something else (an in-loop non-affine value).
    pub fn affine(&self, func: &Function, dt: &DominatorTree, v: Value) -> Option<Lin> {
        let f = lin(func, v, 0);
        for &(l, _) in &f.t {
            if let Some(i) = self.param_index(l) {
                if matches!(self.recs[i], Rec::Other) {
                    return None;
                }
            } else if !self.is_invariant(func, dt, l) {
                return None;
            }
        }
        Some(f)
    }

    /// Net step of a linear form per iteration (constant steps only).
    pub fn step_of(&self, f: &Lin) -> Option<i64> {
        let mut d = 0i64;
        for &(l, c) in &f.t {
            if let Some(i) = self.param_index(l) {
                match self.recs[i] {
                    Rec::Add(Step::K(s)) => d = d.wrapping_add(c.wrapping_mul(s)),
                    Rec::Add(Step::V(_)) => return None,
                    Rec::Inv | Rec::Other => {}
                }
            }
        }
        Some(d)
    }

    /// Entry value of a linear form: header params replaced by their
    /// (single) entry args. `None` with several entry edges.
    pub fn at_entry(&self, func: &Function, f: &Lin) -> Option<Lin> {
        let [e] = self.entries[..] else { return None };
        let args = edge_args(func, e);
        let mut out = Lin { k: f.k, t: Vec::new() };
        for &(l, c) in &f.t {
            let v = match self.param_index(l) {
                Some(i) => func.dfg.resolve_aliases(*args.get(i)?),
                None => l,
            };
            out = out.add(&Lin { k: 0, t: vec![(v, 1)] }.scale_pub(c), 1);
        }
        Some(out)
    }

    /// Trip count of an exit with net step ±1 against an invariant bound,
    /// as the number of iterations the loop runs before leaving through
    /// it (the header is reached `count + 1` times for a header-tested
    /// loop). `a0` is the entry value of `lhs - rhs`.
    pub fn trip(&self, func: &Function, e: &ExitTest) -> Option<Trip> {
        let d = e.d?;
        let diff = e.lhs.add(&e.rhs, -1);
        let a0 = self.at_entry(func, &diff)?;
        let zero = Lin { k: 0, t: Vec::new() };
        // `diff` goes a0, a0+d, ...; the loop continues while `diff stay 0`.
        let (count, guard) = match (d, e.stay) {
            // While a < 0 (i.e. lhs < rhs), stepping up: count = -a0.
            (1, IntCC::UnsignedLessThan | IntCC::SignedLessThan) => {
                let neg = zero.add(&a0, -1);
                (neg, Some((a0.clone(), e.stay, zero)))
            }
            (1, IntCC::UnsignedLessThanOrEqual | IntCC::SignedLessThanOrEqual) => {
                let neg1 = zero.add(&a0, -1).add(&Lin { k: 1, t: Vec::new() }, 1);
                (neg1, Some((a0.clone(), e.stay, zero)))
            }
            (1, IntCC::NotEqual) => (zero.add(&a0, -1), None),
            (-1, IntCC::UnsignedGreaterThan | IntCC::SignedGreaterThan) => {
                (a0.clone(), Some((a0.clone(), e.stay, zero)))
            }
            (-1, IntCC::UnsignedGreaterThanOrEqual | IntCC::SignedGreaterThanOrEqual) => {
                (a0.add(&Lin { k: 1, t: Vec::new() }, 1), Some((a0.clone(), e.stay, zero)))
            }
            (-1, IntCC::NotEqual) => (a0.clone(), None),
            _ => return None,
        };
        Some(Trip { count, guard })
    }
}

impl Lin {
    pub(crate) fn scale_pub(&self, s: i64) -> Lin {
        Lin { k: self.k.wrapping_mul(s), t: self.t.iter().map(|&(v, c)| (v, c.wrapping_mul(s))).collect() }
    }
}

/// Analyze one loop. `None` when the header has no latch or an entry edge
/// without a plain value list.
pub fn analyze(func: &Function, cfg: &ControlFlowGraph, dt: &DominatorTree, la: &LoopAnalysis, lp: Loop) -> Option<LoopScev> {
    let h = la.loop_header(lp);
    if !dt.is_reachable(h) {
        return None;
    }
    let region: FxHashSet<Block> = func.layout.blocks().filter(|&b| la.is_in_loop(b, lp)).collect();
    let mut entries = Vec::new();
    let mut latches = Vec::new();
    for p in cfg.pred_iter(h) {
        let dests = func.dfg.insts[p.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        for (slot, bc) in dests.iter().enumerate() {
            if bc.block(&func.dfg.value_lists) != h {
                continue;
            }
            let e = Edge { inst: p.inst, slot };
            if region.contains(&p.block) {
                latches.push(e);
            } else {
                entries.push(e);
            }
        }
    }
    if latches.is_empty() || entries.is_empty() {
        return None;
    }
    let params: Vec<Value> = func.dfg.block_params(h).to_vec();
    let mut sc = LoopScev { lp, h, region, entries, latches, params: params.clone(), recs: vec![Rec::Other; params.len()], exits: Vec::new(), n_exits: 0 };
    // Invariant params first (a step may be another param that is Inv).
    let latch_args: Vec<Vec<Value>> = sc.latches.iter().map(|&e| edge_args(func, e)).collect();
    if latch_args.iter().any(|a| a.len() != params.len()) {
        return None;
    }
    for (i, &p) in params.iter().enumerate() {
        if latch_args.iter().all(|a| func.dfg.resolve_aliases(a[i]) == p) {
            sc.recs[i] = Rec::Inv;
        }
    }
    for (i, &p) in params.iter().enumerate() {
        if matches!(sc.recs[i], Rec::Inv) {
            continue;
        }
        let mut step: Option<Step> = None;
        let mut ok = true;
        for a in &latch_args {
            let v = crate::bittab::canon(func, cfg, a[i]);
            let f = lin(func, v, 0);
            // f = p + k, or p + x with x invariant.
            let s = match f.t.as_slice() {
                [(l, 1)] if *l == p => Some(Step::K(f.k)),
                [(l1, 1), (l2, 1)] if f.k == 0 && (*l1 == p || *l2 == p) => {
                    let x = if *l1 == p { *l2 } else { *l1 };
                    sc.is_invariant(func, dt, x).then_some(Step::V(x))
                }
                _ => None,
            };
            match (step, s) {
                (_, None) => {
                    ok = false;
                    break;
                }
                (None, Some(s)) => step = Some(s),
                (Some(s0), Some(s)) if s0 == s => {}
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && let Some(s) = step && s != Step::K(0) {
            sc.recs[i] = Rec::Add(s);
        }
    }
    // Exits.
    let mut blocks: Vec<Block> = sc.region.iter().copied().collect();
    blocks.sort_unstable_by_key(|b| b.as_u32());
    for &b in &blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let dests = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        let out: Vec<usize> = (0..dests.len()).filter(|&s| !sc.region.contains(&dests[s].block(&func.dfg.value_lists))).collect();
        sc.n_exits += out.len();
        let [es] = out[..] else { continue };
        let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else { continue };
        let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst() else { continue };
        let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[ci] else { continue };
        let (x, y) = (crate::bittab::canon(func, cfg, args[0]), crate::bittab::canon(func, cfg, args[1]));
        let (Some(lhs), Some(rhs)) = (sc.affine(func, dt, x), sc.affine(func, dt, y)) else { continue };
        let stay = if es == 0 { cond.complement() } else { cond };
        let diff = lhs.add(&rhs, -1);
        let d = sc.step_of(&diff);
        let ivs: Vec<usize> = diff.t.iter().filter_map(|&(l, _)| sc.param_index(l)).filter(|&i| matches!(sc.recs[i], Rec::Add(_))).collect();
        sc.exits.push(ExitTest { inst: t, slot: es, lhs, rhs, stay, ty: func.dfg.value_type(x), d, ivs });
    }
    Some(sc)
}

/// Census step: analyze every loop and report what the consumers could
/// use. Never mutates.
pub fn census(func: &Function, an: &mut Analysis) -> usize {
    let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
    let on = pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::scev", tracing::Level::INFO);
    let mut found = 0;
    for lp in la.loops() {
        let Some(sc) = analyze(func, cfg, dt, la, lp) else { continue };
        let steps: Vec<String> = sc.recs.iter().map(|r| match r {
            Rec::Inv => "inv".to_string(),
            Rec::Add(Step::K(k)) => format!("+{k}"),
            Rec::Add(Step::V(v)) => format!("+{v}"),
            Rec::Other => "?".to_string(),
        }).collect();
        // Same-step pairs (a redundant IV once one is expressed from the other).
        let mut pairs = 0;
        for i in 0..sc.recs.len() {
            for j in i + 1..sc.recs.len() {
                if let (Rec::Add(a), Rec::Add(b)) = (sc.recs[i], sc.recs[j])
                    && a == b
                    && func.dfg.value_type(sc.params[i]) == func.dfg.value_type(sc.params[j])
                {
                    pairs += 1;
                }
            }
        }
        let trips = sc.exits.iter().filter(|e| sc.trip(func, e).is_some()).count();
        let distinct_ivs: FxHashSet<&usize> = sc.exits.iter().flat_map(|e| e.ivs.iter()).collect();
        found += 1;
        if on {
            tracing::info!(
                target: "pliron::scev",
                "{}: loop {} entries={} latches={} params=[{}] exits={} affine={} trips={} same_step_pairs={} exit_ivs={} depth={}",
                func.name,
                sc.h,
                sc.entries.len(),
                sc.latches.len(),
                steps.join(","),
                sc.n_exits,
                sc.exits.len(),
                trips,
                pairs,
                distinct_ivs.len(),
                la.loop_level(sc.h).level(),
            );
        }
    }
    let _ = found;
    0
}

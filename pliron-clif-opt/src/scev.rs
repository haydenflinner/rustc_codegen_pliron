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
        let dump = on && std::env::var("PLIRON_SCEV_DUMP").as_deref() == Ok("1");
        for i in 0..sc.recs.len() {
            for j in i + 1..sc.recs.len() {
                if let (Rec::Add(a), Rec::Add(b)) = (sc.recs[i], sc.recs[j])
                    && a == b
                    && func.dfg.value_type(sc.params[i]) == func.dfg.value_type(sc.params[j])
                {
                    pairs += 1;
                    if dump && let [e] = sc.entries[..] {
                        let args = edge_args(func, e);
                        let show = |v: Value| -> String {
                            let v = func.dfg.resolve_aliases(v);
                            match func.dfg.value_def(v).inst() {
                                Some(i) => format!("{v} = {}", func.dfg.display_inst(i)),
                                None => format!("{v} (param)"),
                            }
                        };
                        let uses = |p: Value| -> String {
                            let mut kinds: Vec<String> = Vec::new();
                            for b in &sc.region {
                                for inst in func.layout.block_insts(*b) {
                                    if func.dfg.inst_args(inst).iter().any(|&a| func.dfg.resolve_aliases(a) == p) {
                                        kinds.push(format!("{}", func.dfg.insts[inst].opcode()));
                                    }
                                }
                            }
                            kinds.sort();
                            kinds.dedup();
                            kinds.join("/")
                        };
                        tracing::info!(
                            target: "pliron::scev",
                            "{}: pair {} {} step {:?}: entry {} | {} ; uses {} | {}",
                            func.name, sc.params[i], sc.params[j], a,
                            args.get(i).map(|&v| show(v)).unwrap_or_default(),
                            args.get(j).map(|&v| show(v)).unwrap_or_default(),
                            uses(sc.params[i]), uses(sc.params[j]),
                        );
                    }
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

/// Exit-value replacement (`PLIRON_IVEXIT=1`, LLVM's
/// `rewriteLoopExitValues` for the add-recurrence case): a header param
/// `q` that steps like a sibling `p` (same constant step) and has no
/// in-loop use but its own step is `p + (q0 - p0)` everywhere; its uses on
/// exit edges get that value computed once on the exit, and the
/// recurrence (one add and one register per iteration) goes. The census
/// found it in regex's `next_state` (an index counted beside the pointer
/// only to be returned) and in every `Vec::extend_with`.
pub fn exit_values(func: &mut Function, an: &mut Analysis) -> usize {
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::{BlockArg, InstBuilder, Opcode};
    let headers: Vec<Loop> = {
        let CfgDtLa { la, .. } = an.cfg_dt_la(func);
        la.loops().collect()
    };
    let mut total = 0;
    for lp in headers {
        // One slot per round: removing a param renumbers the rest.
        for _round in 0..8 {
            let plan = {
                let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
                let Some(sc) = analyze(func, cfg, dt, la, lp) else { break };
                plan_exit_value(func, &sc)
            };
            let Some(pl) = plan else { break };
            apply_exit_value(func, pl);
            total += 1;
        }
    }
    if total > 0 {
        an.invalidate(func);
    }
    total
}

struct ExitPlan {
    /// Slot of the redundant param `q` and the kept param `p`.
    qi: usize,
    q: Value,
    p: Value,
    /// `q`'s step instructions (`q + s`, one per latch path).
    sq_insts: Vec<Inst>,
    s: i64,
    /// Entry edge and the entry args of `q` and `p`.
    entry: Edge,
    q0: Value,
    p0: Value,
    /// Edges into `h` (entry + latches), to strip slot `qi`.
    into_h: Vec<Edge>,
    /// Forwarding params inside the loop that only carry `q`/`sq` on to
    /// the latch: (param, block, slot, edges into that block).
    forwards: Vec<(Value, Block, usize, Vec<Edge>)>,
    /// Exit edges whose args mention a forwarded value: (edge, slot,
    /// stepped?).
    exits: Vec<(Edge, usize, bool)>,
    /// Uses as instruction args outside the loop: (inst, arg index,
    /// stepped?).
    outside: Vec<(Inst, usize, bool)>,
}

fn why(func: &Function, q: Value, line: u32) {
    if pliron_cfg::tracing_on() {
        tracing::trace!(target: "pliron::scev", "{}: exit value {} rejected at scev.rs:{}", func.name, q, line);
    }
}

fn plan_exit_value(func: &Function, sc: &LoopScev) -> Option<ExitPlan> {
    use cranelift_codegen::ir::{BlockArg, Opcode};
    use rustc_hash::FxHashMap;
    let [entry] = sc.entries[..] else { return None };
    let entry_args = edge_args(func, entry);
    if entry_args.len() != sc.params.len() {
        return None;
    }
    let n = sc.params.len();
    let mut blocks: Vec<Block> = sc.region.iter().copied().collect();
    blocks.sort_unstable_by_key(|b| b.as_u32());
    'pairs: for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            let (Rec::Add(Step::K(s)), Rec::Add(Step::K(s2))) = (sc.recs[i], sc.recs[j]) else { continue };
            if s != s2 || func.dfg.value_type(sc.params[i]) != func.dfg.value_type(sc.params[j]) {
                continue;
            }
            let (p, q) = (sc.params[i], sc.params[j]);
            // Find q's step instructions (a stepped copy per latch path).
            let mut steps: Vec<(Inst, Value)> = Vec::new();
            for &b in &blocks {
                for inst in func.layout.block_insts(b) {
                    let is_step = match func.dfg.insts[inst] {
                        InstructionData::Binary { opcode: Opcode::Iadd, args: [x, y] } => {
                            let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
                            (x == q && crate::loopidiom::iconst(func, y) == Some(s)) || (y == q && crate::loopidiom::iconst(func, x) == Some(s))
                        }
                        InstructionData::Binary { opcode: Opcode::Isub, args: [x, y] } => {
                            func.dfg.resolve_aliases(x) == q && crate::loopidiom::iconst(func, y) == Some(s.wrapping_neg())
                        }
                        _ => false,
                    };
                    if is_step {
                        steps.push((inst, func.dfg.first_result(inst)));
                    }
                }
            }
            if steps.is_empty() {
                continue;
            }
            let step_insts: Vec<Inst> = steps.iter().map(|&(i, _)| i).collect();
            // Forwarded values: q (unstepped), sq (stepped), and in-loop
            // params that only carry them. Grown to a fixpoint, then every
            // use is validated.
            let mut fwd: FxHashMap<Value, bool> = FxHashMap::default();
            fwd.insert(q, false);
            for &(_, v) in &steps {
                fwd.insert(v, true);
            }
            let mut forwards: Vec<(Value, Block, usize, Vec<Edge>)> = Vec::new();
            loop {
                let mut grew = false;
                for &b in &blocks {
                    let Some(t) = func.layout.last_inst(b) else { continue };
                    let data = &func.dfg.insts[t];
                    if !matches!(data, InstructionData::Jump { .. } | InstructionData::Brif { .. }) {
                        continue;
                    }
                    let dests = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                    for bc in dests.iter() {
                        let dest = bc.block(&func.dfg.value_lists);
                        if dest == sc.h || !sc.region.contains(&dest) {
                            continue;
                        }
                        for (k, a) in bc.args(&func.dfg.value_lists).enumerate() {
                            let BlockArg::Value(v) = a else { continue };
                            let v = func.dfg.resolve_aliases(v);
                            if let Some(&st) = fwd.get(&v) {
                                let param = func.dfg.block_params(dest)[k];
                                if !fwd.contains_key(&param) {
                                    fwd.insert(param, st);
                                    forwards.push((param, dest, k, Vec::new()));
                                    grew = true;
                                }
                            }
                        }
                    }
                }
                if !grew {
                    break;
                }
            }
            // Validate: no in-loop instruction reads a forwarded value
            // (except q's step); every edge into a forwarder block passes
            // a forwarded value of the same stepped-ness in that slot;
            // the latch slot j and exits may read them.
            let mut exits = Vec::new();
            let mut into_h = Vec::new();
            for &b in &blocks {
                for inst in func.layout.block_insts(b) {
                    if !step_insts.contains(&inst) {
                        for &a in func.dfg.inst_args(inst) {
                            if fwd.contains_key(&func.dfg.resolve_aliases(a)) {
                                { why(func, q, line!()); continue 'pairs; }
                            }
                        }
                    }
                    let data = &func.dfg.insts[inst];
                    if !data.opcode().is_branch() {
                        continue;
                    }
                    let plain = matches!(data, InstructionData::Jump { .. } | InstructionData::Brif { .. });
                    let dests = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                    for (slot, bc) in dests.iter().enumerate() {
                        let dest = bc.block(&func.dfg.value_lists);
                        let args: Vec<Option<Value>> = bc
                            .args(&func.dfg.value_lists)
                            .map(|a| match a {
                                BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                                _ => None,
                            })
                            .collect();
                        let mentions = |k: usize| args.get(k).copied().flatten().and_then(|v| fwd.get(&v).copied());
                        if !plain {
                            if (0..args.len()).any(|k| mentions(k).is_some()) {
                                { why(func, q, line!()); continue 'pairs; }
                            }
                            continue;
                        }
                        if dest == sc.h {
                            if args.len() != n {
                                { why(func, q, line!()); continue 'pairs; }
                            }
                            for k in 0..n {
                                if k != j && mentions(k).is_some() {
                                    { why(func, q, line!()); continue 'pairs; }
                                }
                            }
                            into_h.push(Edge { inst, slot });
                        } else if sc.region.contains(&dest) {
                            for k in 0..args.len() {
                                let Some(st) = mentions(k) else { continue };
                                let param = func.dfg.block_params(dest)[k];
                                match fwd.get(&param) {
                                    Some(&pst) if pst == st => {}
                                    _ => continue 'pairs,
                                }
                                if let Some(f) = forwards.iter_mut().find(|f| f.0 == param) {
                                    f.3.push(Edge { inst, slot });
                                }
                            }
                        } else {
                            for k in 0..args.len() {
                                if let Some(st) = mentions(k) {
                                    exits.push((Edge { inst, slot }, k, st));
                                }
                            }
                        }
                    }
                }
            }
            // Every edge into a forwarder block must pass a forwarded value.
            for f in &forwards {
                let (param, dest, k, ref edges) = *f;
                let _ = param;
                let n_in = blocks
                    .iter()
                    .filter_map(|&b| func.layout.last_inst(b))
                    .map(|t| {
                        func.dfg.insts[t]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                            .iter()
                            .filter(|bc| bc.block(&func.dfg.value_lists) == dest)
                            .count()
                    })
                    .sum::<usize>();
                if edges.len() != n_in || k >= func.dfg.block_params(dest).len() {
                    { why(func, q, line!()); continue 'pairs; }
                }
            }
            // Entries into forwarder blocks from outside the loop would
            // need a value too: refuse (they can't exist for in-loop
            // blocks other than the header in a natural loop).
            into_h.push(entry);
            let mut outside = Vec::new();
            for b in func.layout.blocks() {
                if sc.region.contains(&b) {
                    continue;
                }
                for inst in func.layout.block_insts(b) {
                    for (k, &a) in func.dfg.inst_args(inst).iter().enumerate() {
                        if let Some(&st) = fwd.get(&func.dfg.resolve_aliases(a)) {
                            outside.push((inst, k, st));
                        }
                    }
                    let data = &func.dfg.insts[inst];
                    if data.opcode().is_branch() {
                        for bc in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                            for a in bc.args(&func.dfg.value_lists) {
                                if let BlockArg::Value(v) = a
                                    && fwd.contains_key(&func.dfg.resolve_aliases(v))
                                {
                                    { why(func, q, line!()); continue 'pairs; }
                                }
                            }
                        }
                    }
                }
            }
            return Some(ExitPlan {
                qi: j,
                q,
                p,
                sq_insts: step_insts,
                s,
                entry,
                q0: func.dfg.resolve_aliases(entry_args[j]),
                p0: func.dfg.resolve_aliases(entry_args[i]),
                into_h,
                forwards,
                exits,
                outside,
            });
        }
    }
    None
}

fn apply_exit_value(func: &mut Function, pl: ExitPlan) {
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::{BlockArg, InstBuilder};
    let ty = func.dfg.value_type(pl.q);
    // D = q0 - p0 in the entry edge's source block.
    let mut pos = FuncCursor::new(func).at_inst(pl.entry.inst);
    let d = pos.ins().isub(pl.q0, pl.p0);
    let value_at = |func: &mut Function, inst: Inst, stepped: bool| -> Value {
        let mut pos = FuncCursor::new(func).at_inst(inst);
        let t = pos.ins().iadd(pl.p, d);
        if stepped {
            let c = pos.ins().iconst(ty, pl.s);
            pos.ins().iadd(t, c)
        } else {
            t
        }
    };
    // Exit edges: replace the arg.
    for &(e, k, stepped) in &pl.exits {
        let t = value_at(func, e.inst, stepped);
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[e.inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.slot];
        let block = bc.block(&dfg.value_lists);
        let mut args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
        args[k] = BlockArg::Value(t);
        *bc = cranelift_codegen::ir::BlockCall::new(block, args.iter().copied(), &mut dfg.value_lists);
    }
    // Uses outside the loop.
    for &(inst, k, stepped) in &pl.outside {
        let t = value_at(func, inst, stepped);
        func.dfg.inst_args_mut(inst)[k] = t;
    }
    // Strip slot qi from every edge into h, the forwarder slots, the step
    // and the params. Slots are removed highest-first per block so the
    // indices stay valid.
    for e in &pl.into_h {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[e.inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.slot];
        bc.remove(pl.qi, &mut dfg.value_lists);
    }
    let mut fw: Vec<&(Value, Block, usize, Vec<Edge>)> = pl.forwards.iter().collect();
    fw.sort_by(|a, b| b.2.cmp(&a.2));
    for (param, _b, k, edges) in fw {
        for e in edges {
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[e.inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.slot];
            bc.remove(*k, &mut dfg.value_lists);
        }
        func.dfg.remove_block_param(*param);
    }
    for &i in &pl.sq_insts {
        func.dfg.clear_results(i);
        func.layout.remove_inst(i);
    }
    func.dfg.remove_block_param(pl.q);
    if pliron_cfg::tracing_on() {
        tracing::debug!(target: "pliron::scev", "{}: exit value {} = {} + ({} - {}) at {} exits, {} outside, {} forwards", func.name, pl.q, pl.p, pl.q0, pl.p0, pl.exits.len(), pl.outside.len(), pl.forwards.len());
    }
}

//! Counted pre-loops for multi-exit loops (`PLIRON_MINTRIP`).
//!
//! `SmallVec::extend(repeat(0).take(n))` fills with two exits per element
//! (`len < cap`, iterator not exhausted). LLVM folds them into one
//! `umin` trip count and vectorizes the fill; we unrolled with a test
//! per copy. Here a loop whose every exit is a `brif` on an affine test
//! gets a counted pre-loop:
//!
//! ```text
//! pre:   T = umin(T_e for each exit e);   jump Q(p0.., T)
//! Q(p.., c):  brif c == 0, H(p..), H'(p..)     // H' = clone of the loop
//! clone:      exits folded to their in-loop edge; back edge -> Q(p'.., c - 1)
//! ```
//!
//! then the original loop takes over for the iteration that exits. An
//! exit tests `a cc n` with `n` loop-invariant and `a` linear in header
//! params that step by constants on the single back edge, so `a` at
//! iteration `k` is `A0 + D*k` (`D = ±1` here). `T_e` only has to be a
//! lower bound on the iterations before exit `e` fires: `usub_sat(n, A0)`
//! for a continue test `a <u n` / `a <=u n` (`D = 1`), `usub_sat(A0, n)`
//! for `a >u n` / `a >=u n` (`D = -1`), and the exact wrapping `(n - A0)*D`
//! for `a != n`. The pre-loop has one exit, so loopidiom/loopvec can
//! take it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Type, Value, ValueDef};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ivptr::{Lin, lin};
use crate::pipeline::{Analysis, CfgDtLa};

struct Exit {
    /// The test's `a` as a linear form over header params and invariants.
    a: Lin,
    /// The bound, a linear form over invariants only (constants fold in).
    n: Lin,
    /// Continue condition `a cc n`.
    cc: IntCC,
    d: i64,
}

struct Plan {
    h: Block,
    entry: (Inst, usize),
    back: (Inst, usize),
    region: FxHashSet<Block>,
    fold: FxHashMap<Inst, usize>,
    exits: Vec<Exit>,
    ty: Type,
    /// Every header param's constant step, when all of them have one: the
    /// pre-loop's exit then rebuilds `p0 + s*T` itself and carries no
    /// args (loopidiom wants loop-invariant exit args).
    steps: Option<Vec<i64>>,
    /// A header param stepping by +1: the pre-loop's exit tests it
    /// against `p0 + T` (the counted shape loopvec/loopidiom know)
    /// instead of carrying a counter of its own.
    unit: Option<usize>,
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

fn edge_args(func: &Function, inst: Inst, slot: usize) -> Option<Vec<Value>> {
    let dests = func.dfg.insts[inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
    dests[slot]
        .args(&func.dfg.value_lists)
        .map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect()
}

macro_rules! bail {
    ($($t:tt)*) => {{
        if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::mintrip", tracing::Level::DEBUG) {
            tracing::debug!(target: "pliron::mintrip", "mintrip bail: {}", format_args!($($t)*));
        }
        return None;
    }};
}

fn plan(func: &Function, cfg: &cranelift_codegen::flowgraph::ControlFlowGraph, dt: &cranelift_codegen::dominator_tree::DominatorTree, la: &cranelift_codegen::loop_analysis::LoopAnalysis, lp: cranelift_codegen::loop_analysis::Loop) -> Option<Plan> {
    let h = la.loop_header(lp);
    let region: FxHashSet<Block> = func.layout.blocks().filter(|&b| la.is_in_loop(b, lp)).collect();
    // One entry edge, one back edge.
    let (mut entry, mut back) = (None, None);
    for p in cfg.pred_iter(h) {
        let dests = func.dfg.insts[p.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        for (s, bc) in dests.iter().enumerate() {
            if bc.block(&func.dfg.value_lists) != h {
                continue;
            }
            let slot = if region.contains(&p.block) { &mut back } else { &mut entry };
            if slot.replace((p.inst, s)).is_some() {
                bail!("{h}: several entry or back edges");
            }
        }
    }
    let (entry, back) = (entry?, back?);
    if !matches!(func.dfg.insts[back.0], InstructionData::Jump { .. } | InstructionData::Brif { .. })
        || !matches!(func.dfg.insts[entry.0], InstructionData::Jump { .. } | InstructionData::Brif { .. })
    {
        bail!("{h}: entry/latch terminator");
    }
    let params: Vec<Value> = func.dfg.block_params(h).to_vec();
    let bargs = edge_args(func, back.0, back.1)?;
    edge_args(func, entry.0, entry.1)?;
    // Header params stepping by a constant: p' = p + s.
    let mut step: FxHashMap<Value, i64> = FxHashMap::default();
    for (k, &p) in params.iter().enumerate() {
        let f = lin(func, crate::bittab::canon(func, cfg, bargs[k]), 0);
        if f.t == [(p, 1)] {
            step.insert(p, f.k);
        }
    }
    let invariant = |v: Value| def_block(func, v).is_some_and(|b| !region.contains(&b) && dt.block_dominates(b, h));
    // Every edge leaving the region is one arm of a `brif` on an affine test.
    let mut fold = FxHashMap::default();
    let mut exits = Vec::new();
    let mut ty = None;
    for &b in &region {
        let t = func.layout.last_inst(b)?;
        let dests = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        let out: Vec<usize> = (0..dests.len()).filter(|&s| !region.contains(&dests[s].block(&func.dfg.value_lists))).collect();
        if out.is_empty() {
            continue;
        }
        let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else { bail!("{h}: exit not a brif") };
        let [es] = out[..] else { bail!("{h}: both arms exit") };
        let ci = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst()?;
        let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[ci] else { bail!("{h}: exit test not an icmp") };
        let (x, y) = (crate::bittab::canon(func, cfg, args[0]), crate::bittab::canon(func, cfg, args[1]));
        // `a cc n` with `n` invariant; the exit is the true arm when es == 0.
        let inv_lin = |v: Value| {
            let f = lin(func, v, 0);
            f.t.iter().all(|&(l, _)| invariant(l)).then_some(f)
        };
        let (a, n, cc) = match (inv_lin(x), inv_lin(y)) {
            (None, Some(n)) => (x, n, cond),
            (Some(n), None) => (y, n, cond.swap_args()),
            _ => bail!("{h}: no invariant side"),
        };
        let cc = if es == 0 { cc.complement() } else { cc };
        let f = lin(func, a, 0);
        let mut d = 0i64;
        for &(v, c) in &f.t {
            if let Some(&s) = step.get(&v) {
                d = d.wrapping_add(c.wrapping_mul(s));
            } else if !invariant(v) {
                bail!("{h}: test {a} reads {v}, neither a stepping param nor invariant");
            }
        }
        let ok = matches!((d, cc), (1, IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual | IntCC::NotEqual) | (-1, IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual | IntCC::NotEqual));
        if !ok {
            bail!("{h}: step {d} with {cc:?}");
        }
        // i64 only: linear forms are 64-bit wrapping sums.
        let t_ty = func.dfg.value_type(a);
        if t_ty != cranelift_codegen::ir::types::I64 || *ty.get_or_insert(t_ty) != t_ty {
            bail!("{h}: test type");
        }
        fold.insert(t, 1 - es);
        exits.push(Exit { a: f, n, cc, d });
    }
    // The clone's latch must come out as a plain `jump` to the header.
    if !matches!(func.dfg.insts[back.0], InstructionData::Jump { .. }) && !fold.contains_key(&back.0) {
        bail!("latch");
    }
    if exits.len() < 2 {
        bail!("{h}: fewer than two exits");
    }
    let steps: Option<Vec<i64>> = params.iter().map(|p| step.get(p).copied()).collect();
    let steps = steps.filter(|_| params.iter().all(|&p| func.dfg.value_type(p) == cranelift_codegen::ir::types::I64));
    let unit = params.iter().position(|p| step.get(p) == Some(&1) && func.dfg.value_type(*p) == cranelift_codegen::ir::types::I64);
    Some(Plan { h, entry, back, region, fold, exits, ty: ty?, steps, unit })
}

/// Emit `f` at `pos` with header params replaced by the entry args.
fn emit_lin(pos: &mut FuncCursor, f: &Lin, sub: &FxHashMap<Value, Value>, ty: Type) -> Value {
    let mut acc = pos.ins().iconst(ty, f.k);
    for &(v, c) in &f.t {
        let v = sub.get(&v).copied().unwrap_or(v);
        let term = match c {
            1 => v,
            -1 => pos.ins().ineg(v),
            _ => {
                let k = pos.ins().iconst(ty, c);
                pos.ins().imul(v, k)
            }
        };
        acc = pos.ins().iadd(acc, term);
    }
    acc
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let plans: Vec<Plan> = {
        let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        // Innermost loops only: a clone of an outer loop would copy its
        // inner loops too.
        la.loops()
            .filter(|&lp| la.loops().all(|c| la.loop_parent(c) != Some(lp)))
            .filter_map(|lp| plan(func, cfg, dt, la, lp))
            .filter(|_| pliron_cfg::bisect("mintrip"))
            .collect()
    };
    let n = plans.len();
    for p in plans {
        let params: Vec<Value> = func.dfg.block_params(p.h).to_vec();
        let entry_args = edge_args(func, p.entry.0, p.entry.1).unwrap();
        let sub: FxHashMap<Value, Value> = params.iter().copied().zip(entry_args.iter().copied()).collect();
        // pre (on the entry edge): T, then the guard into the clone.
        let pre = func.dfg.make_block();
        func.layout.insert_block(pre, p.h);
        let bmap = crate::bcheck::clone_region(func, p.h, &p.region, &p.fold);
        let nh = bmap[&p.h];
        let c = p.unit.is_none().then(|| func.dfg.append_block_param(nh, p.ty));
        let mut pos = FuncCursor::new(func).at_bottom(pre);
        let mut t: Option<Value> = None;
        for e in &p.exits {
            let a0 = emit_lin(&mut pos, &e.a, &sub, p.ty);
            let n = emit_lin(&mut pos, &e.n, &sub, p.ty);
            let (hi, lo) = if e.d == 1 { (n, a0) } else { (a0, n) };
            let diff = pos.ins().isub(hi, lo);
            let te = if e.cc == IntCC::NotEqual {
                diff
            } else {
                let gt = pos.ins().icmp(IntCC::UnsignedGreaterThan, hi, lo);
                let z = pos.ins().iconst(p.ty, 0);
                pos.ins().select(gt, diff, z)
            };
            t = Some(match t {
                Some(t) => pos.ins().umin(t, te),
                None => te,
            });
        }
        let t = t.unwrap();
        let hargs: Vec<BlockArg> = entry_args.iter().map(|&v| BlockArg::Value(v)).collect();
        let mut cargs = hargs.clone();
        // The exit bound: `p0 + T` on the unit-step param, else `T` on j.
        let bound = match p.unit {
            Some(u) => pos.ins().iadd(entry_args[u], t),
            None => {
                let zero = pos.ins().iconst(p.ty, 0);
                cargs.push(BlockArg::Value(zero));
                t
            }
        };
        // `brif t` is nonzero-taken: into the clone when T > 0.
        pos.ins().brif(t, nh, &cargs, p.h, &hargs);
        let func = pos.func;
        // Entry edge -> pre.
        {
            let dfg = &mut func.dfg;
            dfg.insts[p.entry.0].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[p.entry.1] =
                cranelift_codegen::ir::BlockCall::new(pre, [], &mut dfg.value_lists);
        }
        // The clone's latch (a `jump nh(p'..)` once its exits are folded)
        // becomes the rotated test on an up-counter, the shape loopidiom
        // and loopvec count (`ivdown` turns it around late): u' != u0 + T
        // on a unit-step param, else j' = j + 1; j' != T.
        let cb = func.layout.inst_block(p.back.0).map(|b| bmap[&b]).unwrap();
        let ct = func.layout.last_inst(cb).unwrap();
        let InstructionData::Jump { destination, .. } = func.dfg.insts[ct] else { unreachable!("plan checked") };
        let largs: Vec<BlockArg> = destination.args(&func.dfg.value_lists).collect();
        let mut pos = FuncCursor::new(func).at_inst(ct);
        let mut back = largs.clone();
        let more = match (p.unit, c) {
            (Some(u), _) => {
                let BlockArg::Value(nu) = largs[u] else { unreachable!("plan checked") };
                pos.ins().icmp(IntCC::NotEqual, nu, bound)
            }
            (None, Some(c)) => {
                let c1 = pos.ins().iadd_imm_s(c, 1);
                back.push(BlockArg::Value(c1));
                pos.ins().icmp(IntCC::NotEqual, c1, bound)
            }
            _ => unreachable!(),
        };
        match &p.steps {
            Some(steps) => {
                // j' == T on exit: every param is p0 + s*T.
                let x = pos.func.dfg.make_block();
                pos.func.layout.insert_block(x, p.h);
                pos.func.replace(ct).brif(more, nh, &back, x, &[]);
                let mut pos = FuncCursor::new(pos.func).at_bottom(x);
                let fin: Vec<BlockArg> = entry_args
                    .iter()
                    .zip(steps)
                    .map(|(&v0, &s)| {
                        BlockArg::Value(match s {
                            0 => v0,
                            1 => pos.ins().iadd(v0, t),
                            -1 => pos.ins().isub(v0, t),
                            _ => {
                                let k = pos.ins().iconst(p.ty, s);
                                let m = pos.ins().imul(t, k);
                                pos.ins().iadd(v0, m)
                            }
                        })
                    })
                    .collect();
                pos.ins().jump(p.h, &fin);
            }
            None => {
                pos.func.replace(ct).brif(more, nh, &back, p.h, &largs);
            }
        }
    }
    if n > 0 {
        an.invalidate(func);
    }
    n
}

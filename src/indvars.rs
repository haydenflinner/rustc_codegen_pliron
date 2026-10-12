//! Induction-variable strength reduction on CLIF (LLVM indvars' narrow
//! slice): `base + iv*K` and bare `iv*K` inside a loop become new header
//! block params, stepped by `K*step` per back edge instead of multiplied.
//! Wrapping integer arithmetic makes the recurrence exact even on overflow,
//! so no legality check is needed beyond shape-matching the step.
//! `PLIRON_INDUCT=0` disables it; `PLIRON_INDUCT_DEBUG` logs rewrites.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, condcodes::IntCC, Opcode, Type,
    Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::loopidiom::deepest_first;

const MAX_LOOPS: usize = 32;
const MAX_PARAMS: usize = 6;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// The multiplicand of an `iv` term: a constant, or a loop-invariant value
/// (e.g. `iv * n` in `a[k*n+j]` — reduced to `np += n` per iteration).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Mul {
    K(i64),
    V(Value),
}

/// `v` as `iv*K`: `imul(iv, K)`, `ishl(iv, log2K)`, or bare `iv` (K = 1).
/// `K` may be a constant or a value defined outside `body`.
fn scaled(
    func: &Function,
    iv: Value,
    v: Value,
    body: &FxHashSet<Block>,
) -> Option<Mul> {
    let v = func.dfg.resolve_aliases(v);
    if v == iv {
        return Some(Mul::K(1));
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let mul_of = |x: Value| {
        if let Some(k) = iconst(func, x) {
            return Some(Mul::K(k));
        }
        let x = func.dfg.resolve_aliases(x);
        if func.dfg.value_type(x) == func.dfg.value_type(iv)
            && def_block(func, x).is_some_and(|d| !body.contains(&d))
        {
            Some(Mul::V(x))
        } else {
            None
        }
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args: [a, b],
        } => {
            if func.dfg.resolve_aliases(a) == iv {
                mul_of(b)
            } else if func.dfg.resolve_aliases(b) == iv {
                mul_of(a)
            } else {
                None
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [a, b],
        } if func.dfg.resolve_aliases(a) == iv => {
            iconst(func, b).and_then(|s| (0..64).contains(&s).then(|| Mul::K(1i64 << s)))
        }
        _ => None,
    }
}

/// One predecessor edge into `h`: the branch instruction and its `slot`th
/// destination, which must be `h`. The argument for param `idx` is the value
/// the edge passes for it.
#[derive(Clone)]
struct Edge {
    inst: Inst,
    slot: usize,
}

fn edges_to(func: &Function, cfg: &ControlFlowGraph, h: Block) -> Vec<Edge> {
    let mut out = Vec::new();
    for p in cfg.pred_iter(h) {
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) == h {
                out.push(Edge {
                    inst: p.inst,
                    slot,
                });
            }
        }
    }
    out
}

fn edge_arg(func: &Function, e: &Edge, idx: usize) -> Option<Value> {
    let bc = func.dfg.insts[e.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[e.slot];
    match bc.args(&func.dfg.value_lists).nth(idx)? {
        BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
        _ => None,
    }
}

/// `iconst`'s immediate must fit the type's unsigned range; masking is free
/// since the consumers wrap anyway.
fn iconst_masked(pos: &mut FuncCursor, t: Type, k: i64) -> Value {
    if t == types::I128 {
        // `iconst` supports up to i64 only; iconcat the two halves (the hi
        // half sign-extends `k`, matching i64 -> i128 sign extension).
        let lo = pos.ins().iconst(types::I64, k);
        let hi = pos.ins().iconst(types::I64, k >> 63);
        return pos.ins().iconcat(lo, hi);
    }
    let mask = if t.bits() < 64 { (1i64 << t.bits()) - 1 } else { -1 };
    pos.ins().iconst(t, k & mask)
}

/// A per-edge delta for a stepped param: a constant, or a value defined
/// outside the loop body (invariant, e.g. the outer iv of `j += i`).
#[derive(Clone, Copy)]
enum StepV {
    K(i64),
    V(Value),
}

/// `p + mul*step` materialized at the end of `inst`'s block.
fn emit_add(func: &mut Function, inst: Inst, p: Value, mul: Mul, step: StepV) -> Value {
    let t = func.dfg.value_type(p);
    let mut pos = FuncCursor::new(func).at_inst(inst);
    let d = match (mul, step) {
        (Mul::K(a), StepV::K(s)) => iconst_masked(&mut pos, t, a.wrapping_mul(s)),
        (Mul::K(1), StepV::V(x)) => x,
        (Mul::K(a), StepV::V(x)) => {
            let c = iconst_masked(&mut pos, t, a);
            pos.ins().imul(x, c)
        }
        (Mul::V(y), StepV::K(1)) => y,
        (Mul::V(y), StepV::K(s)) => {
            let c = iconst_masked(&mut pos, t, s);
            pos.ins().imul(y, c)
        }
        (Mul::V(y), StepV::V(x)) => pos.ins().imul(y, x),
    };
    pos.ins().iadd(p, d)
}

/// `base + v*K` materialized at the end of `inst`'s block.
fn emit_lin(func: &mut Function, inst: Inst, base: Option<Value>, v: Value, mul: Mul) -> Value {
    let t = func.dfg.value_type(v);
    let mut pos = FuncCursor::new(func).at_inst(inst);
    let s = match mul {
        Mul::K(1) => v,
        Mul::K(k) => {
            let c = iconst_masked(&mut pos, t, k);
            pos.ins().imul(v, c)
        }
        Mul::V(x) => pos.ins().imul(v, x),
    };
    match base {
        Some(b) => pos.ins().iadd(b, s),
        None => s,
    }
}

/// `iv + step` decomposition of an edge argument: constant step, or an
/// operand defined outside the loop body (invariant, e.g. `j += i`).
fn step_form(func: &Function, iv: Value, body: &FxHashSet<Block>, arg: Value) -> Option<StepV> {
    let delta = |x: Value| {
        if let Some(k) = iconst(func, x) {
            Some(StepV::K(k))
        } else {
            let x = func.dfg.resolve_aliases(x);
            def_block(func, x)
                .is_some_and(|d| !body.contains(&d))
                .then_some(StepV::V(x))
        }
    };
    let ValueDef::Result(i, _) = func.dfg.value_def(arg) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } if func.dfg.resolve_aliases(a) == iv => delta(b),
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } if func.dfg.resolve_aliases(b) == iv => delta(a),
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args: [a, b],
        } if func.dfg.resolve_aliases(a) == iv => {
            iconst(func, b).map(|x| StepV::K(x.wrapping_neg()))
        }
        _ => None,
    }
}

/// If `inst`'s terminator argument for iv-param `idx` is `iv + step`, the new
/// param's argument is `np + K*step`; otherwise recompute `base + arg*K`.
fn edge_value(
    func: &mut Function,
    iv: Value,
    np: Value,
    body: &FxHashSet<Block>,
    base: Option<Value>,
    mul: Mul,
    e: &Edge,
    arg: Value,
) -> Value {
    // Wrapping arithmetic keeps the recurrence exact even when `mul*step`
    // overflows `i64`.
    match step_form(func, iv, body, arg) {
        Some(s) => emit_add(func, e.inst, np, mul, s),
        None => emit_lin(func, e.inst, base, arg, mul),
    }
}

fn run_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    debug: bool,
) -> usize {
    let h = la.loop_header(lp);
    if !dt.is_reachable(h) {
        return 0;
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let edges = edges_to(func, cfg, h);
    let params = func.dfg.block_params(h).to_vec();
    let mut n = 0;
    // Every `iv*K` in the loop, and every `invariant + iv*K`, is a candidate.
    for (idx, &iv) in params.iter().enumerate() {
        if n >= MAX_PARAMS {
            break;
        }
        if !func.dfg.value_type(iv).is_int() {
            continue;
        }
        // Collect (iv*K insts) and (base + iv*K insts) inside the loop. The
        // `bare` flag records whether a `base + iv` add references the iv
        // directly (vs `imul iv, 1`): converting the former only buys
        // regalloc parallel-copy movs.
        let mut targets: FxHashMap<(Option<Value>, Mul, bool), Vec<Inst>> =
            FxHashMap::default();
        for &b in &body {
            for i in func.layout.block_insts(b) {
                match func.dfg.insts[i] {
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args: [a, b2],
                    } => {
                        for (x, y) in [(a, b2), (b2, a)] {
                            if let Some(mul) = scaled(func, iv, y, &body)
                                && def_block(func, x).is_some_and(|d| !body.contains(&d))
                            {
                                let bare = func.dfg.resolve_aliases(y) == iv;
                                targets.entry((Some(x), mul, bare)).or_default().push(i);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(&r) = func.dfg.inst_results(i).first()
                    && let Some(mul) = scaled(func, iv, r, &body)
                    && mul != Mul::K(1)
                {
                    targets.entry((None, mul, false)).or_default().push(i);
                }
            }
        }
        if targets.is_empty() {
            continue;
        }
        for (&(base, mul, bare), insts) in &targets {
            if n >= MAX_PARAMS {
                break;
            }
            // `base + iv` is already a single cheap add; turning it into a
            // stepped param only buys regalloc parallel-copy movs. A real
            // `imul iv, 1` is a genuine dead multiply — stepping it away
            // removes two insts per iteration.
            if mul == Mul::K(0) || (mul == Mul::K(1) && bare) {
                continue;
            }
            let ty: Type = base
                .map(|b| func.dfg.value_type(b))
                .unwrap_or_else(|| func.dfg.value_type(iv));
            if !ty.is_int() {
                continue;
            }
            // Every edge must pass a plain value for the iv param, else the
            // new param can't be filled in.
            let args: Option<Vec<Value>> = edges
                .iter()
                .map(|e| edge_arg(func, e, idx))
                .collect();
            let Some(args) = args else {
                continue;
            };
            // Compute the new param's argument on every edge into `h`.
            let np = func.dfg.append_block_param(h, ty);
            for (e, arg) in edges.iter().zip(args) {
                let v = edge_value(func, iv, np, &body, base, mul, e, arg);
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[e.inst].branch_destination_mut(
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                )[e.slot];
                bc.append_argument(v, &mut dfg.value_lists);
            }
            for &i in insts {
                let r = func.dfg.first_result(i);
                func.layout.remove_inst(i);
                func.dfg.clear_results(i);
                func.dfg.change_to_alias(r, np);
            }
            if debug {
                eprintln!("indvars {h}: base {base:?} mul {mul:?} -> {np} ({} insts)", insts.len());
            }
            n += 1;
        }
    }
    n
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let debug = std::env::var_os("PLIRON_INDUCT_DEBUG").is_some();
    let loops = deepest_first(&la);
    let mut n = 0;
    for lp in loops.into_iter().take(MAX_LOOPS) {
        n += run_loop(func, &cfg, &dt, &la, lp, debug);
    }
    n
}


/// Whether a latch's continue test is `a < K` (strict) or `a <= K`.
#[derive(Clone, Copy, PartialEq)]
enum Lt {
    Le = 0,
    Lt = 1,
}

/// Loop count tests against a wide constant `K` rematerialize it every
/// iteration. When `p` is a header param entering via `ult j0, K` and every
/// backedge arg it gets is itself count-tested (`a = p` or `p + s` with
/// `s < 2^63`), carry `rem = D - p` as a stepped param (`D = K` or `K - 1`)
/// and test `ule (rem - s), rem0` at the latch: `j0 <= p <= K` keeps `a`
/// inside `[j0, 2^64)`, so `D - a` wraps to a large value exactly when `a`
/// overshoots `D` — `ule (D - a), (D - j0)` then reproduces `a cc K`
/// precisely. `rem0 = D - j0` is a non-constant preheader value, so codegen
/// pins it in a register; `rem - s` doubles as the backedge arg, so the
/// loop trades two remat instructions for one `isub`.
pub fn boundcmp(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let psrc = crate::loadfwd::param_src(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let debug = std::env::var_os("PLIRON_INDUCT_DEBUG").is_some();
    let mut n = 0;
    for lp in deepest_first(&la).into_iter().take(MAX_LOOPS) {
        let h = la.loop_header(lp);
        if !dt.is_reachable(h) {
            continue;
        }
        let body: FxHashSet<Block> = func
            .layout
            .blocks()
            .filter(|&b| la.is_in_loop(b, lp))
            .collect();
        let params = func.dfg.block_params(h).to_vec();
        let edges = edges_to(func, &cfg, h);
        for (idx, &p) in params.iter().enumerate() {
            if !func.dfg.value_type(p).is_int() {
                continue;
            }
            // Every entry edge must pass `ult j0, K` on the value it feeds
            // `p` (proving `j0 < K`); every backedge arg must be `p` or
            // `p + s` and count-tested on that edge (keeping `p <= K`).
            // All entries feed the same `j0`; all tests share one `K` and
            // one strictness (`D`).
            let mut j0 = None;
            let mut bound = None;
            let mut strict = None;
            let mut entries = Vec::new();
            let mut latches: Vec<(Edge, Inst, StepV)> = Vec::new();
            let mut ok = true;
            for e in &edges {
                let eb = func.layout.inst_block(e.inst).unwrap();
                let Some(a) = edge_arg(func, e, idx) else {
                    ok = false;
                    break;
                };
                if body.contains(&eb) {
                    let Some((k, lt, ci)) = count_tested(func, e.inst, e.slot, a, h)
                        .filter(|(k, _, _)| bound.is_none_or(|b| b == *k))
                        .filter(|(_, lt, _)| strict.is_none_or(|s| s == *lt))
                    else {
                        ok = false;
                        break;
                    };
                    let Some(step) = step_ok(func, &dt, &body, &psrc, ci, a, p) else {
                        ok = false;
                        break;
                    };
                    bound = Some(k);
                    strict = Some(lt);
                    latches.push((e.clone(), ci, step));
                    continue;
                }
                let InstructionData::Brif {
                    arg: cond, blocks, ..
                } = func.dfg.insts[e.inst]
                else {
                    ok = false;
                    break;
                };
                // The edge to `h` must be the compare's below-bound arm,
                // strictly (`ult j0, K` -> `j0 <= K - 1 <= D`).
                let Some(k) = (e.slot == 0
                    && blocks[0].block(&func.dfg.value_lists) == h)
                    .then(|| {
                        func.dfg
                            .value_def(func.dfg.resolve_aliases(cond))
                            .inst()
                            .and_then(|ci| match func.dfg.insts[ci] {
                                InstructionData::IntCompare {
                                    cond: IntCC::UnsignedLessThan,
                                    args: [x, y],
                                    ..
                                } if func.dfg.resolve_aliases(x) == a => {
                                    iconst(func, y).filter(|k| {
                                        (4096..=i64::MAX).contains(k)
                                    })
                                }
                                _ => None,
                            })
                    })
                    .flatten()
                else {
                    ok = false;
                    break;
                };
                if bound.is_some_and(|b| b != k)
                    || j0.is_some_and(|j| j != a)
                    || iconst(func, a).is_some()
                {
                    ok = false;
                    break;
                }
                bound = Some(k);
                j0 = Some(a);
                entries.push(e.clone());
            }
            let (Some(j0), Some(k), Some(lt)) = (j0, bound, strict) else {
                continue;
            };
            let [ref entry] = entries[..] else {
                continue;
            };
            if !ok || latches.is_empty() {
                continue;
            }
            let d = k - lt as i64;
            let ty = func.dfg.value_type(p);
            // `rem = D - p` stepped by `-s` per backedge; `rem0 = D - j0`
            // computed once in the entry pred and shared as the bound.
            let mut pos = FuncCursor::new(func).at_inst(entry.inst);
            let kv = iconst_masked(&mut pos, ty, d);
            let rem0 = pos.ins().isub(kv, j0);
            let rem = func.dfg.append_block_param(h, ty);
            {
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[entry.inst].branch_destination_mut(
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                )[entry.slot];
                bc.append_argument(rem0, &mut dfg.value_lists);
            }
            for (e, _, step) in &latches {
                let v = match step {
                    StepV::K(0) => rem,
                    StepV::K(s) => {
                        let mut pos = FuncCursor::new(func).at_inst(e.inst);
                        let c = iconst_masked(&mut pos, ty, *s);
                        pos.ins().isub(rem, c)
                    }
                    StepV::V(x) => {
                        let mut pos = FuncCursor::new(func).at_inst(e.inst);
                        pos.ins().isub(rem, *x)
                    }
                };
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[e.inst].branch_destination_mut(
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                )[e.slot];
                bc.append_argument(v, &mut dfg.value_lists);
            }
            // Rewrite the latch count test to `ule (D - a), rem0`.
            for (_, li, step) in &latches {
                let cur = match step {
                    StepV::K(0) => rem,
                    StepV::K(s) => {
                        let mut pos = FuncCursor::new(func).at_inst(*li);
                        let c = iconst_masked(&mut pos, ty, *s);
                        pos.ins().isub(rem, c)
                    }
                    StepV::V(x) => {
                        let mut pos = FuncCursor::new(func).at_inst(*li);
                        pos.ins().isub(rem, *x)
                    }
                };
                if let InstructionData::IntCompare { cond, .. } = &mut func.dfg.insts[*li] {
                    *cond = IntCC::UnsignedLessThanOrEqual;
                }
                let args = func.dfg.inst_args_mut(*li);
                args[0] = cur;
                args[1] = rem0;
                if debug {
                    eprintln!("indvars {h}: countdown {li} (bound {k})");
                }
                n += 1;
            }
        }
    }
    n
}

/// `inst` is `brif (icmp cc a, K)` whose `slot`th edge continues the loop
/// exactly when `a` is below `K`; returns `K`, the test's strictness, and
/// the compare inst.
fn count_tested(
    func: &Function,
    inst: Inst,
    slot: usize,
    a: Value,
    h: Block,
) -> Option<(i64, Lt, Inst)> {
    let InstructionData::Brif {
        arg: cond, blocks, ..
    } = func.dfg.insts[inst]
    else {
        return None;
    };
    if blocks[slot].block(&func.dfg.value_lists) != h {
        return None;
    }
    let ci = func.dfg.value_def(func.dfg.resolve_aliases(cond)).inst()?;
    let InstructionData::IntCompare {
        cond,
        args: [x, y],
        ..
    } = func.dfg.insts[ci]
    else {
        return None;
    };
    if func.dfg.resolve_aliases(x) != a {
        return None;
    }
    let lt = match (slot, cond) {
        (0, IntCC::UnsignedLessThan) | (1, IntCC::UnsignedGreaterThanOrEqual) => Lt::Lt,
        (0, IntCC::UnsignedLessThanOrEqual) | (1, IntCC::UnsignedGreaterThan) => Lt::Le,
        _ => return None,
    };
    iconst(func, y)
        .filter(|k| (4096..=i64::MAX).contains(k))
        .map(|k| (k, lt, ci))
}

/// The step `s` in `a = p + s` (or `K(0)` when `a` is `p` itself), when `s`
/// is provably `< 2^63` — so `p + s` can't wrap while `p <= K`.
fn step_ok(
    func: &Function,
    dt: &DominatorTree,
    body: &FxHashSet<Block>,
    psrc: &FxHashMap<Value, Value>,
    i: Inst,
    a: Value,
    p: Value,
) -> Option<StepV> {
    let a = func.dfg.resolve_aliases(a);
    if a == p {
        return Some(StepV::K(0));
    }
    let ValueDef::Result(ai, _) = func.dfg.value_def(a) else {
        return None;
    };
    let (x, s) = match func.dfg.insts[ai] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [x, s],
        } => (x, s),
        _ => return None,
    };
    let s = if func.dfg.resolve_aliases(x) == p {
        s
    } else if func.dfg.resolve_aliases(s) == p {
        x
    } else {
        return None;
    };
    if let Some(sk) = iconst(func, s) {
        return (0..=i64::MAX).contains(&sk).then_some(StepV::K(sk));
    }
    let s = func.dfg.resolve_aliases(s);
    (def_block(func, s).is_some_and(|d| !body.contains(&d))
        && bounded(func, dt, psrc, i, s))
    .then_some(StepV::V(s))
}

/// `icmp ult/ule b, k` with `k <= i64::MAX` in a dominator block of `i`,
/// where `b` is `s` or `s*s`: proves `s < 2^63` (`s*s < k` bounds `s` by
/// `sqrt(k)` — a square can't be negative), so `p + s` can't wrap past
/// `u64::MAX` while `p <= K`.
fn bounded(
    func: &Function,
    dt: &DominatorTree,
    psrc: &FxHashMap<Value, Value>,
    i: Inst,
    s: Value,
) -> bool {
    let mut dom = func.layout.inst_block(i);
    while let Some(d) = dom {
        for j in func.layout.block_insts(d) {
            let InstructionData::IntCompare {
                cond,
                args: [a, b],
                ..
            } = func.dfg.insts[j]
            else {
                continue;
            };
            if !matches!(
                cond,
                IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual
            ) || !iconst(func, b).is_some_and(|k| (0..=i64::MAX).contains(&k))
            {
                continue;
            }
            // Follow block params to the value all their edges feed.
            let mut a = func.dfg.resolve_aliases(a);
            for _ in 0..8 {
                match psrc.get(&a) {
                    Some(&v) => a = func.dfg.resolve_aliases(v),
                    None => break,
                }
            }
            if a == s {
                return true;
            }
            if let ValueDef::Result(ai, _) = func.dfg.value_def(a)
                && let InstructionData::Binary {
                    opcode: Opcode::Imul,
                    args: [x, y],
                } = func.dfg.insts[ai]
                && func.dfg.resolve_aliases(x) == s
                && func.dfg.resolve_aliases(y) == s
            {
                return true;
            }
        }
        dom = dt.idom(d);
    }
    false
}

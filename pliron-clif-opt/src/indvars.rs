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
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode,
    ProgramPoint, Type, Value, ValueDef, condcodes::{CondCode, IntCC}, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};

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
fn scaled(func: &Function, iv: Value, v: Value, body: &FxHashSet<Block>) -> Option<Mul> {
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
                out.push(Edge { inst: p.inst, slot });
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
    let mask = if t.bits() < 64 {
        (1i64 << t.bits()) - 1
    } else {
        -1
    };
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

/// Every use of `insts`' results only ever forms a machine address:
/// the value is a load/store address operand directly, or one operand
/// of an `iadd` whose other operand is loop-invariant and whose result
/// is itself address-only — `base + iv*scale` two levels deep still
/// folds into one reg+reg*scale amode.
fn mem_addr_only(func: &Function, body: &FxHashSet<Block>, insts: &[Inst]) -> bool {
    let mut addr: FxHashSet<Value> = insts.iter().map(|&i| func.dfg.first_result(i)).collect();
    loop {
        let mut grew = false;
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for (k, &a) in func.dfg.inst_args(i).iter().enumerate() {
                    let a = func.dfg.resolve_aliases(a);
                    if !addr.contains(&a) {
                        continue;
                    }
                    match func.dfg.insts[i] {
                        InstructionData::Load { .. } if k == 0 => {}
                        InstructionData::Store { .. } if k == 1 => {}
                        InstructionData::Binary {
                            opcode: Opcode::Iadd,
                            args: [x, y],
                        } => {
                            let other = if func.dfg.resolve_aliases(x) == a {
                                y
                            } else {
                                x
                            };
                            match def_block(func, other) {
                                Some(d) if body.contains(&d) => return false,
                                _ => {}
                            }
                            let r = func.dfg.first_result(i);
                            if addr.insert(r) {
                                grew = true;
                            }
                        }
                        _ => return false,
                    }
                }
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a
                            && addr.contains(&func.dfg.resolve_aliases(v))
                        {
                            return false;
                        }
                    }
                }
            }
        }
        if !grew {
            return true;
        }
    }
}

/// Would replacing `insts`' uses of `base` leave `base` with no use inside
/// the loop, while it stays live outside it? Then converting `base + iv*K`
/// to a stepped pointer param evicts `base` from the loop even though the
/// reg+reg*scale amode folds the address for free: it shrinks `base`'s
/// liverange so it no longer spans the whole loop, which both frees a
/// register inside and avoids the per-iteration copies regalloc emits when
/// reconciling such a range's in-loop/out-of-loop assignments.
fn evicts_base(func: &Function, body: &FxHashSet<Block>, insts: &[Inst], base: Value) -> bool {
    let instset: FxHashSet<Inst> = insts.iter().copied().collect();
    // Anything appearing as an operand (inst args or branch args). A result
    // outside this set is dead: its def can't keep `base` live past the
    // next dead-inst sweep, so it doesn't block eviction.
    let used: FxHashSet<Value> = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .flat_map(|i| func.dfg.inst_values(i))
        .map(|v| func.dfg.resolve_aliases(v))
        .collect();
    let base = func.dfg.resolve_aliases(base);
    let mut outside = false;
    for b in func.layout.blocks() {
        let in_body = body.contains(&b);
        for i in func.layout.block_insts(b) {
            if instset.contains(&i)
                || !func
                    .dfg
                    .inst_values(i)
                    .any(|v| func.dfg.resolve_aliases(v) == base)
            {
                continue;
            }
            let op = func.dfg.insts[i].opcode();
            let dead = func.dfg.inst_results(i).iter().all(|&r| !used.contains(&r))
                && !(op.can_load()
                    || op.can_store()
                    || op.can_trap()
                    || op.is_call()
                    || op.is_branch()
                    || op.is_terminator()
                    || op.other_side_effects());
            if dead {
                continue;
            }
            if in_body {
                return false;
            }
            outside = true;
        }
    }
    outside
}

/// `np = base + iv*e` (stepped `e*step` per iteration) evicts a live-out
/// `base` from the loop, but leaves `iv` shadowed when its only
/// remaining work is bound tests and exit values: `iv+c cc bound`
/// rewrites to `np+c*e cc pend` with `pend = base + bound*e`
/// materialized on each entry edge, and exits recover the dropped
/// index as `(np+c*e - base) >> log2(e)` through a shared cold shim.
/// The loop then carries one counter instead of two — LLVM's one-IV
/// scan shape.
///
/// Soundness: `pend` and every compared `np+c*e` stay inside the
/// object's address range — the loop's own `notrap` accesses on `np`
/// establish it contains each visited element, and the bound test
/// exits on first failure so no pointer past `pend` is evaluated —
/// so neither add wraps and the unsigned compare is faithful. `e`
/// must be a positive power of two for the shift recovery; `isub` is
/// exact mod 2^w regardless.
fn elim_shadowed(
    func: &mut Function,
    dt: &DominatorTree,
    body: &FxHashSet<Block>,
    h: Block,
    edges: &[Edge],
    iv: Value,
    np: Value,
    base: Value,
    e: i64,
    debug: bool,
) -> usize {
    if e <= 0 || !e.unsigned_abs().is_power_of_two() || !crate::pass_enabled("PLIRON_IVELIM") {
        return 0;
    }
    let sh = i64::from(e.trailing_zeros());
    let ty = func.dfg.value_type(np);
    if func.dfg.value_type(iv) != ty || func.dfg.value_type(base) != ty {
        return 0;
    }
    let (iv, np, base) = (
        func.dfg.resolve_aliases(iv),
        func.dfg.resolve_aliases(np),
        func.dfg.resolve_aliases(base),
    );
    let params = func.dfg.block_params(h).to_vec();
    let (Some(iv_idx), Some(np_idx)) = (
        params.iter().position(|&p| func.dfg.resolve_aliases(p) == iv),
        params.iter().position(|&p| func.dfg.resolve_aliases(p) == np),
    ) else {
        return 0;
    };
    // Backedges must step both params in lockstep (`np += e*step`), or
    // the `np = base + iv*e` recurrence identity doesn't hold.
    if debug {
        eprintln!("indvars {h}: ivelim cand iv={iv} np={np} base={base} e={e}");
    }
    let mut step = None;
    for e2 in edges {
        if !func.layout.inst_block(e2.inst).is_some_and(|b| body.contains(&b)) {
            continue;
        }
        let si = edge_arg(func, e2, iv_idx).and_then(|a| step_form(func, iv, body, a));
        let sn = edge_arg(func, e2, np_idx).and_then(|a| step_form(func, np, body, a));
        let (Some(StepV::K(s)), Some(StepV::K(sn))) = (si, sn) else {
            if debug {
                eprintln!("indvars {h}: ivelim bail stepform");
            }
            return 0;
        };
        if sn != s.wrapping_mul(e) || step.is_some_and(|x| x != s) {
            if debug {
                eprintln!("indvars {h}: ivelim bail lockstep s={s} sn={sn}");
            }
            return 0;
        }
        step = Some(s);
    }
    if step.is_none() {
        return 0;
    }
    // `iv + c` results in the loop, and the `np + c*e` counterparts
    // that already exist to stand in for them (the step's np arg).
    let mut off: FxHashMap<Value, i64> = [(iv, 0)].into_iter().collect();
    let mut noff: FxHashMap<i64, Value> = [(0, np)].into_iter().collect();
    let mut adefs: FxHashSet<Inst> = FxHashSet::default();
    for &b in body {
        for i in func.layout.block_insts(b) {
            let InstructionData::Binary { opcode, args: [x, y] } = func.dfg.insts[i] else {
                continue;
            };
            let r = func.dfg.first_result(i);
            let mut try_off = |a: Value, d: i64| {
                let a = func.dfg.resolve_aliases(a);
                if let Some(&o) = off.get(&a) {
                    off.insert(r, o.wrapping_add(d));
                    adefs.insert(i);
                }
                if a == np {
                    noff.insert(d, r);
                }
            };
            match opcode {
                Opcode::Iadd => {
                    if let Some(d) = iconst(func, y) {
                        try_off(x, d);
                    }
                    if let Some(d) = iconst(func, x) {
                        try_off(y, d);
                    }
                }
                Opcode::Isub => {
                    if let Some(d) = iconst(func, y) {
                        try_off(x, d.wrapping_neg());
                    }
                }
                _ => {}
            }
        }
    }
    // Pure insts whose results are unreferenced — transitively — are
    // waste: they can't keep `iv` live, and they get removed along
    // with the affine adds. `used` shrinks until stable.
    let mut used: FxHashSet<Value> = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .flat_map(|i| func.dfg.inst_values(i))
        .map(|v| func.dfg.resolve_aliases(v))
        .collect();
    let mut dead: FxHashSet<Inst> = FxHashSet::default();
    loop {
        let mut grew = false;
        for &b in body {
            for i in func.layout.block_insts(b) {
                if dead.contains(&i) {
                    continue;
                }
                let op = func.dfg.insts[i].opcode();
                let pure = !(op.can_load()
                    || op.can_store()
                    || op.can_trap()
                    || op.is_call()
                    || op.is_branch()
                    || op.is_terminator()
                    || op.other_side_effects());
                if pure && func.dfg.inst_results(i).iter().all(|&r| !used.contains(&r)) {
                    dead.insert(i);
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
        used = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .filter(|&i| !dead.contains(&i))
            .flat_map(|i| func.dfg.inst_values(i))
            .map(|v| func.dfg.resolve_aliases(v))
            .collect();
    }
    // The recurrence identity is unsigned: `np+c*e <_u pend` tracks
    // `iv+c <_u bound` only while both stay inside the object, which a
    // `notrap` access on an `np`-affine address proves — as long as it
    // runs every iteration, i.e. its block dominates every backedge.
    let nvals: FxHashSet<Value> = noff.values().copied().collect();
    let guarded = body.iter().any(|&b| {
        let access = func.layout.block_insts(b).any(|i| match func.dfg.insts[i] {
            InstructionData::Load { arg, flags, .. } => {
                func.dfg.mem_flags[flags].notrap()
                    && nvals.contains(&func.dfg.resolve_aliases(arg))
            }
            InstructionData::Store { args, flags, .. } => {
                func.dfg.mem_flags[flags].notrap()
                    && nvals.contains(&func.dfg.resolve_aliases(args[1]))
            }
            _ => false,
        });
        access
            && edges.iter().all(|e2| {
                func.layout
                    .inst_block(e2.inst)
                    .is_none_or(|pb| !body.contains(&pb) || dt.dominates(b, pb, &func.layout))
            })
    });
    if !guarded {
        if debug {
            eprintln!("indvars {h}: ivelim bail notrap");
        }
        return 0;
    }
    // `np` is a header param and dominates the loop; a derived
    // `np + c*e` only substitutes for `iv + c` where its def already
    // dominates the use.
    let pmap = |w: Value, at: Inst| -> Option<Value> {
        let c = off.get(&func.dfg.resolve_aliases(w))?;
        let m = *noff.get(&c.wrapping_mul(e))?;
        (m == np
            || matches!(func.dfg.value_def(m), ValueDef::Result(di, _)
                if dt.dominates(di, at, &func.layout)))
        .then_some(m)
    };
    // Every use of an `iv`-affine value must be a bound icmp (both
    // sides affine is ambiguous), the lockstep adds themselves, the
    // step backedge arg, or an exit arg with a live `np` counterpart.
    let mut icmps: Vec<(Inst, Value, Value, Value)> = vec![]; // (inst, aff, bound, map)
    let mut exits: Vec<(Inst, usize, Block, Vec<Value>)> = vec![]; // (inst, slot, dst, shim args)
    for &b in body {
        for i in func.layout.block_insts(b) {
            if adefs.contains(&i) || dead.contains(&i) {
                continue;
            }
            for (slot, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                let dst = bc.block(&func.dfg.value_lists);
                let mut mapped = vec![];
                let mut hit = false;
                for (ai, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    let BlockArg::Value(a) = a else {
                        return 0;
                    };
                    let a = func.dfg.resolve_aliases(a);
                    match off.get(&a) {
                        None => mapped.push(a),
                        Some(_) if dst == h => {
                            if ai != iv_idx {
                                if debug {
                                    eprintln!("indvars {h}: ivelim bail harg slot {ai}");
                                }
                                return 0;
                            }
                            hit = true;
                        }
                        Some(_) if !body.contains(&dst) => {
                            hit = true;
                            let Some(m) = pmap(a, i) else {
                                if debug {
                                    eprintln!("indvars {h}: ivelim bail exitmap {a} at {b}");
                                }
                                return 0;
                            };
                            mapped.push(m);
                        }
                        Some(_) => {
                            if debug {
                                eprintln!("indvars {h}: ivelim bail inbody arg {a} -> {dst}");
                            }
                            return 0;
                        }
                    }
                }
                if hit && dst != h {
                    exits.push((i, slot, dst, mapped));
                }
            }
            match func.dfg.insts[i] {
                InstructionData::IntCompare { args: [x, y], .. } => {
                    match (
                        off.contains_key(&func.dfg.resolve_aliases(x)),
                        off.contains_key(&func.dfg.resolve_aliases(y)),
                    ) {
                        (true, true) => return 0,
                        (false, false) => {}
                        (ax, _) => {
                            let (w, bound) = if ax { (x, y) } else { (y, x) };
                            let bound = func.dfg.resolve_aliases(bound);
                            // `pend` materializes on entry edges, so the
                            // bound must reach every entry pred.
                            if iconst(func, bound).is_none()
                                && def_block(func, bound).is_some_and(|d| body.contains(&d))
                            {
                                if debug {
                                    eprintln!("indvars {h}: ivelim bail bound {bound} in body");
                                }
                                return 0;
                            }
                            let Some(m) = pmap(w, i) else {
                                if debug {
                                    eprintln!("indvars {h}: ivelim bail icmpmap {w} at {b}");
                                }
                                return 0;
                            };
                            icmps.push((i, w, bound, m));
                        }
                    }
                }
                _ => {
                    for &v in func.dfg.inst_args(i) {
                        if off.contains_key(&func.dfg.resolve_aliases(v)) {
                            if debug {
                                eprintln!("indvars {h}: ivelim bail use {v} in {i} @{b}");
                            }
                            return 0;
                        }
                    }
                }
            }
        }
    }
    // `iv`-affine values can't be used below the loop either (a header
    // param can reach dominated exit blocks).
    for b in func.layout.blocks() {
        if body.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                if off.contains_key(&func.dfg.resolve_aliases(v)) {
                    if debug {
                        eprintln!("indvars {h}: ivelim bail outer use {v} @{b}");
                    }
                    return 0;
                }
            }
        }
    }
    if icmps.is_empty() {
        if debug {
            eprintln!("indvars {h}: ivelim bail noicmps");
        }
        return 0;
    }
    // `pend = base + bound*e` per distinct bound, carried as a new
    // header param: materialized on entry edges, fed itself on
    // backedges.
    let mut pends: FxHashMap<Value, Value> = FxHashMap::default();
    for &(_, _, bnd, _) in &icmps {
        if pends.contains_key(&bnd) {
            continue;
        }
        let pend = func.dfg.append_block_param(h, ty);
        for e2 in edges {
            let v = if func.layout.inst_block(e2.inst).is_some_and(|b| body.contains(&b)) {
                pend
            } else {
                let mut pos = FuncCursor::new(func).at_inst(e2.inst);
                let s = iconst_masked(&mut pos, ty, e);
                let m = pos.ins().imul(bnd, s);
                pos.ins().iadd(base, m)
            };
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[e2.inst]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                [e2.slot];
            bc.append_argument(v, &mut dfg.value_lists);
        }
        pends.insert(bnd, pend);
    }
    for &(i, w, bnd, m) in &icmps {
        let pend = pends[&bnd];
        let mut data = func.dfg.insts[i];
        if let InstructionData::IntCompare { cond, args, .. } = &mut data {
            if func.dfg.resolve_aliases(args[0]) == func.dfg.resolve_aliases(w) {
                *args = [m, pend];
            } else {
                *args = [pend, m];
                *cond = cond.swap_args();
            }
        }
        func.dfg.insts[i] = data;
    }
    // Exit edges carrying affine args route through a shared shim that
    // recovers each index as `(p - base) >> log2(e)` — one shim per
    // (target, affine-signature). Shims are marked cold: they run once
    // per loop exit, and the marking keeps forwarder-folding passes
    // (fusechains, foldforwarders) from inlining the recovery back into
    // the loop's exiting blocks, where it would execute unconditionally
    // every iteration.
    let mut shims: FxHashMap<(Block, Vec<Option<Type>>), Block> = FxHashMap::default();
    for &(i, slot, dst, ref mapped) in &exits {
        let b = func.layout.inst_block(i).unwrap();
        let sig: Vec<Option<Type>> = func.dfg.insts[i]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[slot]
            .args(&func.dfg.value_lists)
            .map(|a| match a {
                BlockArg::Value(v) => {
                    let v = func.dfg.resolve_aliases(v);
                    (!off.contains_key(&v)).then_some(func.dfg.value_type(v))
                }
                _ => None,
            })
            .collect();
        let shim = match shims.get(&(dst, sig.clone())) {
            Some(&s) => s,
            None => {
                let s = func.dfg.make_block();
                func.layout.set_cold(s);
                func.layout.insert_block_after(s, b);
                let mut args = vec![];
                for &t in &sig {
                    args.push(func.dfg.append_block_param(s, t.unwrap_or(ty)));
                }
                let mut pos = FuncCursor::new(func).at_bottom(s);
                for (ai, t) in sig.iter().enumerate() {
                    if t.is_none() {
                        let d = pos.ins().isub(args[ai], base);
                        args[ai] = if sh == 0 {
                            d
                        } else {
                            let sc = pos.ins().iconst(ty, sh);
                            pos.ins().ushr(d, sc)
                        };
                    }
                }
                let jargs: Vec<BlockArg> = args.iter().map(|&v| BlockArg::from(v)).collect();
                pos.ins().jump(dst, &jargs);
                shims.insert((dst, sig), s);
                s
            }
        };
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[i]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[slot];
        bc.set_block(shim, &mut dfg.value_lists);
        bc.clear(&mut dfg.value_lists);
        bc.extend(mapped.iter().copied(), &mut dfg.value_lists);
    }
    // Strip `iv`'s edge arg and param; the affine adds are dead once
    // their uses are gone, along with the waste insts ignored above.
    for e2 in edges {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[e2.inst]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e2.slot];
        bc.remove(iv_idx, &mut dfg.value_lists);
    }
    for &i in adefs.union(&dead) {
        func.dfg.clear_results(i);
        func.layout.remove_inst(i);
    }
    func.dfg.remove_block_param(params[iv_idx]);
    if debug {
        eprintln!("indvars {h}: ivelim {iv} via {np} (e={e})");
    }
    1
}

fn run_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    debug: bool,
    amode_scale: bool,
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
    // Pointer params created for live-out bases may fully shadow their
    // index: (iv param, np, base, e) for `elim_shadowed` below.
    let mut cands: Vec<(Value, Value, Value, i64)> = vec![];
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
        let mut targets: FxHashMap<(Option<Value>, Mul, bool), Vec<Inst>> = FxHashMap::default();
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
            // `base + iv*{1,2,4,8}` feeding only memory addresses folds
            // into the reg+reg*scale amode for free: a stepped param is a
            // whole extra IV chain for nothing (iterw kept `i` for the
            // bound check and paid `lea`+`addq` per element on the
            // redundant pointer param). Only where the target folds
            // scaled addressing — wasm/riscv compute addresses
            // explicitly and genuinely want the stepped pointer. Except
            // when the conversion evicts `base` from the loop: a live-out
            // invariant no longer spans the loop, so the pointer param
            // buys back the register and the liverange-split copies.
            if amode_scale
                && matches!(mul, Mul::K(1 | 2 | 4 | 8))
                && mem_addr_only(func, &body, insts)
                && !base.is_some_and(|b| evicts_base(func, &body, insts, b))
            {
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
            let args: Option<Vec<Value>> = edges.iter().map(|e| edge_arg(func, e, idx)).collect();
            let Some(args) = args else {
                continue;
            };
            // Compute the new param's argument on every edge into `h`.
            let np = func.dfg.append_block_param(h, ty);
            for (e, arg) in edges.iter().zip(args) {
                let v = edge_value(func, iv, np, &body, base, mul, e, arg);
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[e.inst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                    [e.slot];
                bc.append_argument(v, &mut dfg.value_lists);
            }
            for &i in insts {
                let r = func.dfg.first_result(i);
                func.layout.remove_inst(i);
                func.dfg.clear_results(i);
                func.dfg.change_to_alias(r, np);
            }
            if debug {
                eprintln!(
                    "indvars {h}: base {base:?} mul {mul:?} -> {np} ({} insts)",
                    insts.len()
                );
            }
            if let (Some(b), Mul::K(k)) = (base, mul) {
                cands.push((iv, np, b, k));
            }
            n += 1;
        }
    }
    for (iv, np, b, k) in cands {
        n += elim_shadowed(func, dt, &body, h, &edges, iv, np, b, k, debug);
    }
    n
}

pub fn run(func: &mut Function, an: &mut crate::pipeline::Analysis, amode_scale: bool) -> usize {
    let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
    let debug = crate::env_set("PLIRON_INDUCT_DEBUG");
    let loops = deepest_first(la);
    let mut n = 0;
    for lp in loops.into_iter().take(MAX_LOOPS) {
        n += run_loop(func, cfg, dt, la, lp, debug, amode_scale);
    }
    n
}

/// Loop-carried load reuse (LLVM GVN's backedge phi-translation). A load
/// `B` rereads each iteration the address a sibling load `A` read last
/// iteration whenever their affine addresses are one step apart:
/// `koff_B + shift == koff_A`. Carrying `prev` as a header param —
/// seeded by a compensating entry-edge load, fed `A`'s result on every
/// backedge — turns the two-load scan into a one-load loop. Run
/// detection (`v[i] < v[i-1]`), insertion-sort probes and sliding-window
/// compares lose a load, the index arithmetic and a block per element;
/// unrolled copies additionally collapse onto the previous copy's load
/// by the same-address CSE in the same group.
///
/// Soundness: the compensating load evaluates exactly `B`'s first-
/// iteration address on each entry edge, so nothing is speculated. `A`
/// must dominate every backedge so its result is a legal edge argument
/// — which also means it executes each iteration, and since its address
/// is a pure function of the loop state every evaluation loads the same
/// location. `B` may sit anywhere in the loop: `prev` equals its read
/// whenever it runs, and never matters when it doesn't. The body may
/// hold no stores or calls, so the carried location is unchanged
/// between `A`'s load and `B`'s next-iteration read.

const MAX_ROT: usize = 4;
const DEC_FUEL: usize = 12;

/// `v` as `Σ terms + kcoef*iter + koff`: the affine shape of a value
/// expressed over an abstract iteration count `iter` (0 on the first).
/// `terms` are sorted invariant-leaf (value, coef) pairs; `kcoef` is the
/// per-iteration drift. Header params contribute their entry seed's own
/// decomposition plus `step*iter`, so a strength-reduced pointer param
/// and `iv*esize` over the index param express the same sequence.
type Dec = (Vec<(Value, i64)>, i64, i64);

/// Per-param loop behavior: uniform backedge step (None when any backedge
/// passes a non-affine value) and the seed's leaf decomposition (None
/// when entry edges pass different seeds).
type ParamInfo = (Option<i64>, Option<(Vec<(Value, i64)>, i64)>);

/// Invariant-leaf decomposition of a value: affine structure becomes
/// leaf/const terms, everything else is an opaque leaf. Never fails —
/// used on entry seeds, whose values sit outside the loop by definition.
fn dec_term(
    func: &Function,
    v: Value,
    fuel: usize,
    terms: &mut FxHashMap<Value, i64>,
    koff: &mut i64,
) {
    if fuel == 0 {
        *terms.entry(v).or_default() += 1;
        return;
    }
    let v = func.dfg.resolve_aliases(v);
    if let Some(k) = iconst(func, v) {
        *koff = koff.wrapping_add(k);
        return;
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        *terms.entry(v).or_default() += 1;
        return;
    };
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } => {
            dec_term(func, args[0], fuel - 1, terms, koff);
            dec_term(func, args[1], fuel - 1, terms, koff);
        }
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args,
        } => {
            dec_term(func, args[0], fuel - 1, terms, koff);
            let mut t2 = FxHashMap::default();
            let mut k2 = 0i64;
            dec_term(func, args[1], fuel - 1, &mut t2, &mut k2);
            for (x, c) in t2 {
                *terms.entry(x).or_default() -= c;
            }
            *koff = koff.wrapping_sub(k2);
        }
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args,
        } => {
            for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
                if let Some(k) = iconst(func, c) {
                    let mut t2 = FxHashMap::default();
                    let mut k2 = 0i64;
                    dec_term(func, x, fuel - 1, &mut t2, &mut k2);
                    for (y, d) in t2 {
                        *terms.entry(y).or_default() += d.wrapping_mul(k);
                    }
                    *koff = koff.wrapping_add(k2.wrapping_mul(k));
                    return;
                }
            }
            *terms.entry(v).or_default() += 1;
        }
        _ => {
            *terms.entry(v).or_default() += 1;
        }
    }
}

/// Add `scale * dec(v)` to (`terms`, `kcoef`, `koff`). An in-body value
/// that isn't affine and isn't a header param changes per iteration —
/// the decomposition fails.
#[allow(clippy::too_many_arguments)]
fn dec(
    func: &Function,
    params: &[Value],
    pinfos: &[ParamInfo],
    body: &FxHashSet<Block>,
    v: Value,
    scale: i64,
    fuel: usize,
    terms: &mut FxHashMap<Value, i64>,
    kcoef: &mut i64,
    koff: &mut i64,
) -> Option<()> {
    if fuel == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if let Some(j) = params.iter().position(|&p| p == v) {
        let (step, seed) = &pinfos[j];
        let (Some(s), Some((st, sk))) = (*step, seed) else {
            return None;
        };
        *kcoef = kcoef.wrapping_add(s.wrapping_mul(scale));
        *koff = koff.wrapping_add(sk.wrapping_mul(scale));
        for &(t, c) in st {
            *terms.entry(t).or_default() += c.wrapping_mul(scale);
        }
        return Some(());
    }
    if let Some(k) = iconst(func, v) {
        *koff = koff.wrapping_add(scale.wrapping_mul(k));
        return Some(());
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        // A non-header block param: invariant iff defined outside the loop.
        if def_block(func, v).is_some_and(|b| !body.contains(&b)) {
            *terms.entry(v).or_default() += scale;
            return Some(());
        }
        return None;
    };
    // Non-affine nodes (or imul/shl without a constant factor): invariant
    // leaf only when defined outside the loop — an in-body value changes
    // per iteration.
    fn leaf(
        func: &Function,
        body: &FxHashSet<Block>,
        i: Inst,
        v: Value,
        scale: i64,
        terms: &mut FxHashMap<Value, i64>,
    ) -> Option<()> {
        if func
            .layout
            .inst_block(i)
            .is_some_and(|b| !body.contains(&b))
        {
            *terms.entry(v).or_default() += scale;
            return Some(());
        }
        None
    }
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } => {
            dec(
                func,
                params,
                pinfos,
                body,
                args[0],
                scale,
                fuel - 1,
                terms,
                kcoef,
                koff,
            )?;
            dec(
                func,
                params,
                pinfos,
                body,
                args[1],
                scale,
                fuel - 1,
                terms,
                kcoef,
                koff,
            )
        }
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args,
        } => {
            dec(
                func,
                params,
                pinfos,
                body,
                args[0],
                scale,
                fuel - 1,
                terms,
                kcoef,
                koff,
            )?;
            dec(
                func,
                params,
                pinfos,
                body,
                args[1],
                scale.wrapping_neg(),
                fuel - 1,
                terms,
                kcoef,
                koff,
            )
        }
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args,
        } => {
            for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
                if let Some(k) = iconst(func, c) {
                    return dec(
                        func,
                        params,
                        pinfos,
                        body,
                        x,
                        scale.wrapping_mul(k),
                        fuel - 1,
                        terms,
                        kcoef,
                        koff,
                    );
                }
            }
            leaf(func, body, i, v, scale, terms)
        }
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args,
        } if iconst(func, args[1]).is_some_and(|s| (0..64).contains(&s)) => {
            let s = iconst(func, args[1])?;
            dec(
                func,
                params,
                pinfos,
                body,
                args[0],
                scale.wrapping_shl(s as u32),
                fuel - 1,
                terms,
                kcoef,
                koff,
            )
        }
        _ => leaf(func, body, i, v, scale, terms),
    }
}

fn dec_addr(
    func: &Function,
    params: &[Value],
    pinfos: &[ParamInfo],
    body: &FxHashSet<Block>,
    v: Value,
) -> Option<Dec> {
    let mut tm = FxHashMap::default();
    let (mut kcoef, mut koff) = (0i64, 0i64);
    dec(
        func, params, pinfos, body, v, 1, DEC_FUEL, &mut tm, &mut kcoef, &mut koff,
    )?;
    let mut terms: Vec<(Value, i64)> = tm.into_iter().filter(|&(_, c)| c != 0).collect();
    terms.sort();
    Some((terms, kcoef, koff))
}

/// Can `dec`'s iteration-0 address be evaluated at edge `e`? Every term
/// is an out-of-loop value that must dominate the branch.
fn rot_edge_ok(func: &Function, dt: &DominatorTree, dec: &Dec, e: &Edge) -> bool {
    dec.0.iter().all(|&(v, _)| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => dt.dominates(i, e.inst, &func.layout),
        ValueDef::Param(b, _) => dt.dominates(b, e.inst, &func.layout),
        _ => false,
    })
}

/// `dec`'s seed address — the first-iteration `Σ terms + koff` — in
/// `loadfwd`'s `(root, offset, type)` form, when it's a single unit term
/// (`v + koff`); multi-term seeds can't name one location.
fn seed_loc(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    (terms, _, koff): &Dec,
    ty: Type,
) -> Option<crate::loadfwd::Loc> {
    let &[(v, 1)] = &terms[..] else {
        return None;
    };
    let (r, o) = crate::loadfwd::norm_root(func, ps, na, v);
    Some((r, o.wrapping_add(*koff), ty))
}

/// The value `loc` provably holds at edge `e`, found by walking up the
/// dominator tree from the branch: the nearest dominating access wins —
/// a load of exactly `loc` gives its result, an exact-loc store gives
/// the stored value — while any other may-write to `loc`, or a
/// call/fence/atomic, ends the search. Seeding `prev` from an existing
/// access introduces no speculation, so the rotated load needn't run on
/// every iteration.
fn edge_avail(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    dt: &DominatorTree,
    loc: crate::loadfwd::Loc,
    e: &Edge,
) -> Option<Value> {
    let mut b = func.layout.inst_block(e.inst)?;
    let mut upto = Some(e.inst);
    for _ in 0..64 {
        let insts: Vec<Inst> = func
            .layout
            .block_insts(b)
            .take_while(|&i| Some(i) != upto)
            .collect();
        for &i in insts.iter().rev() {
            match func.dfg.insts[i] {
                // A load's result is usable even if it could trap: the
                // edge is only reached when it didn't.
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    ..
                } => {
                    let (r2, o2) = crate::loadfwd::norm_root(func, ps, na, arg);
                    if (r2, o2.wrapping_add(i64::from(i32::from(offset)))) == (loc.0, loc.1)
                        && func.dfg.value_type(func.dfg.first_result(i)) == loc.2
                    {
                        return Some(func.dfg.first_result(i));
                    }
                }
                InstructionData::Store { args, offset, .. } => {
                    let val = func.dfg.resolve_aliases(args[0]);
                    let (r2, o2) = crate::loadfwd::norm_root(func, ps, na, args[1]);
                    let w = (
                        r2,
                        o2.wrapping_add(i64::from(i32::from(offset))),
                        func.dfg.value_type(val),
                    );
                    if w == loc {
                        return Some(val);
                    }
                    if crate::loadfwd::may_write(w, None, loc) {
                        return None;
                    }
                }
                _ => {
                    let op = func.dfg.insts[i].opcode();
                    if op.can_store() || op.is_call() || op.other_side_effects() {
                        return None;
                    }
                }
            }
        }
        upto = None;
        b = dt.idom(b)?;
    }
    None
}

/// Whether `i` runs on every iteration that starts, so the entry edges may
/// load its first-iteration address up front: its block dominates every
/// backedge and every way out of the loop (exit edges, traps, returns).
/// Otherwise the seed load is speculative, and its address may be an
/// empty `Vec`'s dangling pointer.
fn runs_every_iteration(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    body: &FxHashSet<Block>,
    edges: &[Edge],
    i: Inst,
) -> bool {
    let Some(ib) = func.layout.inst_block(i) else {
        return false;
    };
    let latches_ok = edges
        .iter()
        .filter_map(|e| func.layout.inst_block(e.inst))
        .filter(|b| body.contains(b))
        .all(|b| dt.dominates(ib, b, &func.layout));
    latches_ok
        && body.iter().all(|&b| {
            let mut succs = cfg.succ_iter(b).peekable();
            let leaves = succs.peek().is_none() || succs.any(|s| !body.contains(&s));
            !leaves || dt.dominates(ib, b, &func.layout)
        })
}

/// The entry-edge seed for a rotated load: `Σ terms + koff` (the
/// iteration-0 address), then the load itself. Caller must have checked
/// `rot_edge_ok` first — every leaf resolves.
fn emit_rot_load(func: &mut Function, dec: &Dec, ty: Type, flags: MemFlagsData, e: &Edge) -> Value {
    let (terms, _, koff) = dec;
    let mut pos = FuncCursor::new(func).at_inst(e.inst);
    let mut acc: Option<Value> = None;
    let mut add = |pos: &mut FuncCursor, v: Value, coef: i64| {
        let t = if coef == 1 {
            v
        } else {
            let c = iconst_masked(pos, pos.func.dfg.value_type(v), coef);
            pos.ins().imul(v, c)
        };
        acc = Some(match acc {
            Some(a) => pos.ins().iadd(a, t),
            None => t,
        });
    };
    for &(v, coef) in terms {
        add(&mut pos, v, coef);
    }
    if *koff != 0 {
        let c = iconst_masked(&mut pos, cranelift_codegen::ir::types::I64, *koff);
        add(&mut pos, c, 1);
    }
    let a = match acc {
        Some(a) => a,
        None => pos.ins().iconst(types::I64, 0),
    };
    pos.ins().load(ty, flags, a, 0)
}

/// One entry edge's `prev` seed: reuse a value already materialized at
/// the edge, or emit the first-iteration load at the edge.
enum Seed {
    V(Value),
    Load,
}

fn rot_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
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
    // The carried location must be stable between `A`'s load and the next
    // iteration's read: no stores or calls anywhere in the loop.
    for &b in &body {
        for i in func.layout.block_insts(b) {
            let op = func.dfg.insts[i].opcode();
            if op.can_store()
                || op.other_side_effects()
                || matches!(
                    op,
                    Opcode::Call
                        | Opcode::CallIndirect
                        | Opcode::TryCall
                        | Opcode::TryCallIndirect
                        | Opcode::AtomicCas
                        | Opcode::AtomicRmw
                        | Opcode::AtomicLoad
                        | Opcode::Fence
                )
            {
                if debug {
                    eprintln!("ldrot {h}: bail {op} at {b}");
                }
                return 0;
            }
        }
    }
    let edges = edges_to(func, cfg, h);
    if !edges.iter().any(|e| {
        func.layout
            .inst_block(e.inst)
            .is_some_and(|b| !body.contains(&b))
    }) {
        if debug {
            eprintln!("ldrot {h}: bail no entry edge");
        }
        return 0; // no entry edge — nothing to seed `prev` from
    }
    let params = func.dfg.block_params(h).to_vec();
    let inb = |func: &Function, e: &Edge| {
        func.layout
            .inst_block(e.inst)
            .is_some_and(|b| body.contains(&b))
    };
    // Per-param (step, seed): the step must be a uniform constant on every
    // backedge, and every entry edge must pass the same seed value so the
    // param's sequence `seed + iter*step` is well-defined.
    let mut pinfos: Vec<ParamInfo> = Vec::with_capacity(params.len());
    'p: for (j, &p) in params.iter().enumerate() {
        let (mut s, mut seed) = (None, None);
        for e in &edges {
            let Some(a) = edge_arg(func, e, j) else {
                pinfos.push((None, None));
                continue 'p;
            };
            if !inb(func, e) {
                // Entry edge: all entries must seed the same value.
                let a = func.dfg.resolve_aliases(a);
                if seed.is_some_and(|x| x != a) {
                    pinfos.push((None, None));
                    continue 'p;
                }
                seed = Some(a);
                continue;
            }
            let d = if a == p {
                0
            } else {
                match step_form(func, p, &body, a) {
                    Some(StepV::K(k)) => k,
                    _ => {
                        pinfos.push((None, None));
                        continue 'p;
                    }
                }
            };
            if s.is_some_and(|x| x != d) {
                pinfos.push((None, None));
                continue 'p;
            }
            s = Some(d);
        }
        let Some(sd) = seed else {
            // Params with no entry edge can't seed a sequence.
            pinfos.push((None, None));
            continue;
        };
        let (mut tm, mut koff) = (FxHashMap::default(), 0i64);
        dec_term(func, sd, DEC_FUEL, &mut tm, &mut koff);
        let mut st: Vec<(Value, i64)> = tm.into_iter().filter(|&(_, c)| c != 0).collect();
        st.sort();
        pinfos.push((s, Some((st, koff))));
    }
    // Affine loads anywhere in the loop, grouped by (invariant terms,
    // per-iteration drift): within a group only the constant offset
    // differs. `B` (the replaced load) may sit in any body block — `prev`
    // is a pure function of the header params — but `A` must dominate the
    // backedges so its result is a legal edge arg (which also means it
    // runs each iteration).
    type Key = (Vec<(Value, i64)>, i64);
    let mut groups: FxHashMap<Key, Vec<(i64, Inst, Value, Type, Opcode)>> = FxHashMap::default();
    let mut decs: FxHashMap<Inst, Dec> = FxHashMap::default();
    for b in func.layout.blocks().filter(|b| body.contains(b)) {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Load {
                opcode: op,
                arg,
                flags,
                offset,
                ..
            } = func.dfg.insts[i]
                && func.dfg.mem_flags[flags].notrap()
                && let Some((terms, kcoef, koff)) = dec_addr(func, &params, &pinfos, &body, arg)
                && kcoef != 0
            {
                let res = func.dfg.first_result(i);
                let ty = func.dfg.value_type(res);
                let koff = koff.wrapping_add(i32::from(offset) as i64);
                decs.insert(i, (terms.clone(), kcoef, koff));
                groups
                    .entry((terms, kcoef))
                    .or_default()
                    .push((koff, i, res, ty, op));
            }
        }
    }
    if debug {
        for ((terms, kcoef), ls) in &groups {
            eprintln!(
                "ldrot {h}: group kcoef={kcoef} terms={terms:?} loads={:?}",
                ls.iter().map(|&(k, i, ..)| (k, i)).collect::<Vec<_>>()
            );
        }
    }
    if groups.is_empty() {
        return 0;
    }
    let mut removed = FxHashSet::default();
    let mut rot = 0;
    let mut n = 0;
    for ((_, shift), ls) in &groups {
        // Same-address reuse: a load whose `dec` matches an earlier
        // dominating load reads the same address this iteration — the
        // unrolled copies' `v[i-1]` against the previous copy's `v[i]`.
        for &(koff, bi, bres, bty, bop) in ls {
            if removed.contains(&bi) {
                continue;
            }
            if let Some(&(_, _, ares, _, _)) = ls.iter().find(|&&(k, i, _, t, o)| {
                k == koff
                    && t == bty
                    && o == bop
                    && i != bi
                    && !removed.contains(&i)
                    && dt.dominates(i, bi, &func.layout)
            }) {
                let ares = func.dfg.resolve_aliases(ares);
                func.layout.remove_inst(bi);
                func.dfg.clear_results(bi);
                func.dfg.change_to_alias(bres, ares);
                removed.insert(bi);
                n += 1;
            }
        }
        // `shift` is the per-iteration address drift: `B`'s address this
        // iteration is `A`'s last iteration when `koff_A = koff_B + shift`.
        if *shift == 0 || rot >= MAX_ROT {
            continue;
        }
        for &(koff, bi, _, bty, bop) in ls {
            if rot >= MAX_ROT {
                break;
            }
            if removed.contains(&bi) {
                continue;
            }
            let want = koff.wrapping_add(*shift);
            let Some(&(_, ai, ares, _, _)) = ls.iter().find(|&&(k, i, _, t, o)| {
                k == want
                    && t == bty
                    && o == bop
                    && i != bi
                    && !removed.contains(&i)
                    && edges
                        .iter()
                        .filter(|e| inb(func, e))
                        .all(|e| dt.dominates(i, e.inst, &func.layout))
            }) else {
                continue;
            };
            let InstructionData::Load { flags, .. } = func.dfg.insts[ai] else {
                continue;
            };
            let flags = func.dfg.mem_flags[flags];
            let bdec = decs[&bi].clone();
            // `prev`'s entry-edge seed: reuse the value `bdec`'s loc
            // already holds at the edge when it's avail — no speculation,
            // so `bi` needn't run every iteration — else emit the
            // first-iteration load, which is sound only when it does.
            let every = runs_every_iteration(func, cfg, dt, &body, &edges, bi);
            let sloc = seed_loc(func, ps, na, &bdec, bty);
            let mut es = Vec::new();
            let mut ok = true;
            for e in &edges {
                if inb(func, e) {
                    es.push(None);
                } else if let Some(v) =
                    sloc.and_then(|l| edge_avail(func, ps, na, dt, l, e))
                {
                    es.push(Some(Seed::V(v)));
                } else if every && rot_edge_ok(func, dt, &bdec, e) {
                    es.push(Some(Seed::Load));
                } else {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            // `prev` = phi{entry: B's iteration-0 value, backedge: A}.
            let prev = func.dfg.append_block_param(h, bty);
            for (e, seed) in edges.iter().zip(es) {
                let v = match seed {
                    Some(Seed::V(v)) => v,
                    Some(Seed::Load) => emit_rot_load(func, &bdec, bty, flags, e),
                    None => ares,
                };
                let dfg = &mut func.dfg;
                dfg.insts[e.inst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                    [e.slot]
                    .append_argument(v, &mut dfg.value_lists);
            }
            let r = func.dfg.first_result(bi);
            func.layout.remove_inst(bi);
            func.dfg.clear_results(bi);
            func.dfg.change_to_alias(r, prev);
            if debug {
                eprintln!("ldrot {h}: load {bi} -> param {prev} (feed {ares}, shift {shift})");
            }
            removed.insert(bi);
            rot += 1;
            n += 1;
        }
    }
    n
}

pub fn rotld(func: &mut Function, an: &mut crate::pipeline::Analysis) -> usize {
    let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
    let debug = crate::env_set("PLIRON_LDROT_DEBUG");
    let ps = crate::loadfwd::param_src(func);
    let na = FxHashSet::default();
    let loops = deepest_first(la);
    let mut n = 0;
    for lp in loops.into_iter().take(MAX_LOOPS) {
        n += rot_loop(func, cfg, dt, la, lp, &ps, &na, debug);
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
/// loop trades two remat instructions for one `isub`. Bounds `K < kmin`
/// fold into the compare itself (x64's `cmp reg, imm32`), where the
/// countdown's extra param and `isub` are pure overhead.
pub fn boundcmp(func: &mut Function, an: &mut crate::pipeline::Analysis, kmin: i64) -> usize {
    let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
    let psrc = crate::loadfwd::param_src(func);
    let debug = crate::env_set("PLIRON_INDUCT_DEBUG");
    let mut n = 0;
    for lp in deepest_first(la).into_iter().take(MAX_LOOPS) {
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
        let edges = edges_to(func, cfg, h);
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
                let Some(k) = (e.slot == 0 && blocks[0].block(&func.dfg.value_lists) == h)
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
                                    iconst(func, y).filter(|k| (kmin..=i64::MAX).contains(k))
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
                let bc = &mut dfg.insts[entry.inst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                    [entry.slot];
                bc.append_argument(rem0, &mut dfg.value_lists);
            }
            // One `rem - s` per latch, emitted before the compare `li`
            // (which dominates the brif it feeds) so the backedge arg and
            // the rewritten test `ule (D - a), rem0` share it.
            for (e, li, step) in &latches {
                let v = match step {
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
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[e.inst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                    [e.slot];
                bc.append_argument(v, &mut dfg.value_lists);
                if let InstructionData::IntCompare { cond, .. } = &mut func.dfg.insts[*li] {
                    *cond = IntCC::UnsignedLessThanOrEqual;
                }
                let args = func.dfg.inst_args_mut(*li);
                args[0] = v;
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
        cond, args: [x, y], ..
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
    (def_block(func, s).is_some_and(|d| !body.contains(&d)) && bounded(func, dt, psrc, i, s))
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
                cond, args: [a, b], ..
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

/// `icmp cc p, K` at a count latch whose backedge arg is `p + s` (const
/// `0 < s <= smax`) is rewritten to `icmp cc (p+s), K+s` — the test moves
/// onto the already-computed step value so the latch lowers to
/// `add $s; cmp $K+s; jcc` on one register instead of a `lea` step plus
/// `cmp` on the pre-inc value (which pins the old count in a second
/// register).
///
/// `p cc K` ⟺ `(p+s) cc (K+s)` needs `p + s` not to wrap where the
/// compare sees it. Entries must feed args provably `<= K` (an `iconst`,
/// or a `count_tested`-shaped `ult`/`ule` guard on the edge to `h`);
/// latch args are `p + s` with `p cc K` tested, so the compare operand
/// is `<= K + s` and its step `<= K + 2s`, gated to fit `i64`.
///
/// Rewriting to the post-inc form also produces the latch shape
/// `boundcmp` expects, so a later countdown pass can still fire.
pub fn postinc(func: &mut Function, an: &mut crate::pipeline::Analysis, smax: i64) -> usize {
    let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
    let dbg = std::env::var("PLIRON_POSTINC_DEBUG").is_ok();
    let mut n = 0;
    for lp in deepest_first(la).into_iter().take(MAX_LOOPS) {
        let h = la.loop_header(lp);
        if !dt.is_reachable(h) {
            continue;
        }
        let body: FxHashSet<Block> = func
            .layout
            .blocks()
            .filter(|&b| la.is_in_loop(b, lp))
            .collect();
        let edges = edges_to(func, cfg, h);
        for (idx, &p) in func.dfg.block_params(h).to_vec().iter().enumerate() {
            let ty = func.dfg.value_type(p);
            if !ty.is_int() || ty.is_vector() {
                continue;
            }
            let mut latches = Vec::new();
            let mut entries = Vec::new();
            for e in &edges {
                let eb = func.layout.inst_block(e.inst).unwrap();
                if body.contains(&eb) {
                    latches.push(e.clone());
                } else {
                    entries.push(e.clone());
                }
            }
            if latches.is_empty() {
                if dbg && !entries.is_empty() {
                    eprintln!("postinc {h} p{p}: no latch edge");
                }
                continue;
            }
            // Every latch must test `p` below a common `K` on the edge to
            // `h`, with backedge arg `p + s` dominating the compare.
            let mut k_bound = None;
            let mut rewrite = Vec::new();
            let mut ok = true;
            for e in &latches {
                let Some(a) = edge_arg(func, e, idx) else {
                    if dbg {
                        eprintln!("postinc {h} p{p}: latch edge arg not a value");
                    }
                    ok = false;
                    break;
                };
                let Some((k, _lt, ci)) = count_tested(func, e.inst, e.slot, p, h)
                    .filter(|(k, _, _)| k_bound.is_none_or(|b| b == *k))
                else {
                    if dbg {
                        eprintln!("postinc {h} p{p}: latch {} not `p cc K`", e.inst);
                    }
                    ok = false;
                    break;
                };
                let Some(StepV::K(s)) = step_form(func, p, &body, a) else {
                    if dbg {
                        eprintln!("postinc {h} p{p}: backedge arg {a} not `p + const`");
                    }
                    ok = false;
                    break;
                };
                if s <= 0 || s > smax || k > i64::MAX - 2 * s {
                    if dbg {
                        eprintln!("postinc {h} p{p}: s={s} k={k} rejected");
                    }
                    ok = false;
                    break;
                }
                k_bound = Some(k);
                rewrite.push((e.clone(), ci, a, k + s));
            }
            // Entries must feed args provably `<= K`: an `iconst`, or a
            // `count_tested`-style `ult`/`ule` guard on the edge to `h`.
            if ok && let Some(k) = k_bound {
                for e in &entries {
                    let bounded = edge_arg(func, e, idx).is_some_and(|a| {
                        iconst(func, a).is_some_and(|ka| ka <= k)
                            || count_tested(func, e.inst, e.slot, a, h)
                                .is_some_and(|(kg, lt, _)| kg - lt as i64 <= k)
                    });
                    if !bounded {
                        if dbg {
                            eprintln!("postinc {h} p{p}: entry {} arg unbounded", e.inst);
                        }
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            if dbg {
                eprintln!("postinc {h} p{p}: rewriting {} compares", rewrite.len());
            }
            for (e, ci, a, k) in rewrite {
                let dom = match func.dfg.value_def(a) {
                    ValueDef::Result(di, _) => ProgramPoint::Inst(di),
                    ValueDef::Param(db, _) => ProgramPoint::Block(db),
                    ValueDef::Union(_, _) => continue,
                };
                if !dt.dominates(dom, ci, &func.layout) {
                    // `a`'s def sits after `ci` in the latch — move the
                    // compare down beside its new operand, safe while the
                    // brif is its only use.
                    let Some(&cr) = func.dfg.inst_results(ci).first() else {
                        continue;
                    };
                    let single = func
                        .layout
                        .blocks()
                        .flat_map(|b| func.layout.block_insts(b))
                        .all(|i| {
                            i == e.inst
                                || !func
                                    .dfg
                                    .inst_values(i)
                                    .any(|v| func.dfg.resolve_aliases(v) == cr)
                        });
                    if !single {
                        continue;
                    }
                    func.layout.remove_inst(ci);
                    func.layout.insert_inst(ci, e.inst);
                }
                let mut pos = FuncCursor::new(func).at_inst(ci);
                let ks = iconst_masked(&mut pos, ty, k);
                if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[ci] {
                    *args = [a, ks];
                }
                n += 1;
            }
        }
    }
    n
}

/// Reassociate `v` to `(root, k)` by walking `iadd`/`isub` iconst
/// chains: `iadd(v, c)` → `v + c`, `isub(v, c)` → `v - c`.
fn lin(func: &Function, mut v: Value) -> (Value, i64) {
    let mut k = 0i64;
    for _ in 0..16 {
        let rv = func.dfg.resolve_aliases(v);
        let step = match func.dfg.value_def(rv) {
            ValueDef::Result(d, _) => match func.dfg.insts[d] {
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args: [a, b],
                } => iconst(func, b)
                    .map(|c| (a, c))
                    .or_else(|| iconst(func, a).map(|c| (b, c))),
                InstructionData::Binary {
                    opcode: Opcode::Isub,
                    args: [a, b],
                } => iconst(func, b).map(|c| (a, -c)),
                _ => None,
            },
            _ => None,
        };
        match step {
            Some((nv, c)) => {
                v = nv;
                k += c;
            }
            None => return (rv, k),
        }
    }
    (func.dfg.resolve_aliases(v), k)
}

/// What `x - y` provably equals at a program point, for `relparm`'s
/// diff proofs.
pub(crate) enum Diff {
    /// `x - y = v` for a concrete value `v`.
    Lit(Value),
    /// `x - y = p_i - p_j` — the loop-head pair's own diff. Valid only
    /// where the head invariant is in scope (inside the loop body),
    /// which is also the only place `p_i`/`p_j` are visible.
    Head,
}

/// Prove `x - y` as a `Diff` by symbolic subtraction. Grammar:
///
/// - `x - 0 = x`, `x - isub(x, B) = B`, `iadd(y, B) - y = B`
///   (literal forms)
/// - `p_i - p_j` -> `Head`
/// - `f(a, c) - f(b, c)` with a shared operand -> recurse on `(a, b)`:
///   `iadd` cancels any shared operand, `isub` only a shared rhs (a
///   shared lhs negates the diff)
/// - `x`, `y` params of the same block `q` -> the diff at `q`'s exits
///   is the join of every in-edge's arg diff; recurse per edge and
///   require agreement (`Head` edges defer to the established `B`)
pub(crate) fn diff_b(
    func: &Function,
    cfg: &ControlFlowGraph,
    pi: Value,
    pj: Value,
    ai: Value,
    aj: Value,
    depth: u32,
    dbg: bool,
) -> Option<Diff> {
    let x = func.dfg.resolve_aliases(ai);
    let y = func.dfg.resolve_aliases(aj);
    if iconst(func, y) == Some(0) {
        return Some(Diff::Lit(x));
    }
    if let ValueDef::Result(d, _) = func.dfg.value_def(y)
        && let InstructionData::Binary {
            opcode: Opcode::Isub,
            args: [a, b],
        } = func.dfg.insts[d]
        && func.dfg.resolve_aliases(a) == x
    {
        return Some(Diff::Lit(func.dfg.resolve_aliases(b)));
    }
    if let ValueDef::Result(d, _) = func.dfg.value_def(x)
        && let InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } = func.dfg.insts[d]
    {
        if func.dfg.resolve_aliases(a) == y {
            return Some(Diff::Lit(func.dfg.resolve_aliases(b)));
        }
        if func.dfg.resolve_aliases(b) == y {
            return Some(Diff::Lit(func.dfg.resolve_aliases(a)));
        }
    }
    if x == pi && y == pj {
        return Some(Diff::Head);
    }
    if depth == 0 {
        return None;
    }
    // `y = y0 + (x - d)` ⇒ `x - y = d - y0` (and the `x` mirror). This
    // is the un-folded byte-length form: `pos + (cur' - cur)`.
    let isub_of = |v: Value| -> Option<(Value, Value)> {
        match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
            ValueDef::Result(d, _) => match func.dfg.insts[d] {
                InstructionData::Binary {
                    opcode: Opcode::Isub,
                    args: [a, b],
                } => Some((func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b))),
                _ => None,
            },
            _ => None,
        }
    };
    let iadd_of = |v: Value| -> Option<(Value, Value)> {
        match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
            ValueDef::Result(d, _) => match func.dfg.insts[d] {
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args: [a, b],
                } => Some((func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b))),
                _ => None,
            },
            _ => None,
        }
    };
    if let Some((y0, y1)) = iadd_of(y) {
        for (other, maybe) in [(y0, y1), (y1, y0)] {
            if let Some((a, d)) = isub_of(maybe)
                && a == x
            {
                return diff_b(func, cfg, pi, pj, d, other, depth - 1, dbg);
            }
        }
    }
    if let Some((x0, x1)) = iadd_of(x) {
        for (other, maybe) in [(x0, x1), (x1, x0)] {
            if let Some((a, d)) = isub_of(maybe)
                && a == y
            {
                return diff_b(func, cfg, pi, pj, other, d, depth - 1, dbg);
            }
        }
    }
    // Equal const-offset strips: `lin` reassociates `v` to `(root, k)`
    // through `iadd`/`isub` iconst chains; matching `k`s cancel.
    let (rx, cx) = lin(func, x);
    let (ry, cy) = lin(func, y);
    if cx == cy && (rx, ry) != (x, y) {
        return diff_b(func, cfg, pi, pj, rx, ry, depth - 1, dbg);
    }
    // Shared-operand strip: `f(a, c) - f(b, c) = a - b`.
    let bin = |v: Value| -> Option<(Opcode, Value, Value)> {
        match func.dfg.value_def(v) {
            ValueDef::Result(d, _) => match func.dfg.insts[d] {
                InstructionData::Binary {
                    opcode,
                    args: [a, b],
                } if opcode == Opcode::Iadd || opcode == Opcode::Isub => Some((
                    opcode,
                    func.dfg.resolve_aliases(a),
                    func.dfg.resolve_aliases(b),
                )),
                _ => None,
            },
            _ => None,
        }
    };
    if let (Some((ox, xa, xb)), Some((oy, ya, yb))) = (bin(x), bin(y)) {
        let pair = match (ox, oy) {
            (Opcode::Iadd, Opcode::Iadd) if xa == ya => Some((xb, yb)),
            (Opcode::Iadd, Opcode::Iadd) if xa == yb => Some((xb, ya)),
            (Opcode::Iadd, Opcode::Iadd) if xb == ya => Some((xa, yb)),
            (Opcode::Iadd, Opcode::Iadd) if xb == yb => Some((xa, ya)),
            (Opcode::Isub, Opcode::Isub) if xb == yb => Some((xa, ya)),
            _ => None,
        };
        if let Some((nx, ny)) = pair {
            return diff_b(func, cfg, pi, pj, nx, ny, depth - 1, dbg);
        }
    }
    // Param-pair chase: `x`, `y` params of the same block — the diff at
    // its exits is the join of every in-edge's arg diff.
    if let (ValueDef::Param(q, ix2), ValueDef::Param(q2, jx2)) =
        (func.dfg.value_def(x), func.dfg.value_def(y))
        && q == q2
    {
        let mut bound = None;
        let mut head = false;
        for e2 in &edges_to(func, cfg, q) {
            let (Some(a2), Some(b2)) =
                (edge_arg(func, e2, ix2), edge_arg(func, e2, jx2))
            else {
                if dbg {
                    eprintln!("  diff_b {x}/{y} @{q}: e{} no args@{ix2},{jx2}", e2.inst);
                }
                return None;
            };
            match diff_b(func, cfg, pi, pj, a2, b2, depth - 1, dbg) {
                Some(Diff::Lit(bv)) => match bound {
                    None => bound = Some(bv),
                    Some(p) if p == bv => {}
                    _ => {
                        if dbg {
                            eprintln!("  diff_b {x}/{y} @{q}: e{} B {bv}!={bound:?}", e2.inst);
                        }
                        return None;
                    }
                },
                Some(Diff::Head) => head = true,
                None => {
                    if dbg {
                        eprintln!(
                            "  diff_b {x}/{y} @{q}: e{} args {a2}/{b2} fail",
                            e2.inst
                        );
                    }
                    return None;
                }
            }
        }
        return match bound {
            Some(bv) => Some(Diff::Lit(bv)),
            None if head => Some(Diff::Head),
            _ => None,
        };
    }
    if dbg {
        eprintln!("  diff_b {x}/{y}: no rule (d{depth})");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{
        AbiParam, Function, InstBuilder, MemFlagsData, Signature, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    /// ipnsort's run-detection scan: the `i < len` guard exits `head`
    /// before `body`, so `body`'s `v[i-1]` load fails
    /// `runs_every_iteration` — yet `v[i0-1]` (seed `i0 = 2`, i.e.
    /// `v[1]` at `base + 8`) was already loaded on the entry path, so
    /// `prev` can seed from that value and the rotation is still sound.
    #[test]
    fn ldrot_seeds_prev_from_available_loc() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        let body = func.dfg.make_block();
        let latch = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [entry, h, body, latch, done] {
            func.layout.append_block(b);
        }
        let base = func.dfg.append_block_param(entry, types::I64);
        let len = func.dfg.append_block_param(entry, types::I64);
        let i = func.dfg.append_block_param(h, types::I64);

        // `v[1]` is already read on the entry path; `prev` seeds from it.
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        cur.ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), base, 8);
        let two = cur.ins().iconst(types::I64, 2);
        cur.ins().jump(h, &[two.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let g = cur.ins().icmp(IntCC::UnsignedLessThan, i, len);
        cur.ins().brif(g, body, &[], done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(body);
        let eight = cur.ins().iconst(types::I64, 8);
        let off = cur.ins().imul(i, eight);
        let a_cur = cur.ins().iadd(base, off);
        let one = cur.ins().iconst(types::I64, 1);
        let im1 = cur.ins().isub(i, one);
        let off2 = cur.ins().imul(im1, eight);
        let a_prev = cur.ins().iadd(base, off2);
        let la = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), a_cur, 0);
        let lb = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), a_prev, 0);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, la, lb);
        cur.ins().brif(c, latch, &[], done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(latch);
        let i2 = cur.ins().iadd(i, one);
        cur.ins().jump(h, &[i2.into()]);

        FuncCursor::new(&mut func)
            .at_bottom(done)
            .ins()
            .return_(&[]);

        let mut an = crate::pipeline::Analysis::default();
        assert_eq!(rotld(&mut func, &mut an), 1);
        // `h` gained the `prev` param; `lb` folded into it, leaving the
        // entry seed load and `la` as the only loads.
        assert_eq!(func.dfg.block_params(h).len(), 2);
        let nloads = func
            .layout
            .blocks()
            .map(|b| {
                func.layout
                    .block_insts(b)
                    .filter(|&i| {
                        matches!(func.dfg.insts[i], InstructionData::Load { .. })
                    })
                    .count()
            })
            .sum::<usize>();
        assert_eq!(nloads, 2);
        let flags = cranelift_codegen::settings::Flags::new(
            cranelift_codegen::settings::builder(),
        );
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("ldrot produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `icmp p, K` where the backedge arg `p + s` is defined *after* the
    /// compare in the latch (paramsink's re-based form): the compare
    /// moves down beside the brif and retargets to `icmp (p+s), K+s`.
    #[test]
    fn postinc_moves_count_test_onto_step() {
        let mut func = Function::with_name_signature(
            UserFuncName::user(0, 0),
            Signature::new(CallConv::Fast),
        );
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [entry, h, done] {
            func.layout.append_block(b);
        }
        let p = func.dfg.append_block_param(h, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().jump(h, &[z.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let k = cur.ins().iconst(types::I64, 5000);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, p, k);
        let one = cur.ins().iconst(types::I64, 1);
        let a = cur.ins().iadd(p, one);
        cur.ins().brif(c, h, &[a.into()], done, &[]);

        FuncCursor::new(&mut func)
            .at_bottom(done)
            .ins()
            .return_(&[]);

        let mut an = crate::pipeline::Analysis::default();
        assert_eq!(postinc(&mut func, &mut an, 1024), 1);

        // `icmp ult (p+1), 5001` is the last inst before the brif.
        let brif = func.layout.block_insts(h).last().unwrap();
        let ci = func.layout.prev_inst(brif).unwrap();
        let InstructionData::IntCompare {
            cond,
            args: [x, y],
            ..
        } = func.dfg.insts[ci]
        else {
            panic!("no compare before brif: {}", func.display());
        };
        assert_eq!(cond, IntCC::UnsignedLessThan);
        assert_eq!(func.dfg.resolve_aliases(x), a);
        assert_eq!(iconst(&func, y), Some(5001));

        let flags = cranelift_codegen::settings::Flags::new(
            cranelift_codegen::settings::builder(),
        );
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("postinc produced invalid IR: {e}\n{}", func.display());
        }
    }

}

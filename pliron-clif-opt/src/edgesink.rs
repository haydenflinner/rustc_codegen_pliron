//! Sink pure insts used only as one `brif` edge's args onto that edge
//! (`PLIRON_EDGESINK`).
//!
//! Packed state for the path not taken — `ParseIntError`'s
//! `band`/`bor` field chains built before the digit-validity `brif`
//! (parse_only), tagged-union payloads for the error arm — executes
//! unconditionally when it sits in the predecessor, and lengthens any
//! loop-carried chain through it. The value's only use is the edge
//! arg to the cold successor, so the work is provably dead on the
//! other edge: clone it into a fresh edge block and it runs only when
//! the edge is actually taken.
//!
//! The set grows backward: a sunk inst's own operands sink too while
//! they are pure, defined in the same block, and used nowhere outside
//! the set (a shared `band` feeding both edges' values stays put).
//! Operands stay valid because the edge block's sole predecessor is
//! the original block — everything defined there dominates it.
//!
//! Only `brif` edges qualify: a `jump`'s single edge always executes,
//! so sinking would only add a block. `br_table` edges live in the
//! jump-table pool and `try_call` edges carry pseudo-value args —
//! neither is touched.
//!
//! Loop-exit sinking (`exit_sink`, `PLIRON_EXITSINK`, default on) is
//! the same idea for a value several exits read: a pure in-loop inst
//! whose every use is outside its loop (or an arg on an edge leaving
//! it) is cloned to those uses and leaves the loop. See `exit_sink`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::jumpthread::pure_op;

/// Cap on insts moved onto one edge — beyond this the edge clone
/// bloats code size for no measurable win.
const MAX_SINK: usize = 16;

/// Cap on use blocks a shared def clones into in the cold-push phase.
/// Beyond this the I$ cost outweighs the saving — the clones only run
/// when their edge is taken, so the dynamic work is never more than
/// the original; the bound is purely about code size.
const MAX_PUSH_BLOCKS: usize = 8;

/// Total uses per value (`inst_values` covers terminator edge args).
fn use_counts(func: &Function) -> FxHashMap<Value, u32> {
    let mut m: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *m.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    m
}

/// `v`'s defining inst if it lives in `b`, is pure, and has a single
/// result. Also (`PLIRON_EDGESINK_LOADS=0` disables) a non-trapping load
/// with nothing that may write memory between it and `b`'s terminator:
/// on the edge, memory is what it was at the load (a loop's length
/// reload used only by its exit edge, num-bigint-dig's montgomery).
fn sinkable_def(func: &Function, b: Block, v: Value) -> Option<Inst> {
    match func.dfg.value_def(v) {
        ValueDef::Result(d, _)
            if func.layout.inst_block(d) == Some(b)
                && func.dfg.inst_results(d).len() == 1
                && (pure_op(func, d) || quiet_load(func, d)) =>
        {
            Some(d)
        }
        _ => None,
    }
}

fn quiet_loads_on() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| pliron_cfg::pass_enabled("PLIRON_EDGESINK_LOADS"));
    *ON
}

/// Edge `edge` of `b`'s terminator leaves `b`'s innermost loop.
fn exits_loop(func: &Function, la: &LoopAnalysis, b: Block, edge: usize) -> bool {
    let Some(lp) = la.innermost_loop(b) else { return false };
    let t = func.layout.last_inst(b).unwrap();
    let calls = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
    !la.is_in_loop(calls[edge].block(&func.dfg.value_lists), lp)
}

/// A `notrap` load followed in its block only by instructions that
/// neither write memory nor have other side effects.
fn quiet_load(func: &Function, d: Inst) -> bool {
    if !quiet_loads_on() {
        return false;
    }
    let InstructionData::Load { opcode, flags, .. } = func.dfg.insts[d] else {
        return false;
    };
    if !matches!(opcode, Opcode::Load | Opcode::Uload8 | Opcode::Sload8 | Opcode::Uload16 | Opcode::Sload16 | Opcode::Uload32 | Opcode::Sload32)
        // Volatile accesses are lowered without `notrap`.
        || !func.dfg.mem_flags[flags].notrap()
    {
        return false;
    }
    let mut cur = func.layout.next_inst(d);
    while let Some(i) = cur {
        let op = func.dfg.insts[i].opcode();
        if !op.is_terminator() && (op.can_store() || op.is_call() || op.other_side_effects() || op.can_trap()) {
            return false;
        }
        cur = func.layout.next_inst(i);
    }
    true
}

/// The set of `b`-insts movable onto edge `edge` of terminator `t`,
/// in `b`'s layout order. `None` when the edge carries no sinkable
/// arg or the chain exceeds `MAX_SINK`.
fn sink_set(
    func: &Function,
    uses: &FxHashMap<Value, u32>,
    b: Block,
    edge: usize,
) -> Option<Vec<Inst>> {
    let t = func.layout.last_inst(b)?;
    let data = &func.dfg.insts[t];
    let calls = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
    let call = *calls.get(edge)?;
    let pool = &func.dfg.value_lists;
    // Raw args move verbatim to the edge block's jump; a non-`Value`
    // arg can't come from `brif`, but bail rather than assume.
    let mut args = Vec::with_capacity(call.len(pool));
    let mut occ: FxHashMap<Value, u32> = FxHashMap::default();
    for a in call.args(pool) {
        let v = a.as_value()?;
        args.push(v);
        *occ.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
    }
    if args.is_empty() {
        return None;
    }
    // A value sinks iff every use of it is an arg position on this
    // edge (seed) or an operand of another sunk inst (chain).
    let mut in_set: FxHashSet<Value> = FxHashSet::default();
    let mut queue: Vec<Value> = Vec::new();
    for (&v, &o) in &occ {
        if uses.get(&v).copied().unwrap_or(0) == o && sinkable_def(func, b, v).is_some() {
            queue.push(v);
        }
    }
    while let Some(v) = queue.pop() {
        if !in_set.insert(v) {
            continue;
        }
        let Some(d) = sinkable_def(func, b, v) else {
            continue;
        };
        if in_set.len() > MAX_SINK {
            return None;
        }
        for w in func.dfg.inst_args(d) {
            let wr = func.dfg.resolve_aliases(*w);
            if in_set.contains(&wr) {
                continue;
            }
            // `wr` joins the set when all its uses are inside it:
            // operands of sunk insts plus its own edge-arg
            // occurrences (those uses move to the edge block's jump).
            let mut cnt = occ.get(&wr).copied().unwrap_or(0);
            for sv in &in_set {
                let Some(sd) = sinkable_def(func, b, *sv) else {
                    continue;
                };
                cnt += func
                    .dfg
                    .inst_args(sd)
                    .iter()
                    .filter(|a| func.dfg.resolve_aliases(**a) == wr)
                    .count() as u32;
            }
            if cnt == uses.get(&wr).copied().unwrap_or(0) && sinkable_def(func, b, wr).is_some() {
                queue.push(wr);
            }
        }
    }
    if in_set.is_empty() {
        return None;
    }
    // Emit the set in `b`'s layout order so intra-set dependencies
    // keep their def-before-use order.
    let mut out = Vec::with_capacity(in_set.len());
    for i in func.layout.block_insts(b) {
        if let Some(&r) = func.dfg.inst_results(i).first()
            && in_set.contains(&func.dfg.resolve_aliases(r))
        {
            out.push(i);
        }
    }
    Some(out).filter(|s| !s.is_empty())
}

/// Move `set` onto a fresh edge block between `b`'s edge `edge` and
/// its original destination.
fn apply(func: &mut Function, b: Block, edge: usize, set: &[Inst]) -> bool {
    let t = func.layout.last_inst(b).unwrap();
    let (dest, args) = {
        let data = &func.dfg.insts[t];
        let calls = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        let call = calls[edge];
        let pool = &func.dfg.value_lists;
        let args: Vec<Value> = call.args(pool).map(|a| a.as_value().unwrap()).collect();
        (call.block(pool), args)
    };
    let mid = func.dfg.make_block();
    func.layout.insert_block_after(mid, b);
    for &d in set {
        func.layout.remove_inst(d);
        func.layout.append_inst(d, mid);
    }
    let barg: Vec<BlockArg> = args.iter().map(|&v| BlockArg::Value(v)).collect();
    FuncCursor::new(func).at_bottom(mid).ins().jump(dest, &barg);
    let dfg = &mut func.dfg;
    dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[edge] =
        BlockCall::new(mid, [], &mut dfg.value_lists);
    true
}

/// Phase 2: edge sinking leaves shared producers behind — `band acc,
/// mask` feeding `bor`s now living in several edge blocks all hang off
/// the hot block. Clone such a def into each block that uses it when
/// every use sits in a strictly-dominated block at the same loop
/// depth (never sink into a deeper loop — that trades the def's one
/// execution for per-iteration work).
fn push_to_use_blocks(func: &mut Function, dt: &DominatorTree, la: &LoopAnalysis) -> usize {
    // value -> use blocks; `inst_values` covers terminator edge args,
    // attributing them to the pred block (a `b`-edge arg makes `b` a
    // use block, which the `ub.contains(&b)` test then rejects).
    let mut use_in: FxHashMap<Value, FxHashSet<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                use_in
                    .entry(func.dfg.resolve_aliases(v))
                    .or_default()
                    .insert(b);
            }
        }
    }
    let mut pushed = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let Some(&vr) = func.dfg.inst_results(i).first() else {
                continue;
            };
            if !pure_op(func, i) || func.dfg.inst_results(i).len() != 1 {
                continue;
            }
            let v = func.dfg.resolve_aliases(vr);
            if v != vr {
                continue;
            }
            let Some(ub) = use_in.get(&v) else {
                continue;
            };
            if ub.is_empty() || ub.len() > MAX_PUSH_BLOCKS || ub.contains(&b) {
                continue;
            }
            let ubs: Vec<Block> = ub.iter().copied().collect();
            let my_loop = la.innermost_loop(b);
            if !ubs.iter().all(|u| {
                *u != b && la.innermost_loop(*u) == my_loop && dt.try_dominates(b, *u, &func.layout)
            }) {
                continue;
            }
            // Clone `i` into each use block at its head. `i`'s operands
            // dominated `b`, and `b` dominates every use block — but an
            // earlier push in this same pass may already have sunk an
            // operand's def out of `b`, so re-check each operand's def
            // block against each use block.
            let ops_ok = ubs.iter().all(|u| {
                func.dfg
                    .inst_args(i)
                    .iter()
                    .all(|w| match func.dfg.resolved_value_def(*w) {
                        ValueDef::Result(d, _) => func
                            .layout
                            .inst_block(d)
                            .is_some_and(|db| db != *u && dt.try_dominates(db, *u, &func.layout)),
                        ValueDef::Param(pb, _) => pb == *u || dt.try_dominates(pb, *u, &func.layout),
                        _ => false,
                    })
            });
            if !ops_ok {
                continue;
            }
            let data = func.dfg.insts[i];
            let ty = func.dfg.ctrl_typevar(i);
            for &u in &ubs {
                let ni = func.dfg.make_inst(data);
                func.set_srcloc(ni, func.srcloc(i));
                func.dfg.make_inst_results(ni, ty);
                match func.layout.first_inst(u) {
                    Some(first) => func.layout.insert_inst(ni, first),
                    None => func.layout.append_inst(ni, u),
                }
                let nv = func.dfg.first_result(ni);
                let mut raw = FxHashSet::default();
                for ui in func.layout.block_insts(u) {
                    for w in func.dfg.inst_values(ui) {
                        if func.dfg.resolve_aliases(w) == v {
                            raw.insert(w);
                        }
                    }
                }
                for ui in func.layout.block_insts(u).collect::<Vec<_>>() {
                    func.dfg
                        .map_inst_values(ui, |w| if raw.contains(&w) { nv } else { w });
                }
            }
            func.layout.remove_inst(i);
            func.dfg.clear_results(i);
            // Keep `use_in` honest for later defs in this sweep: the
            // clones add uses of `i`'s operands in each `u`, and `i`'s
            // removal drops their use in `b` — but only when `i` was the
            // last user: a sibling inst in `b` can still read the operand
            // (an `iadd` sunk past a same-block `icmp` on the same value
            // left the operand dangling once the def pushed out of `b`,
            // verifier: "defined by inst which has no block"). Check the
            // remaining insts in `b` before dropping the entry.
            for w in func.dfg.inst_args(i).to_vec() {
                let wr = func.dfg.resolve_aliases(w);
                use_in.entry(wr).or_default().extend(ubs.iter().copied());
                let still_used = func.layout.block_insts(b).any(|ui| {
                    func.dfg
                        .inst_values(ui)
                        .any(|x| func.dfg.resolve_aliases(x) == wr)
                });
                if !still_used {
                    use_in.entry(wr).or_default().remove(&b);
                }
            }
            pushed += 1;
        }
    }
    pushed
}

pub fn run(func: &mut Function) -> usize {
    let uses = use_counts(func);
    let mut changed = 0;
    // A load on the edge saves work only when the edge leaves a loop
    // (the per-iteration reload becomes one at exit) from a block with
    // no call. Anywhere else it runs as often as before, and the new
    // block reshapes the live ranges around it: after a call (slot
    // reloads of its out-params) pulldown's `parse_block` lost the
    // register for its byte-scan index across the call, +19%.
    // Computed on the first load candidate. Edge blocks split edges, so
    // earlier and later ones leave the original blocks' loops as they
    // were; only original blocks are queried.
    let mut la: Option<LoopAnalysis> = None;
    // Snapshot: edge blocks we add end in `jump` and are never
    // rescanned, so in-place iteration is safe but needlessly fuzzy.
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if !matches!(func.dfg.insts[t], InstructionData::Brif { .. }) {
            continue;
        }
        for edge in 0..2 {
            let Some(set) = sink_set(func, &uses, b, edge) else {
                continue;
            };
            if set.iter().any(|&d| !pure_op(func, d)) {
                let la = la.get_or_insert_with(|| {
                    let cfg = ControlFlowGraph::with_function(func);
                    let dt = DominatorTree::with_function(func, &cfg);
                    let mut la = LoopAnalysis::new();
                    la.compute(func, &cfg, &dt);
                    la
                });
                if !exits_loop(func, la, b, edge)
                    || func.layout.block_insts(b).any(|i| func.dfg.insts[i].opcode().is_call())
                {
                    continue;
                }
            }
            if apply(func, b, edge, &set) {
                changed += set.len();
            }
        }
    }
    let (cfg, dt, la) = analyses(func);
    let sunk = exit_sink(func, &cfg, &dt, &la);
    // Exit sinking splits edges: the push phase needs the new blocks.
    let (dt, la) = if sunk > 0 {
        let (_, dt, la) = analyses(func);
        (dt, la)
    } else {
        (dt, la)
    };
    changed + sunk + push_to_use_blocks(func, &dt, &la)
}

fn analyses(func: &Function) -> (ControlFlowGraph, DominatorTree, LoopAnalysis) {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    (cfg, dt, la)
}

fn exit_sink_on() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| pliron_cfg::pass_enabled("PLIRON_EXITSINK"));
    *ON
}

/// A loop-header param's step: every back edge passes `q = w + k`.
#[derive(Clone, Copy)]
struct Bump {
    q: Value,
    k: i64,
    /// Block defining `q`.
    qb: Block,
}

fn iconst_of(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

/// `w` is an `i64` param of `lp`'s header that every back edge steps
/// by one constant.
fn iv_bump(func: &Function, cfg: &ControlFlowGraph, la: &LoopAnalysis, lp: Loop, w: Value) -> Option<Bump> {
    let ValueDef::Param(h, j) = func.dfg.value_def(w) else {
        return None;
    };
    if h != la.loop_header(lp) || func.dfg.value_type(w) != I64 {
        return None;
    }
    let pool = &func.dfg.value_lists;
    let mut q: Option<Value> = None;
    for p in cfg.pred_iter(h) {
        if !la.is_in_loop(p.block, lp) {
            continue;
        }
        for c in func.dfg.insts[p.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
            if c.block(pool) != h {
                continue;
            }
            let a = func.dfg.resolve_aliases(c.args(pool).nth(j)?.as_value()?);
            match q {
                None => q = Some(a),
                Some(x) if x == a => {}
                _ => return None,
            }
        }
    }
    let q = q?;
    let ValueDef::Result(qi, _) = func.dfg.value_def(q) else {
        return None;
    };
    let InstructionData::Binary { opcode, args } = func.dfg.insts[qi] else {
        return None;
    };
    let (a0, a1) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
    let k = match opcode {
        Opcode::Iadd if a0 == w => iconst_of(func, a1)?,
        Opcode::Iadd if a1 == w => iconst_of(func, a0)?,
        Opcode::Isub if a0 == w => iconst_of(func, a1)?.checked_neg()?,
        _ => return None,
    };
    Some(Bump { q, k, qb: func.layout.inst_block(qi)? })
}

/// Loop-exit sinking.
///
/// Shape: a pure inst in a loop block whose result no in-loop code
/// reads — every use is an inst outside the loop, an arg on a `brif`
/// edge leaving the loop, or an operand of another such inst:
///
/// ```text
/// h(p):  n = isub p, base        ; the count, wanted only after the loop
///        q = iadd p, 1
///        c = load p
///        brif c == x, latch, exit(n)
/// latch: brif q != end, h(q), exit2      ; exit2: n + 1
/// ```
///
/// Evidence: the slice iterator under `position`/`take_while(..).count()`
/// (pulldown-cmark's scanners, `tests/parity/closing.rs` with
/// `PLIRON_WM_KEEPCOUNT=0`). Where `webmerge` retires the counter that
/// steps in lockstep with the pointer, the count is `p - base`,
/// computed every trip for the two exits: nine instructions a trip.
/// Sunk, the loop is the load (post-indexed on aarch64), two compares
/// and two branches. pulldown-cmark, exact: 53.38M -> 53.33M.
///
/// Rule: the inst is cloned to the head of each using block (an exit
/// edge out of an in-loop `brif` is split to hold it) and removed from
/// the loop, when
/// - every use site runs at most once per exit of the loop (outside it,
///   and not inside another loop that excludes the def), and
/// - every operand is loop-invariant, sinks too, is an `iconst`, or is
///   a header param stepped by a constant on every back edge (`q = p +
///   k`). Where `q`'s def dominates the site the clone reads `q - k`
///   instead of `p`, so `p` still dies at its step and the header param
///   keeps one register (otherwise the back edge pays a `mov`); an
///   `iadd`/`isub` clone is built as `op(q, x) -/+ k` and a using
///   `iadd _, k2` absorbs the constant (`(p - base) + 1` on the
///   exhausted exit is `q - base`).
///
/// Any other loop-variant operand would stay live to the exit just for
/// the clone, so such insts stay put.
fn exit_sink(func: &mut Function, cfg: &ControlFlowGraph, dt: &DominatorTree, la: &LoopAnalysis) -> usize {
    if !exit_sink_on() || la.loops().next().is_none() {
        return 0;
    }
    struct Cand {
        inst: Inst,
        block: Block,
        lp: Loop,
    }
    // Candidates in reverse postorder: a def comes before its users.
    let mut cands: FxHashMap<Value, Cand> = FxHashMap::default();
    let mut order: Vec<Value> = Vec::new();
    for &b in dt.cfg_postorder().iter().rev() {
        let Some(lp) = la.innermost_loop(b) else {
            continue;
        };
        for i in func.layout.block_insts(b) {
            if !pure_op(func, i) {
                continue;
            }
            let v = func.dfg.first_result(i);
            if func.dfg.resolve_aliases(v) != v {
                continue;
            }
            cands.insert(v, Cand { inst: i, block: b, lp });
            order.push(v);
        }
    }
    if cands.is_empty() {
        return 0;
    }
    let mut sites: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                if cands.contains_key(&v) {
                    let l = sites.entry(v).or_default();
                    if l.last() != Some(&i) {
                        l.push(i);
                    }
                }
            }
        }
    }
    let mut bumps: FxHashMap<Value, Option<Bump>> = FxHashMap::default();
    // `v` is read at an exit edge of in-loop `brif` `s` only.
    let exit_edges = |func: &Function, s: Inst, v: Value, lp: Loop| -> Option<Vec<usize>> {
        let InstructionData::Brif { arg, blocks, .. } = &func.dfg.insts[s] else {
            return None;
        };
        if func.dfg.resolve_aliases(*arg) == v {
            return None;
        }
        let pool = &func.dfg.value_lists;
        let mut out = Vec::new();
        for (e, bc) in blocks.iter().enumerate() {
            let mut has = false;
            for a in bc.args(pool) {
                has |= func.dfg.resolve_aliases(a.as_value()?) == v;
            }
            if has {
                if la.is_in_loop(bc.block(pool), lp) {
                    return None;
                }
                out.push(e);
            }
        }
        Some(out)
    };
    // Shrink the candidate set to its largest self-consistent subset.
    let mut alive: FxHashSet<Value> = order.iter().copied().collect();
    let mut stable = false;
    for _ in 0..8 {
        let mut dropped = false;
        for &v in &order {
            if !alive.contains(&v) {
                continue;
            }
            let c = &cands[&v];
            let ok = (|| {
                let us = sites.get(&v)?;
                if us.is_empty() || us.len() > MAX_PUSH_BLOCKS {
                    return None;
                }
                for &s in us {
                    let u = func.layout.inst_block(s)?;
                    if la.is_in_loop(u, c.lp) {
                        let member = func
                            .dfg
                            .inst_results(s)
                            .first()
                            .is_some_and(|r| alive.contains(r) && cands[r].inst == s && cands[r].lp == c.lp);
                        if !member {
                            exit_edges(func, s, v, c.lp)?;
                        }
                    } else if let Some(lu) = la.innermost_loop(u)
                        && !la.is_in_loop(c.block, lu)
                    {
                        return None;
                    }
                }
                for &a in func.dfg.inst_args(c.inst) {
                    let w = func.dfg.resolve_aliases(a);
                    match func.dfg.value_def(w) {
                        ValueDef::Param(pb, _) => {
                            if la.is_in_loop(pb, c.lp) {
                                // A parameter of this loop's own header
                                // (the memo is per parameter, so only ever
                                // for the loop that parameter heads).
                                if pb != la.loop_header(c.lp) {
                                    return None;
                                }
                                bumps
                                    .entry(w)
                                    .or_insert_with(|| iv_bump(func, cfg, la, c.lp, w))
                                    .as_ref()?;
                            }
                        }
                        ValueDef::Result(di, _) => {
                            let db = func.layout.inst_block(di)?;
                            if la.is_in_loop(db, c.lp)
                                && !(alive.contains(&w) && cands[&w].lp == c.lp)
                                && func.dfg.insts[di].opcode() != Opcode::Iconst
                            {
                                return None;
                            }
                        }
                        _ => return None,
                    }
                }
                Some(())
            })()
            .is_some();
            if !ok {
                alive.remove(&v);
                dropped = true;
            }
        }
        if !dropped {
            stable = true;
            break;
        }
    }
    if !stable || alive.is_empty() {
        return 0;
    }
    let mut n = 0;
    // Edge blocks made here, with the in-loop block they hang off.
    let mut mid_pred: FxHashMap<Block, Block> = FxHashMap::default();
    // Users before defs: an operand's clone lands above its user's.
    for &v in order.iter().rev() {
        if !alive.contains(&v) {
            continue;
        }
        let c = &cands[&v];
        let (d, lp) = (c.inst, c.lp);
        let users = sites.get(&v).cloned().unwrap_or_default();
        // A user that stayed in the loop (bisect) keeps `v` there.
        let movable = users.iter().all(|&s| {
            func.layout
                .inst_block(s)
                .is_some_and(|u| !la.is_in_loop(u, lp) || exit_edges(func, s, v, lp).is_some())
        });
        if users.is_empty() || !movable || !pliron_cfg::bisect("exitsink") {
            continue;
        }
        // Split the exit edges that carry `v`.
        let mut finals: Vec<Inst> = Vec::new();
        for &s in &users {
            let u = func.layout.inst_block(s).unwrap();
            if !la.is_in_loop(u, lp) {
                finals.push(s);
                continue;
            }
            for e in exit_edges(func, s, v, lp).unwrap() {
                let (dest, args) = {
                    let InstructionData::Brif { blocks, .. } = &func.dfg.insts[s] else {
                        unreachable!()
                    };
                    let pool = &func.dfg.value_lists;
                    let args: Vec<Value> = blocks[e].args(pool).map(|a| a.as_value().unwrap()).collect();
                    (blocks[e].block(pool), args)
                };
                let mid = func.dfg.make_block();
                func.layout.insert_block_after(mid, u);
                let barg: Vec<BlockArg> = args.iter().map(|&a| BlockArg::Value(a)).collect();
                let j = FuncCursor::new(func).at_bottom(mid).ins().jump(dest, &barg);
                let dfg = &mut func.dfg;
                dfg.insts[s].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e] =
                    BlockCall::new(mid, [], &mut dfg.value_lists);
                mid_pred.insert(mid, u);
                for &a in &args {
                    let r = func.dfg.resolve_aliases(a);
                    let still = func.dfg.inst_values(s).any(|x| func.dfg.resolve_aliases(x) == r);
                    if let Some(l) = sites.get_mut(&r) {
                        if !still {
                            l.retain(|&x| x != s);
                        }
                        if !l.contains(&j) {
                            l.push(j);
                        }
                    }
                }
                finals.push(j);
            }
        }
        let mut by_block: Vec<(Block, Vec<Inst>)> = Vec::new();
        for s in finals {
            let x = func.layout.inst_block(s).unwrap();
            match by_block.iter_mut().find(|(b, _)| *b == x) {
                Some((_, l)) => {
                    if !l.contains(&s) {
                        l.push(s)
                    }
                }
                None => by_block.push((x, vec![s])),
            }
        }
        let data = func.dfg.insts[d];
        let ty = func.dfg.ctrl_typevar(d);
        let args: Vec<Value> = func.dfg.inst_args(d).to_vec();
        for (x, us) in by_block {
            // Header params whose step dominates this site.
            let mut subs: Vec<(usize, Bump)> = Vec::new();
            for (ix, &a) in args.iter().enumerate() {
                let w = func.dfg.resolve_aliases(a);
                // Only a parameter of the loop `d` leaves: one of an
                // enclosing loop's header is invariant here, and its
                // step may be `d` itself (fuzz seed 74256: `d = 0 + w`
                // in an inner loop, passed back to the outer header).
                if !matches!(func.dfg.value_def(w), ValueDef::Param(pb, _) if pb == la.loop_header(lp)) {
                    continue;
                }
                let Some(Some(bp)) = bumps.get(&w) else {
                    continue;
                };
                if bp.q == v {
                    continue;
                }
                let dom = match mid_pred.get(&x) {
                    Some(&u) => bp.qb == u || dt.try_dominates(bp.qb, u, &func.layout),
                    None => bp.qb != x && dt.try_dominates(bp.qb, x, &func.layout),
                };
                if dom {
                    subs.push((ix, *bp));
                }
            }
            let first = func.layout.first_inst(x).unwrap();
            let mut new_args = args.clone();
            // `v = n + post` when the constant is peeled off the clone.
            let mut post: Option<i64> = None;
            if let [(ix, bp)] = subs[..]
                && matches!(data.opcode(), Opcode::Iadd | Opcode::Isub)
                && ty == I64
            {
                new_args[ix] = bp.q;
                post = Some(if data.opcode() == Opcode::Isub && ix == 1 { bp.k } else { bp.k.wrapping_neg() });
            } else {
                for &(ix, bp) in &subs {
                    let mut cur = FuncCursor::new(func).at_inst(first);
                    let kc = cur.ins().iconst(I64, bp.k.wrapping_neg());
                    new_args[ix] = cur.ins().iadd(bp.q, kc);
                }
            }
            let ni = func.dfg.make_inst(data);
            func.set_srcloc(ni, func.srcloc(d));
            func.dfg.inst_args_mut(ni).copy_from_slice(&new_args);
            func.dfg.make_inst_results(ni, ty);
            func.layout.insert_inst(ni, first);
            let nv = func.dfg.first_result(ni);
            for &a in &new_args {
                let r = func.dfg.resolve_aliases(a);
                if let Some(l) = sites.get_mut(&r) {
                    l.push(ni);
                }
            }
            // The value the remaining users read; made on demand.
            let mut full: Option<Value> = post.is_none().then_some(nv);
            for s in us {
                if let Some(pk) = post
                    && let InstructionData::Binary { opcode: Opcode::Iadd, args: [a0, a1] } = func.dfg.insts[s]
                {
                    let (r0, r1) = (func.dfg.resolve_aliases(a0), func.dfg.resolve_aliases(a1));
                    let other = if r0 == v && r1 != v {
                        Some(r1)
                    } else if r1 == v && r0 != v {
                        Some(r0)
                    } else {
                        None
                    };
                    if let Some(o) = other
                        && let Some(k2) = iconst_of(func, o)
                    {
                        if let Some(l) = sites.get_mut(&o) {
                            l.retain(|&y| y != s);
                        }
                        let total = pk.wrapping_add(k2);
                        if total == 0 {
                            let res = func.dfg.first_result(s);
                            func.layout.remove_inst(s);
                            func.dfg.detach_inst_results(s);
                            func.dfg.change_to_alias(res, nv);
                        } else {
                            let kc = FuncCursor::new(func).at_inst(s).ins().iconst(I64, total);
                            func.dfg.inst_args_mut(s).copy_from_slice(&[nv, kc]);
                        }
                        continue;
                    }
                }
                let fv = match full {
                    Some(fv) => fv,
                    None => {
                        let mut cur = FuncCursor::new(func).after_inst(ni);
                        let kc = cur.ins().iconst(I64, post.unwrap());
                        let fv = cur.ins().iadd(nv, kc);
                        full = Some(fv);
                        fv
                    }
                };
                let raw: Vec<Value> = func.dfg.inst_values(s).filter(|&w| func.dfg.resolve_aliases(w) == v).collect();
                func.dfg.map_inst_values(s, |w| if raw.contains(&w) { fv } else { w });
            }
        }
        for &a in &args {
            let r = func.dfg.resolve_aliases(a);
            if let Some(l) = sites.get_mut(&r) {
                l.retain(|&y| y != d);
            }
        }
        func.layout.remove_inst(d);
        func.dfg.clear_results(d);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::types::{I32, I64};
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, Function, InstBuilder, Opcode, Signature, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        cranelift_codegen::verify_function(func, &flags).unwrap();
    }

    /// `brif` error-edge args built by a pure chain sink onto the edge;
    /// the success edge and the shared producer stay in `b`.
    #[test]
    fn sinks_edge_only_chain() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I32));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        let err = func.dfg.make_block();
        let ok = func.dfg.make_block();
        for bl in [b, err, ok] {
            func.layout.append_block(bl);
        }
        let x = func.dfg.append_block_param(b, I64);
        let c = func.dfg.append_block_param(b, I32);
        let perr = func.dfg.append_block_param(err, I64);
        let pok = func.dfg.append_block_param(ok, I64);
        {
            // e1 = x & -256 — shared by both edges, must stay in b.
            // e2 = e1 | 5  — only the err edge, must sink with its 5.
            // s  = e1 | 7  — only the ok edge, sinks too.
            let mut cur = FuncCursor::new(&mut func).at_bottom(b);
            let m = cur.ins().iconst(I64, -256);
            let e1 = cur.ins().band(x, m);
            let k5 = cur.ins().iconst(I64, 5);
            let e2 = cur.ins().bor(e1, k5);
            let k7 = cur.ins().iconst(I64, 7);
            let s = cur.ins().bor(e1, k7);
            cur.ins()
                .brif(c, err, &[BlockArg::Value(e2)], ok, &[BlockArg::Value(s)]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(err);
            cur.ins().return_(&[perr]);
            let mut cur = FuncCursor::new(&mut func).at_bottom(ok);
            cur.ins().return_(&[pok]);
        }
        verify(&func);
        let n = run(&mut func);
        verify(&func);
        // b keeps: iconst -256, band, brif — the two `bor`/`iconst`
        // pairs moved into their edge blocks.
        let bors: Vec<_> = func
            .layout
            .block_insts(b)
            .filter(|i| {
                matches!(
                    func.dfg.insts[*i],
                    InstructionData::Binary {
                        opcode: Opcode::Bor,
                        ..
                    }
                )
            })
            .collect();
        assert!(bors.is_empty(), "bor stayed in b: {}", func.display());
        assert!(n >= 2, "expected the |5 and |7 chains to sink, got {n}");
        let edge_blocks = func
            .layout
            .blocks()
            .filter(|bl| {
                let insts: Vec<_> = func.layout.block_insts(*bl).collect();
                insts.len() >= 2
                    && matches!(
                        func.dfg.insts[*insts.last().unwrap()],
                        InstructionData::Jump { .. }
                    )
                    && insts[..insts.len() - 1].iter().all(|i| pure_op(&func, *i))
            })
            .count();
        assert_eq!(edge_blocks, 2, "{}", func.display());
    }

    /// A value used on both edges (or in the block body) stays.
    #[test]
    fn keeps_shared_or_body_used() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I32));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        let l = func.dfg.make_block();
        let r = func.dfg.make_block();
        for bl in [b, l, r] {
            func.layout.append_block(bl);
        }
        let x = func.dfg.append_block_param(b, I64);
        let c = func.dfg.append_block_param(b, I32);
        func.dfg.append_block_param(l, I64);
        func.dfg.append_block_param(r, I64);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b);
            let k = cur.ins().iconst(I64, 3);
            // v goes to BOTH edges — must not sink.
            let v = cur.ins().iadd(x, k);
            cur.ins()
                .brif(c, l, &[BlockArg::Value(v)], r, &[BlockArg::Value(v)]);
        }
        {
            let p = func.dfg.block_params(l)[0];
            let mut cur = FuncCursor::new(&mut func).at_bottom(l);
            cur.ins().return_(&[p]);
            let p = func.dfg.block_params(r)[0];
            let mut cur = FuncCursor::new(&mut func).at_bottom(r);
            cur.ins().return_(&[p]);
        }
        verify(&func);
        assert_eq!(run(&mut func), 0);
        verify(&func);
        assert_eq!(func.layout.blocks().count(), 3);
    }

    /// `n = p - base` read only by the loop's two exits leaves the loop;
    /// the clones read the stepped pointer, so `(p - base) + 1` on the
    /// latch exit is `q - base`.
    #[test]
    fn sinks_exit_only_value_out_of_loop() {
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in 0..3 {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        let latch = func.dfg.make_block();
        let exit = func.dfg.make_block();
        for bl in [entry, h, latch, exit] {
            func.layout.append_block(bl);
        }
        let base = func.dfg.append_block_param(entry, I64);
        let end = func.dfg.append_block_param(entry, I64);
        let stop = func.dfg.append_block_param(entry, I64);
        let p = func.dfg.append_block_param(h, I64);
        let r = func.dfg.append_block_param(exit, I64);
        FuncCursor::new(&mut func).at_bottom(entry).ins().jump(h, &[BlockArg::Value(base)]);
        let (n, q, one) = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(h);
            let n = cur.ins().isub(p, base);
            let one = cur.ins().iconst(I64, 1);
            let q = cur.ins().iadd(p, one);
            let c = cur.ins().icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, p, stop);
            cur.ins().brif(c, exit, &[BlockArg::Value(n)], latch, &[]);
            (n, q, one)
        };
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(latch);
            let m = cur.ins().iadd(n, one);
            let t = cur.ins().icmp(cranelift_codegen::ir::condcodes::IntCC::NotEqual, q, end);
            cur.ins().brif(t, h, &[BlockArg::Value(q)], exit, &[BlockArg::Value(m)]);
        }
        FuncCursor::new(&mut func).at_bottom(exit).ins().return_(&[r]);
        verify(&func);
        let k = run(&mut func);
        verify(&func);
        assert!(k >= 2, "{}", func.display());
        let subs_in = |func: &Function, b: Block| {
            func.layout
                .block_insts(b)
                .filter(|&i| func.dfg.insts[i].opcode() == Opcode::Isub)
                .count()
        };
        assert_eq!(subs_in(&func, h) + subs_in(&func, latch), 0, "{}", func.display());
        // Both exits compute from `q`, and `p` has no use left but its
        // step and the loop's own test.
        let mut subs = 0;
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if let InstructionData::Binary { opcode: Opcode::Isub, args } = func.dfg.insts[i] {
                    assert_eq!(func.dfg.resolve_aliases(args[0]), q, "{}", func.display());
                    subs += 1;
                }
            }
        }
        assert_eq!(subs, 2, "{}", func.display());
        // The latch exit's `+ 1` cancelled against the step.
        let adds = func
            .layout
            .blocks()
            .filter(|&b| b != h)
            .flat_map(|b| func.layout.block_insts(b).collect::<Vec<_>>())
            .filter(|&i| func.dfg.insts[i].opcode() == Opcode::Iadd)
            .count();
        assert_eq!(adds, 1, "{}", func.display());
    }

    /// A value an in-loop inst still reads stays in the loop.
    #[test]
    fn keeps_value_used_in_loop() {
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in 0..2 {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let h = func.dfg.make_block();
        let exit = func.dfg.make_block();
        for bl in [entry, h, exit] {
            func.layout.append_block(bl);
        }
        let base = func.dfg.append_block_param(entry, I64);
        let end = func.dfg.append_block_param(entry, I64);
        let p = func.dfg.append_block_param(h, I64);
        let r = func.dfg.append_block_param(exit, I64);
        FuncCursor::new(&mut func).at_bottom(entry).ins().jump(h, &[BlockArg::Value(base)]);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(h);
            let n = cur.ins().isub(p, base);
            let one = cur.ins().iconst(I64, 1);
            let q = cur.ins().iadd(p, one);
            // The loop's own test reads `n`.
            let t = cur.ins().icmp(cranelift_codegen::ir::condcodes::IntCC::NotEqual, n, end);
            cur.ins().brif(t, h, &[BlockArg::Value(q)], exit, &[BlockArg::Value(n)]);
        }
        FuncCursor::new(&mut func).at_bottom(exit).ins().return_(&[r]);
        verify(&func);
        run(&mut func);
        verify(&func);
        // (The step may move to the back edge; the count stays.)
        assert!(
            func.layout.block_insts(h).any(|i| func.dfg.insts[i].opcode() == Opcode::Isub),
            "{}",
            func.display()
        );
    }
}

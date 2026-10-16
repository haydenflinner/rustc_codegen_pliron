//! Branch-chained range-check folds.
//!
//! Two shapes from rustc's `char::is_*`/ascii classification chains:
//!
//! 1. A lower-bound test `brif ule(lo, x) → (brif ult(x, hi) → HIT)
//!    else MISS` where both blocks share the same miss edge — a
//!    two-branch `[lo, hi)` interval check — collapses to the
//!    unsigned-offset form `brif ult(x-lo, hi-lo) → HIT else MISS`:
//!    one `isub`/`icmp`/`brif` instead of two `icmp`/`brif` pairs. The
//!    second block is bypassed, not removed (other preds still need it).
//!
//! 2. Two such offset checks on the same `x` and width, `x-k1 u<w` and
//!    `x-k2 u<w`, both reaching the same HIT edge, fold to
//!    `(x|d) - k_hi u< w` when `d = |k1-k2|` is a power of two and the
//!    bit-d partition is exact: `x|d` equals `x` or `x+d`, so the masked
//!    test applies the hi range to bit-set `x` and the lo range to
//!    bit-clear `x`. Exact iff every `x` in the lo range has bit d clear
//!    and every `x` in the hi range has it set — `k_lo mod 2d + w <= d`
//!    and `d <= k_hi mod 2d`, `k_hi mod 2d + w <= 2d`. This is LLVM's
//!    `c | 0x20` case-fold for `a-z|A-Z`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, ProgramPoint,
    Value, ValueDef,
};
use rustc_hash::FxHashMap;

/// Block each value is used in (None if in several) — for checking a
/// bypassed block's defs stay local.
fn use_blocks(func: &Function) -> FxHashMap<Value, Option<Block>> {
    let mut m: FxHashMap<Value, Option<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let e = m.entry(func.dfg.resolve_aliases(v)).or_insert(Some(b));
                if *e != Some(b) {
                    *e = None;
                }
            }
        }
    }
    m
}

/// Every def in `b` (params + results) is only used inside `b` —
/// bypassing `b` then can't strand a use, like block17's param flowing
/// to a successor's icmp in the original rangefold verifier failure.
fn defs_local(func: &Function, uses: &FxHashMap<Value, Option<Block>>, b: Block) -> bool {
    let ok = |v: &Value| uses.get(v).is_none_or(|&u| u == Some(b));
    func.dfg.block_params(b).iter().all(ok)
        && func
            .layout
            .block_insts(b)
            .all(|i| func.dfg.inst_results(i).iter().all(ok))
}

fn iconst(func: &Function, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits() as u64),
        _ => None,
    }
}

/// Every value `hdest`'s insts use must still dominate them once `at`'s
/// block becomes a direct predecessor: defs inside `hdest` and `hdest`'s
/// own params are unaffected, but a def that only dominated `hdest`'s
/// old preds — like an iconst in the bypassed lo-block — would dangle.
fn body_avail(func: &Function, dom: &DominatorTree, hdest: Block, at: Inst) -> bool {
    func.layout.block_insts(hdest).all(|i| {
        func.dfg.inst_values(i).all(|v| {
            let v = func.dfg.resolve_aliases(v);
            match func.dfg.value_def(v) {
                ValueDef::Result(di, _) => {
                    func.layout.inst_block(di) == Some(hdest)
                        || dom.dominates(ProgramPoint::Inst(di), at, &func.layout)
                }
                ValueDef::Param(b, _) => {
                    b == hdest || dom.dominates(ProgramPoint::Block(b), at, &func.layout)
                }
                _ => false,
            }
        })
    })
}

/// `lo(k)` means `x >= k`; `hi(k)` means `x < k` — so a `[lo, hi)`
/// check is `x - lo u< hi - lo`.
#[derive(Clone, Copy)]
enum Half {
    Lo(u64),
    Hi(u64),
}

fn half_bound(func: &Function, c: Value) -> Option<(Value, Half)> {
    let i = func.dfg.resolved_value_def(c).inst()?;
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        args,
        cond,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let (a, b) = (args[0], args[1]);
    match (cond, iconst(func, a), iconst(func, b)) {
        (IntCC::UnsignedLessThanOrEqual, Some(k), None) => Some((b, Half::Lo(k))),
        (IntCC::UnsignedGreaterThanOrEqual, None, Some(k)) => Some((a, Half::Lo(k))),
        (IntCC::UnsignedLessThan, Some(k), None) => Some((b, Half::Lo(k.checked_add(1)?))),
        (IntCC::UnsignedGreaterThan, None, Some(k)) => Some((a, Half::Lo(k.checked_add(1)?))),
        (IntCC::UnsignedLessThan, None, Some(k)) => Some((a, Half::Hi(k))),
        (IntCC::UnsignedLessThanOrEqual, None, Some(k)) => Some((a, Half::Hi(k.checked_add(1)?))),
        (IntCC::UnsignedGreaterThan, Some(k), None) => Some((b, Half::Hi(k))),
        (IntCC::UnsignedGreaterThanOrEqual, Some(k), None) => {
            Some((b, Half::Hi(k.checked_add(1)?)))
        }
        _ => None,
    }
}

fn dest_block(func: &Function, bc: BlockCall) -> Block {
    bc.block(&func.dfg.value_lists)
}

fn same_edge(func: &Function, a: BlockCall, b: BlockCall) -> bool {
    if dest_block(func, a) != dest_block(func, b) {
        return false;
    }
    let mut x = a.args(&func.dfg.value_lists);
    let mut y = b.args(&func.dfg.value_lists);
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(BlockArg::Value(p)), Some(BlockArg::Value(q))) => {
                if func.dfg.resolve_aliases(p) != func.dfg.resolve_aliases(q) {
                    return false;
                }
            }
            (Some(aa), Some(bb)) if aa == bb => {}
            _ => return false,
        }
    }
}

/// Miss edges `a`/`b` landing on different blocks whose bodies are
/// congruent — `is_alphabetic`'s match arms duplicate the `c < 170`
/// test per arm, so the chain's shared miss is two twins. Returning
/// true keeps `a`'s edge; the twin loses its last pred and is swept.
fn miss_twin(func: &Function, a: BlockCall, b: BlockCall) -> bool {
    let (da, db) = (dest_block(func, a), dest_block(func, b));
    if da == db {
        return false;
    }
    // Both edges must be arg-free — congruent bodies can still compute
    // different outgoing args, and picking `a`'s dest would misroute them.
    let bare = |bc: BlockCall| {
        bc.args(&func.dfg.value_lists)
            .all(|arg| arg.as_value().is_none())
    };
    bare(a)
        && bare(b)
        && func.dfg.block_params(da).is_empty()
        && func.dfg.block_params(db).is_empty()
        && crate::tailmerge::key(func, da) == crate::tailmerge::key(func, db)
        && crate::tailmerge::key(func, da).is_some()
}

/// Every `BlockArg::Value` on `bc` must be usable at `inst` — an edge
/// moved from its origin block carries args evaluated there, and a def
/// inside the bypassed block (or anywhere not dominating `inst`) would
/// dangle.
fn args_avail(func: &Function, dom: &DominatorTree, bc: BlockCall, inst: Inst) -> bool {
    bc.args(&func.dfg.value_lists).all(|a| match a {
        BlockArg::Value(v) => {
            let v = func.dfg.resolve_aliases(v);
            let pp = match func.dfg.value_def(v) {
                ValueDef::Result(i, _) => ProgramPoint::Inst(i),
                ValueDef::Param(b, _) => ProgramPoint::Block(b),
                _ => return false,
            };
            dom.dominates(pp, inst, &func.layout)
        }
        _ => true,
    })
}

/// A hit-edge arg's materialization plan at the merge point.
enum Mat {
    /// Keep the edge arg verbatim (pseudo-args or dominating values).
    Arg(BlockArg),
    /// Pure inst the fold bypasses: re-emit it with dominating
    /// operands (`iadd count,1` produced per-arm by paramsink).
    Clone(Inst),
    /// Literal — re-emit an iconst.
    K(u64, cranelift_codegen::ir::Type),
}

/// Can this edge arg be materialized at `at`? Dominating defs reuse;
/// a pure inst may be re-emitted when every operand dominates `at` or
/// is itself an iconst (depth-1 — enough for `count+1` arms).
fn mat_of(func: &Function, dom: &DominatorTree, a: BlockArg, at: Inst) -> Option<Mat> {
    // Pseudo-args (try_call ret/exn) are bound to their own terminator
    // — they can't ride the merged edge.
    let v = a.as_value()?;
    let v = func.dfg.resolve_aliases(v);
    let pp = match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => ProgramPoint::Inst(i),
        ValueDef::Param(b, _) => ProgramPoint::Block(b),
        _ => return None,
    };
    if dom.dominates(pp, at, &func.layout) {
        return Some(Mat::Arg(a));
    }
    if let Some(k) = iconst(func, v) {
        return Some(Mat::K(k, func.dfg.value_type(v)));
    }
    let ValueDef::Result(di, 0) = func.dfg.value_def(v) else {
        return None;
    };
    if !crate::jumpthread::pure_op(func, di) {
        return None;
    }
    let ok = func.dfg.inst_args(di).to_vec().iter().all(|&o| {
        let o = func.dfg.resolve_aliases(o);
        if iconst(func, o).is_some() {
            return true;
        }
        let pp = match func.dfg.value_def(o) {
            ValueDef::Result(i, _) => ProgramPoint::Inst(i),
            ValueDef::Param(b, _) => ProgramPoint::Block(b),
            _ => return false,
        };
        dom.dominates(pp, at, &func.layout)
    });
    ok.then_some(Mat::Clone(di))
}

fn emit_mat(func: &mut Function, at: Inst, m: &Mat) -> BlockArg {
    match m {
        Mat::Arg(a) => *a,
        Mat::K(k, ty) => {
            let mut pos = FuncCursor::new(func).at_inst(at);
            BlockArg::Value(pos.ins().iconst(*ty, *k as i64))
        }
        Mat::Clone(di) => {
            let data = func.dfg.insts[*di];
            let ty = func.dfg.value_type(func.dfg.first_result(*di));
            let ops: Vec<Result<Value, (u64, cranelift_codegen::ir::Type)>> = func
                .dfg
                .inst_args(*di)
                .to_vec()
                .iter()
                .map(|&o| {
                    let o = func.dfg.resolve_aliases(o);
                    match iconst(func, o) {
                        Some(k) => Err((k, func.dfg.value_type(o))),
                        None => Ok(o),
                    }
                })
                .collect();
            let mut pos = FuncCursor::new(func).at_inst(at);
            let vs: Vec<Value> = ops
                .iter()
                .map(|o| match o {
                    Ok(v) => *v,
                    Err((k, ty)) => pos.ins().iconst(*ty, *k as i64),
                })
                .collect();
            let nd = crate::paramsink::rebuild(&data, &vs);
            let ni = pos.func.dfg.make_inst(nd);
            pos.func.dfg.make_inst_results(ni, ty);
            pos.func.layout.insert_inst(ni, at);
            BlockArg::Value(pos.func.dfg.first_result(ni))
        }
    }
}

/// `v` defined as `isub x, k` → `(x, k)`.
fn isub_const(func: &Function, v: Value) -> Option<(Value, u64)> {
    let i = func.dfg.resolved_value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args: [a, b],
        } => iconst(func, b).map(|k| (func.dfg.resolve_aliases(a), k)),
        _ => None,
    }
}

/// An `icmp ult s, w` where `s = isub x, k` → `(x, k, w)`.
fn offset_test(func: &Function, c: Value) -> Option<(Value, u64, u64)> {
    let i = func.dfg.resolved_value_def(c).inst()?;
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        args: [a, b],
        cond: IntCC::UnsignedLessThan,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let w = iconst(func, b)?;
    let (x, k) = isub_const(func, a)?;
    Some((x, k, w))
}

/// A block we can bypass: every non-terminator inst must be pure —
/// skipping it must drop no store, call, or trap.
fn bypassable(func: &Function, b: Block) -> bool {
    func.layout
        .block_insts(b)
        .take_while(|&i| i != func.layout.last_inst(b).unwrap())
        .all(|i| {
            let op = func.dfg.insts[i].opcode();
            !(op.can_store() || op.can_trap() || op.is_call() || op.other_side_effects())
        })
}

/// Rule 1: `brif (x >= lo) → [brif (x < hi) → HIT else MISS] else MISS`
/// becomes `brif (x-lo u< hi-lo) → HIT else MISS`.
fn bound_pairs(func: &mut Function) -> usize {
    let dbg = tracing::enabled!(target: "pliron::rangefold", tracing::Level::DEBUG);
    let uses = use_blocks(func);
    let mut n = 0;
    // Bypassing moves edges, and a stale dominator tree could let a
    // later rewrite in the same batch accept a value that only
    // dominated through the just-removed edge — apply one fold per
    // dominator rebuild.
    loop {
        let cfg = ControlFlowGraph::with_function(func);
        let dom = DominatorTree::with_function(func, &cfg);
        let mut found = None;
        for b in func.layout.blocks().collect::<Vec<_>>() {
            let Some(term) = func.layout.last_inst(b) else {
                continue;
            };
            let InstructionData::Brif { arg: c, blocks, .. } = func.dfg.insts[term] else {
                continue;
            };
            let (tbc, fbc) = (blocks[0], blocks[1]);
            // The pass edge is "x >= lo" either way: a `Lo` test takes
            // it on true, an `x < lo` (`Hi`) test takes it on false —
            // MIR emits both polarities (`ugt 97,c` in chars_alpha).
            let (x, lo, pass_bc, miss_bc) = match half_bound(func, c) {
                Some((x, Half::Lo(lo))) => (x, lo, tbc, fbc),
                Some((x, Half::Hi(lo))) => (x, lo, fbc, tbc),
                _ => continue,
            };
            if dbg {
                tracing::debug!(target: "pliron::rangefold", "rf: block{b} lo-test x={x} lo={lo}");
            }
            let hi_b = dest_block(func, pass_bc);
            if hi_b == b {
                continue;
            }
            // The taken arm must be a bare upper-bound test sharing the
            // same miss edge — we bypass it entirely.
            if !bypassable(func, hi_b) || !defs_local(func, &uses, hi_b) {
                if dbg {
                    tracing::debug!(target: "pliron::rangefold", "rf:   hi_b{hi_b} impure or escaping");
                }
                continue;
            }
            let Some(hterm) = func.layout.last_inst(hi_b) else {
                continue;
            };
            let InstructionData::Brif {
                arg: c2,
                blocks: hblocks,
                ..
            } = func.dfg.insts[hterm]
            else {
                continue;
            };
            let (hbc, mbc) = (hblocks[0], hblocks[1]);
            let Some((x2, Half::Hi(hi))) = half_bound(func, c2) else {
                continue;
            };
            // Merging the tests moves the hit edge's args to this block.
            // Values that dominate reuse; a pure inst paramsink sunk
            // (`iadd count,1`) or a const re-emits. Unmaterializable args
            // — defs that stay inside the bypassed block — would dangle or
            // hoist unconditionally: the `mov;stur` flag regression shape.
            let Some(hit_mats) = hbc
                .args(&func.dfg.value_lists)
                .map(|a| mat_of(func, &dom, a, term))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let miss_eq = same_edge(func, miss_bc, mbc) || miss_twin(func, miss_bc, mbc);
            // The bounds must be representable in x's type: an iconst
            // wider than the type truncates on materialization, and the
            // `x-lo u< hi-lo` model needs hi <= 2^bits to hold mod 2^N.
            let tb = func.dfg.value_type(x).bits();
            let fits = |v: u64| tb >= 64 || v >> tb == 0;
            let ok = func.dfg.resolve_aliases(x) == func.dfg.resolve_aliases(x2)
                && hi > lo
                && fits(lo)
                && fits(hi)
                && miss_eq
                && body_avail(func, &dom, dest_block(func, hbc), term);
            if dbg {
                tracing::debug!(target: "pliron::rangefold", "rf:   x={x}=={x2}? {} hi={hi}>lo? {} miss-eq? {} mats={}",
                    func.dfg.resolve_aliases(x) == func.dfg.resolve_aliases(x2),
                    hi > lo,
                    miss_eq,
                    hit_mats.len()
                );
            }
            if !ok {
                continue;
            }
            found = Some((
                term,
                x,
                lo,
                hi - lo,
                dest_block(func, hbc),
                miss_bc,
                hit_mats,
            ));
            break;
        }
        let Some((term, x, lo, w, hdest, fbc, mats)) = found else {
            return n;
        };
        let hargs: Vec<BlockArg> = mats.iter().map(|m| emit_mat(func, term, m)).collect();
        let hbc = BlockCall::new(hdest, hargs, &mut func.dfg.value_lists);
        let ty = func.dfg.value_type(x);
        let mut pos = FuncCursor::new(func).at_inst(term);
        let k = pos.ins().iconst(ty, lo as i64);
        let t = pos.ins().isub(x, k);
        let wv = pos.ins().iconst(ty, w as i64);
        let c = pos.ins().icmp(IntCC::UnsignedLessThan, t, wv);
        let InstructionData::Brif { arg, blocks, .. } = &mut func.dfg.insts[term] else {
            return n;
        };
        *arg = c;
        *blocks = [hbc, fbc].into();
        n += 1;
    }
}

/// Operand-equality for clone shape checks: same resolved value, or
/// both iconsts with the same bits and type.
fn op_eq(func: &Function, x: Value, y: Value) -> bool {
    let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
    x == y
        || match (iconst(func, x), iconst(func, y)) {
            (Some(a), Some(b)) => a == b && func.dfg.value_type(x) == func.dfg.value_type(y),
            _ => false,
        }
}

/// Same pure op on equal operands — edge args paramsink produced read
/// as different insts (`iadd count,1` cloned per classification arm).
fn shape_eq(func: &Function, di: Inst, dj: Inst) -> bool {
    if func.dfg.insts[di].opcode() != func.dfg.insts[dj].opcode() {
        return false;
    }
    let (aa, bb) = (
        func.dfg.inst_args(di).to_vec(),
        func.dfg.inst_args(dj).to_vec(),
    );
    aa.len() == bb.len() && aa.iter().zip(&bb).all(|(&x, &y)| op_eq(func, x, y))
}

/// Hit-edge args for a union merge: same dest, and per-slot either the
/// identical arg or shape-equal pure args (`iadd count,1` on each arm)
/// that materialize at `at`. Returns the merged edge's arg plans.
fn shared_args(
    func: &Function,
    dom: &DominatorTree,
    a: BlockCall,
    b: BlockCall,
    at: Inst,
) -> Option<Vec<Mat>> {
    let av: Vec<BlockArg> = a.args(&func.dfg.value_lists).collect();
    let bv: Vec<BlockArg> = b.args(&func.dfg.value_lists).collect();
    if av.len() != bv.len() {
        return None;
    }
    av.iter()
        .zip(&bv)
        .map(|(&x, &y)| {
            if x == y {
                return mat_of(func, dom, x, at);
            }
            let (Some(xv), Some(yv)) = (x.as_value(), y.as_value()) else {
                return None;
            };
            let (xi, yi) = (
                func.dfg.resolved_value_def(xv).inst()?,
                func.dfg.resolved_value_def(yv).inst()?,
            );
            if !shape_eq(func, xi, yi) {
                return None;
            }
            mat_of(func, dom, x, at)
        })
        .collect()
}

/// Rule 2: `brif (x-k1 u<w) → HIT else [brif (x-k2 u<w) → HIT else MISS]`
/// becomes `brif ((x|d)-k_hi u<w) → HIT else MISS` when the bit-d
/// partition is exact.
fn range_unions(func: &mut Function) -> usize {
    let uses = use_blocks(func);
    let mut n = 0;
    // Same one-fold-per-dominator-rebuild discipline as bound_pairs:
    // the miss edge gains `b` as a new pred of nb's old miss dest.
    loop {
        let cfg = ControlFlowGraph::with_function(func);
        let dom = DominatorTree::with_function(func, &cfg);
        let mut found = None;
        for b in func.layout.blocks().collect::<Vec<_>>() {
            let Some(term) = func.layout.last_inst(b) else {
                continue;
            };
            let InstructionData::Brif { arg: c, blocks, .. } = func.dfg.insts[term] else {
                continue;
            };
            let (tbc, fbc) = (blocks[0], blocks[1]);
            let Some((x1, k1, w1)) = offset_test(func, c) else {
                continue;
            };
            let nb = dest_block(func, fbc);
            if nb == b {
                continue;
            }
            if !bypassable(func, nb) || !defs_local(func, &uses, nb) {
                continue;
            }
            let Some(nterm) = func.layout.last_inst(nb) else {
                continue;
            };
            let InstructionData::Brif {
                arg: c2,
                blocks: nblocks,
                ..
            } = func.dfg.insts[nterm]
            else {
                continue;
            };
            let (tbc2, fbc2) = (nblocks[0], nblocks[1]);
            let Some((x2, k2, w2)) = offset_test(func, c2) else {
                continue;
            };
            if x1 != x2 || w1 != w2 || k1 == k2 || dest_block(func, tbc) != dest_block(func, tbc2) {
                continue;
            }
            // The hit edges must agree on args: identical values, or the
            // same sunk computation on each arm (`iadd count,1`), which
            // re-emits once at the merged edge.
            let Some(hit_mats) = shared_args(func, &dom, tbc, tbc2, term) else {
                continue;
            };
            let (k_lo, k_hi) = (k1.min(k2), k1.max(k2));
            let d = k_hi - k_lo;
            if !d.is_power_of_two() {
                continue;
            }
            // The mod-2d bit analysis is only exact in a non-wrapping
            // type: constants must fit the width and the hi window must
            // not wrap past the type's max, or the satisfying set isn't
            // the contiguous interval the bit math models.
            let tb = func.dfg.value_type(x1).bits();
            let fits = |v: u64| tb >= 64 || v >> tb == 0;
            if !(fits(k1) && fits(k2) && fits(w1))
                || (k_hi as u128) + (w1 as u128) > (1u128 << tb)
            {
                continue;
            }
            // Exactness: lo range entirely inside a bit-d-clear mod-2d
            // window, hi range entirely inside a bit-d-set one.
            if (k_lo % (2 * d)) + w1 > d || (k_hi % (2 * d)) + w1 > 2 * d || (k_hi % (2 * d)) < d {
                continue;
            }
            if !args_avail(func, &dom, fbc2, term)
                || !body_avail(func, &dom, dest_block(func, fbc2), term)
            {
                continue;
            }
            found = Some((term, x1, k_hi, d, w1, dest_block(func, tbc), fbc2, hit_mats));
            break;
        }
        let Some((term, x, k_hi, d, w, hdest, fbc, mats)) = found else {
            return n;
        };
        let hargs: Vec<BlockArg> = mats.iter().map(|m| emit_mat(func, term, m)).collect();
        let hbc = BlockCall::new(hdest, hargs, &mut func.dfg.value_lists);
        let ty = func.dfg.value_type(x);
        let mut pos = FuncCursor::new(func).at_inst(term);
        let dv = pos.ins().iconst(ty, d as i64);
        let u = pos.ins().bor(x, dv);
        let k = pos.ins().iconst(ty, k_hi as i64);
        let s = pos.ins().isub(u, k);
        let wv = pos.ins().iconst(ty, w as i64);
        let c = pos.ins().icmp(IntCC::UnsignedLessThan, s, wv);
        let InstructionData::Brif { arg, blocks, .. } = &mut func.dfg.insts[term] else {
            return n;
        };
        *arg = c;
        *blocks = [hbc, fbc].into();
        n += 1;
    }
}

pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    // bound_pairs creates the `isub`+`icmp ult` shape range_unions
    // consumes; iterate so a union exposing the next pair gets folded.
    // The dominator tree is rebuilt per round: rewrites move edges.
    let r1 = crate::config::config().enabled("PLIRON_RF1");
    // Unions lengthen the test chain (bor → isub → cmp) but only fire on
    // shared materializable hit args, which measured ~6% on chars_alpha and
    // neutral elsewhere — default on; PLIRON_RF2=0 disables.
    let r2 = crate::config::config().enabled("PLIRON_RF2");
    for _ in 0..4 {
        let k =
            (if r1 { bound_pairs(func) } else { 0 }) + (if r2 { range_unions(func) } else { 0 });
        n += k;
        if k == 0 {
            break;
        }
    }
    n
}

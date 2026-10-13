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
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode,
    ProgramPoint, Value, ValueDef,
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

/// `lo(k)` means `x >= k`; `hi(k)` means `x < k` — so a `[lo, hi)`
/// check is `x - lo u< hi - lo`.
#[derive(Clone, Copy)]
enum Half {
    Lo(u64),
    Hi(u64),
}

fn half_bound(func: &Function, c: Value) -> Option<(Value, Half)> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst()?;
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
        (IntCC::UnsignedLessThanOrEqual, None, Some(k)) => {
            Some((a, Half::Hi(k.checked_add(1)?)))
        }
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

/// Every `BlockArg::Value` on `bc` must be usable at `inst` — an edge
/// moved from its origin block carries args evaluated there, and a def
/// inside the bypassed block (or anywhere not dominating `inst`) would
/// dangle.
fn args_avail(
    func: &Function,
    dom: &DominatorTree,
    bc: BlockCall,
    inst: Inst,
) -> bool {
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

/// `v` defined as `isub x, k` → `(x, k)`.
fn isub_const(func: &Function, v: Value) -> Option<(Value, u64)> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
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
    let i = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst()?;
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
fn bound_pairs(func: &mut Function, dom: &DominatorTree) -> usize {
    let dbg = crate::config::env_set("PLIRON_RF_DEBUG");
    let uses = use_blocks(func);
    let mut rewrites = Vec::new();
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif {
            arg: c, blocks, ..
        } = func.dfg.insts[term]
        else {
            continue;
        };
        let (tbc, fbc) = (blocks[0], blocks[1]);
        let Some((x, Half::Lo(lo))) = half_bound(func, c) else {
            continue;
        };
        if dbg {
            eprintln!("rf: block{b} lo-test x={x} lo={lo}");
        }
        let hi_b = dest_block(func, tbc);
        if hi_b == b {
            continue;
        }
        // The taken arm must be a bare upper-bound test sharing the
        // same miss edge — we bypass it entirely.
        if !bypassable(func, hi_b) || !defs_local(func, &uses, hi_b) {
            if dbg {
                eprintln!("rf:   hi_b{hi_b} impure or escaping");
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
        // Merging the tests hoists the hit edge's args into this block,
        // where they materialize unconditionally — a `mov;stur` flag
        // write LLVM's shape only pays on the lo-test hit (chars_alpha
        // regressed ~50%). Only fold when the hit edge is arg-free.
        if hbc.args(&func.dfg.value_lists).len() != 0 {
            continue;
        }
        let ok = func.dfg.resolve_aliases(x) == func.dfg.resolve_aliases(x2)
            && hi > lo
            && same_edge(func, fbc, mbc)
            && args_avail(func, dom, hbc, term);
        if dbg {
            eprintln!(
                "rf:   x={x}=={x2}? {} hi={hi}>lo? {} miss-eq? {} args? {}",
                func.dfg.resolve_aliases(x) == func.dfg.resolve_aliases(x2),
                hi > lo,
                same_edge(func, fbc, mbc),
                args_avail(func, dom, hbc, term)
            );
        }
        if !ok {
            continue;
        }
        rewrites.push((term, x, lo, hi - lo, hbc, fbc));
    }
    for (term, x, lo, w, hbc, fbc) in &rewrites {
        let ty = func.dfg.value_type(*x);
        let mut pos = FuncCursor::new(func).at_inst(*term);
        let k = pos.ins().iconst(ty, *lo as i64);
        let t = pos.ins().isub(*x, k);
        let wv = pos.ins().iconst(ty, *w as i64);
        let c = pos.ins().icmp(IntCC::UnsignedLessThan, t, wv);
        let InstructionData::Brif { arg, blocks, .. } = &mut func.dfg.insts[*term] else {
            continue;
        };
        *arg = c;
        *blocks = [*hbc, *fbc].into();
    }
    rewrites.len()
}

/// Rule 2: `brif (x-k1 u<w) → HIT else [brif (x-k2 u<w) → HIT else MISS]`
/// becomes `brif ((x|d)-k_hi u<w) → HIT else MISS` when the bit-d
/// partition is exact.
fn range_unions(func: &mut Function, dom: &DominatorTree) -> usize {
    let mut rewrites = Vec::new();
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif {
            arg: c, blocks, ..
        } = func.dfg.insts[term]
        else {
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
        if !bypassable(func, nb) {
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
        if x1 != x2 || w1 != w2 || k1 == k2 || !same_edge(func, tbc, tbc2) {
            continue;
        }
        let (k_lo, k_hi) = (k1.min(k2), k1.max(k2));
        let d = k_hi - k_lo;
        if !d.is_power_of_two() {
            continue;
        }
        // Exactness: lo range entirely inside a bit-d-clear mod-2d
        // window, hi range entirely inside a bit-d-set one.
        if (k_lo % (2 * d)) + w1 > d || (k_hi % (2 * d)) + w1 > 2 * d || (k_hi % (2 * d)) < d {
            continue;
        }
        if !args_avail(func, dom, fbc2, term) {
            continue;
        }
        rewrites.push((term, x1, k_hi, d, w1, tbc, fbc2));
    }
    for (term, x, k_hi, d, w, tbc, fbc) in &rewrites {
        let ty = func.dfg.value_type(*x);
        let mut pos = FuncCursor::new(func).at_inst(*term);
        let dv = pos.ins().iconst(ty, *d as i64);
        let u = pos.ins().bor(*x, dv);
        let k = pos.ins().iconst(ty, *k_hi as i64);
        let s = pos.ins().isub(u, k);
        let wv = pos.ins().iconst(ty, *w as i64);
        let c = pos.ins().icmp(IntCC::UnsignedLessThan, s, wv);
        let InstructionData::Brif { arg, blocks, .. } = &mut func.dfg.insts[*term] else {
            continue;
        };
        *arg = c;
        *blocks = [*tbc, *fbc].into();
    }
    rewrites.len()
}

pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    // bound_pairs creates the `isub`+`icmp ult` shape range_unions
    // consumes; iterate so a union exposing the next pair gets folded.
    // The dominator tree is rebuilt per round: rewrites move edges.
    let r1 = crate::config::config().enabled("PLIRON_RF1");
    // Unions lengthen the test chain (bor → isub → cmp) and measured a
    // regression on chars_alpha's mixed-hit-rate loop — opt-in until the
    // profitability signal is understood.
    let r2 = crate::config::config().is("PLIRON_RF2", "1");
    for _ in 0..4 {
        let cfg = ControlFlowGraph::with_function(func);
        let dom = DominatorTree::with_function(func, &cfg);
        let k = (if r1 { bound_pairs(func, &dom) } else { 0 })
            + (if r2 { range_unions(func, &dom) } else { 0 });
        n += k;
        if k == 0 {
            break;
        }
    }
    n
}

//! Cancel common addends in equality tests (default on; `PLIRON_CMPCANCEL=0` off).
//!
//! `icmp eq/ne (a + b + k1), (b + c + k2)` is `icmp eq/ne (a + (k1-k2)), c`:
//! addition wraps, so adding the same value to both sides of an equality
//! changes nothing. The shape is every pointer-walked loop whose address
//! was re-expressed as `base + idx` (the slice iterator under `count()`,
//! `position()`, `take_while`): the exit test stays
//! `(base + idx + 1) != (base + len)`, which keeps a second induction
//! variable (the pointer), the loop-invariant `base + len`, and the
//! address add out of the load's addressing mode. Cancelled, the test is
//! `idx + 1 != len` on the counter the loop already steps.
//! pulldown-cmark `scan_while` / `scan_ch`: ten instructions an iteration
//! against LLVM's six (`scanners.rs` runs 1.65x LLVM's instructions).
//!
//! Only rewrites when each side comes down to at most one value plus a
//! constant, and reuses an `iadd x, k` that already dominates the test
//! (the counter's own step) before creating one.
//!
//! Two extensions, both from `scan_closing_code_fence`'s second scan
//! (`tests/parity/closing.rs`), whose slice starts at `bytes + found`:
//!
//! - A parameter of a block with one incoming edge is that edge's
//!   argument (loop rotation passes the guard's values to the loop that
//!   way), so sums are read through it.
//! - Inside a loop, `(a + x) + k == c` with `a`, `c` defined outside the
//!   loop and `x` inside becomes `x + k == c - a`, the difference
//!   computed once in the block that enters the loop
//!   (`PLIRON_CMPCANCEL_INV=0` off). There the test was
//!   `(bytes + found) + idx + 1 != bytes + len`: after cancelling `bytes`
//!   the pointer stayed live for the compare, so the load could not use
//!   `[base, idx]` and the loop ran eight instructions a trip; with
//!   `len - found` outside it runs LLVM's six.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value, ValueDef};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_hash::FxHashMap;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// The value a parameter of a single-edge block is bound to.
fn sole_arg(func: &Function, cfg: &ControlFlowGraph, v: Value) -> Option<Value> {
    let ValueDef::Param(b, ix) = func.dfg.value_def(v) else {
        return None;
    };
    let mut preds = cfg.pred_iter(b);
    let p = preds.next()?;
    if preds.next().is_some() {
        return None;
    }
    let pool = &func.dfg.value_lists;
    let mut edges = func.dfg.insts[p.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .iter()
        .filter(|bc| bc.block(pool) == b);
    let bc = edges.next()?;
    if edges.next().is_some() {
        return None;
    }
    bc.args(pool).nth(ix)?.as_value()
}

/// `v` as a sum: its non-constant leaves and the constant part.
fn flatten(func: &Function, cfg: &ControlFlowGraph, v: Value, depth: u32, terms: &mut Vec<Value>, k: &mut i64) {
    let v = func.dfg.resolve_aliases(v);
    if let Some(c) = iconst(func, v) {
        *k = k.wrapping_add(c);
        return;
    }
    if depth > 0 && terms.len() < 6 {
        if let Some(i) = func.dfg.value_def(v).inst()
            && let InstructionData::Binary { opcode: Opcode::Iadd, args } = func.dfg.insts[i]
        {
            flatten(func, cfg, args[0], depth - 1, terms, k);
            flatten(func, cfg, args[1], depth - 1, terms, k);
            return;
        }
        if let Some(a) = sole_arg(func, cfg, v) {
            flatten(func, cfg, a, depth - 1, terms, k);
            return;
        }
    }
    terms.push(v);
}

fn mask(ty: Type, k: i64) -> i64 {
    let bits = ty.bits();
    if bits >= 64 { k } else { k & ((1i64 << bits) - 1) }
}

/// Returns the number of tests rewritten.
pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    // `iadd x, k` already in the function: (x, k) -> definitions.
    let mut steps: FxHashMap<(Value, i64), Vec<(Block, Inst, Value)>> = FxHashMap::default();
    let mut tests: Vec<(Block, Inst)> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            match func.dfg.insts[i] {
                InstructionData::Binary { opcode: Opcode::Iadd, args } => {
                    let (x, y) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
                    let ty = func.dfg.value_type(x);
                    let r = func.dfg.first_result(i);
                    if let Some(k) = iconst(func, y) {
                        steps.entry((x, mask(ty, k))).or_default().push((b, i, r));
                    } else if let Some(k) = iconst(func, x) {
                        steps.entry((y, mask(ty, k))).or_default().push((b, i, r));
                    }
                }
                InstructionData::IntCompare { cond, .. } if matches!(cond, IntCC::Equal | IntCC::NotEqual) => {
                    tests.push((b, i));
                }
                _ => {}
            }
        }
    }
    let inv_on = pliron_cfg::pass_enabled("PLIRON_CMPCANCEL_INV");
    let mut la: Option<LoopAnalysis> = None;
    let mut n = 0;
    for (b, t) in tests {
        let InstructionData::IntCompare { args, .. } = func.dfg.insts[t] else { continue };
        let ty = func.dfg.value_type(args[0]);
        if !ty.is_int() || ty.is_vector() || ty.bits() > 64 {
            continue;
        }
        let (mut lt, mut rt, mut lk, mut rk) = (Vec::new(), Vec::new(), 0i64, 0i64);
        flatten(func, &cfg, args[0], 5, &mut lt, &mut lk);
        flatten(func, &cfg, args[1], 5, &mut rt, &mut rk);
        let mut cancelled = false;
        lt.retain(|x| match rt.iter().position(|y| y == x) {
            Some(j) => {
                rt.swap_remove(j);
                cancelled = true;
                false
            }
            None => true,
        });
        if !cancelled || (lt.is_empty() && rt.is_empty()) {
            continue;
        }
        if lt.len() + rt.len() == 3 && inv_on {
            // One loop-variant value and two invariants: the invariants
            // meet on the right, outside the loop.
            let la = la.get_or_insert_with(|| {
                let mut la = LoopAnalysis::new();
                la.compute(func, &cfg, &domtree);
                la
            });
            let Some(lp) = la.innermost_loop(b) else { continue };
            let inside = |v: Value| match func.dfg.value_def(v) {
                ValueDef::Result(i, _) => func.layout.inst_block(i).is_none_or(|db| la.is_in_loop(db, lp)),
                ValueDef::Param(pb, _) => la.is_in_loop(pb, lp),
                _ => true,
            };
            // (term, on the left side)
            let all: Vec<(Value, bool)> =
                lt.iter().map(|&v| (v, true)).chain(rt.iter().map(|&v| (v, false))).collect();
            let var: Vec<&(Value, bool)> = all.iter().filter(|(v, _)| inside(*v)).collect();
            let [&(x, x_left)] = var[..] else { continue };
            let inv: Vec<(Value, bool)> = all.iter().copied().filter(|&(v, _)| v != x).collect();
            let [(i0, l0), (i1, l1)] = inv[..] else { continue };
            // `x + k == pos - neg` (or `pos + neg2`): an invariant on
            // `x`'s side changes sign.
            // The block that enters the loop: both invariants dominate
            // the header from outside the loop, so they are defined by
            // the end of its immediate dominator.
            let Some(ht) = domtree.idom(la.loop_header(lp)).and_then(|hb| func.layout.last_inst(hb)) else {
                continue;
            };
            if !pliron_cfg::bisect("cmpcancel") {
                continue;
            }
            let k = if x_left { mask(ty, lk.wrapping_sub(rk)) } else { mask(ty, rk.wrapping_sub(lk)) };
            let mut hpos = FuncCursor::new(func).at_inst(ht);
            let other = match (l0 == x_left, l1 == x_left) {
                (false, false) => hpos.ins().iadd(i0, i1),
                (true, false) => hpos.ins().isub(i1, i0),
                (false, true) => hpos.ins().isub(i0, i1),
                (true, true) => continue,
            };
            let step = steps.get(&(x, k)).and_then(|l| {
                l.iter().find_map(|&(db, di, v)| {
                    let ok = if db == b {
                        func.layout.block_insts(b).take_while(|&i| i != t).any(|i| i == di)
                    } else {
                        domtree.block_dominates(db, b)
                    };
                    ok.then_some(v)
                })
            });
            let nl = match step {
                Some(v) => v,
                None if k == 0 => x,
                None => {
                    let mut pos = FuncCursor::new(func).at_inst(t);
                    let c = pos.ins().iconst(ty, k);
                    pos.ins().iadd(x, c)
                }
            };
            if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[t] {
                args[0] = nl;
                args[1] = other;
            }
            n += 1;
            continue;
        }
        if lt.len() > 1 || rt.len() > 1 {
            continue;
        }
        // `L + lk == R + rk`: the constant goes to one side.
        let k_left = mask(ty, lk.wrapping_sub(rk));
        let k_right = mask(ty, rk.wrapping_sub(lk));
        // An existing `x + k` that dominates the test.
        let avail = |func: &Function, x: Value, k: i64| -> Option<Value> {
            steps.get(&(x, k))?.iter().find_map(|&(db, di, v)| {
                let ok = if db == b {
                    func.layout.block_insts(b).take_while(|&i| i != t).any(|i| i == di)
                } else {
                    domtree.block_dominates(db, b)
                };
                ok.then_some(v)
            })
        };
        // (new lhs, new rhs), creating at most one constant and one add.
        enum Side {
            Have(Value),
            Add(Value, i64),
            Const(i64),
        }
        let (l, r) = match (lt.first().copied(), rt.first().copied()) {
            (Some(x), Some(y)) if k_left == 0 => (Side::Have(x), Side::Have(y)),
            (Some(x), Some(y)) => {
                if let Some(v) = avail(func, x, k_left) {
                    (Side::Have(v), Side::Have(y))
                } else if let Some(v) = avail(func, y, k_right) {
                    (Side::Have(x), Side::Have(v))
                } else {
                    (Side::Add(x, k_left), Side::Have(y))
                }
            }
            (Some(x), None) => (Side::Have(x), Side::Const(k_right)),
            (None, Some(y)) => (Side::Const(k_left), Side::Have(y)),
            (None, None) => continue,
        };
        if !pliron_cfg::bisect("cmpcancel") {
            continue;
        }
        let mut pos = FuncCursor::new(func).at_inst(t);
        let mut make = |pos: &mut FuncCursor, s: Side| -> Value {
            match s {
                Side::Have(v) => v,
                Side::Const(k) => pos.ins().iconst(ty, k),
                Side::Add(x, k) => {
                    let c = pos.ins().iconst(ty, k);
                    pos.ins().iadd(x, c)
                }
            }
        };
        let nl = make(&mut pos, l);
        let nr = make(&mut pos, r);
        if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[t] {
            args[0] = nl;
            args[1] = nr;
        }
        n += 1;
    }
    n
}

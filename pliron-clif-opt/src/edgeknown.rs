//! Tests on a block parameter that every incoming edge already decided
//! (`PLIRON_EDGEKNOWN=1`, opt-in).
//!
//! `b(p): brif (p == 0), X, Y` where each predecessor reaches `b` over an
//! edge of its own `brif` that settles whether the argument for `p` is
//! zero:
//!
//! ```text
//! block0:  brif (len == 0), exit, head(len)          ; len != 0 on the edge
//! latch:   rest = isub len, n
//!          brif (len != n), head(rest), exit          ; rest != 0 on the edge
//! head(l): brif (l == 0), empty, body                 ; never taken
//! ```
//!
//! regex's `InstPtrs::next` (`if data.is_empty() { return None }`, then
//! `for b in data.iter()`): the rotated loop keeps the guard on both
//! edges and the slice iterator's first `ptr == end` — `len == 0` after
//! folding — is retested in the header on every iteration. jumpthread's
//! facts are per dominating branch on one value; here the value differs
//! per edge (a block argument) and the latch's fact is about a
//! difference. An edge's argument is known when it is a constant, the
//! predecessor's own condition, the value that condition compares with
//! zero, or `isub`/`bxor` of the two values it compares for equality.
//! When all edges agree the `brif` becomes a `jump`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value};

fn iconst(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// `v` as an equality test: `(x, y, true)` for `x != y`, `false` for `==`.
fn eq_test(func: &Function, v: Value) -> Option<(Value, Value, bool)> {
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::IntCompare { cond, args, .. } if matches!(cond, IntCC::Equal | IntCC::NotEqual) => Some((
            func.dfg.resolve_aliases(args[0]),
            func.dfg.resolve_aliases(args[1]),
            cond == IntCC::NotEqual,
        )),
        _ => None,
    }
}

/// `q` where `sum` is `x + q` (either operand order).
fn added(func: &Function, x: Value, sum: Value) -> Option<Value> {
    match func.dfg.insts[func.dfg.value_def(sum).inst()?] {
        InstructionData::Binary { opcode: Opcode::Iadd, args } => {
            let (p, q) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
            if p == x {
                Some(q)
            } else if q == x {
                Some(p)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether `a` is non-zero (`Some(true)`), zero (`Some(false)`) or
/// undecided when control leaves `term` over its `then` (or else) edge.
fn edge_fact(func: &Function, term: Inst, then: bool, a: Value) -> Option<bool> {
    let a = func.dfg.resolve_aliases(a);
    if let Some(k) = iconst(func, a) {
        // Narrow constants carry their bits sign-extended or not; zero is zero.
        let bits = func.dfg.value_type(a).bits();
        let k = if bits >= 64 { k as u64 } else { (k as u64) & ((1u64 << bits) - 1) };
        return Some(k != 0);
    }
    let InstructionData::Brif { arg, .. } = func.dfg.insts[term] else {
        return None;
    };
    let c = func.dfg.resolve_aliases(arg);
    if c == a {
        return Some(then);
    }
    let (x, y, ne) = eq_test(func, c)?;
    // `x != y` holds on this edge?
    let differ = ne == then;
    if (x == a && iconst(func, y) == Some(0)) || (y == a && iconst(func, x) == Some(0)) {
        return Some(differ);
    }
    if let Some(d) = func.dfg.value_def(a).inst()
        && let InstructionData::Binary { opcode: Opcode::Isub | Opcode::Bxor, args } = func.dfg.insts[d]
    {
        let (p, q) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
        if (p == x && q == y) || (p == y && q == x) {
            return Some(differ);
        }
    }
    None
}

/// Returns the number of `brif`s that became `jump`s.
pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        if Some(b) == entry {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else { continue };
        let InstructionData::Brif { arg, blocks: [bt, bf], .. } = func.dfg.insts[t] else {
            continue;
        };
        // The tested parameter, and whether non-zero takes the `then` edge.
        let c = func.dfg.resolve_aliases(arg);
        let params = func.dfg.block_params(b);
        let (p, nz_then) = if params.contains(&c) {
            (c, true)
        } else if let Some((x, y, ne)) = eq_test(func, c) {
            if params.contains(&x) && iconst(func, y) == Some(0) {
                (x, ne)
            } else if params.contains(&y) && iconst(func, x) == Some(0) {
                (y, ne)
            } else if let Some(q) = added(func, x, y).or_else(|| added(func, y, x))
                && params.contains(&q)
            {
                // `x == x + q` is `q == 0` (a slice iterator's
                // `ptr == ptr + len` before the egraph folds it).
                (q, ne)
            } else {
                continue;
            }
        } else {
            continue;
        };
        let pi = params.iter().position(|&x| x == p).unwrap();
        let mut known: Option<bool> = None;
        let mut ok = true;
        let mut any = false;
        for pred in cfg.pred_iter(b) {
            any = true;
            let dests = func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            let hits: Vec<usize> =
                (0..dests.len()).filter(|&i| dests[i].block(&func.dfg.value_lists) == b).collect();
            let fact = match hits[..] {
                [i] => match dests[i].args(&func.dfg.value_lists).nth(pi) {
                    Some(BlockArg::Value(a)) => edge_fact(func, pred.inst, i == 0, a),
                    _ => None,
                },
                _ => None,
            };
            match (fact, known) {
                (Some(f), None) => known = Some(f),
                (Some(f), Some(k)) if f == k => {}
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        let Some(nz) = known else { continue };
        if !ok || !any || !pliron_cfg::bisect("edgeknown") {
            continue;
        }
        let keep = if nz == nz_then { bt } else { bf };
        let (kb, kargs): (Block, Vec<BlockArg>) =
            (keep.block(&func.dfg.value_lists), keep.args(&func.dfg.value_lists).collect());
        func.layout.remove_inst(t);
        FuncCursor::new(func).at_bottom(b).ins().jump(kb, &kargs);
        n += 1;
    }
    n
}

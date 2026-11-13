//! Tests on a block parameter that every incoming edge already decided
//! (default on; `PLIRON_EDGEKNOWN=0` off). Two relatives live here:
//! `selects` (`PLIRON_SELKNOWN=0` off) folds selects whose condition is
//! decided where they stand, and `retests` (`PLIRON_RETEST=0` off)
//! folds a branch that repeats the test on its block's only way in.
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

/// The fact for `b`'s parameter `pi` if every incoming edge agrees.
fn param_fact(func: &Function, cfg: &ControlFlowGraph, b: Block, pi: usize) -> Option<bool> {
    let mut known: Option<bool> = None;
    for pred in cfg.pred_iter(b) {
        let dests = func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        let hits: Vec<usize> = (0..dests.len()).filter(|&i| dests[i].block(&func.dfg.value_lists) == b).collect();
        let [i] = hits[..] else { return None };
        let Some(BlockArg::Value(a)) = dests[i].args(&func.dfg.value_lists).nth(pi) else {
            return None;
        };
        let mut fact = edge_fact(func, pred.inst, i == 0, a);
        // A plain `jump` out of a block with one predecessor: the edge
        // into that block decided it (a split edge, a landing block).
        if fact.is_none()
            && matches!(func.dfg.insts[pred.inst], InstructionData::Jump { .. })
            && Some(pred.block) != func.layout.entry_block()
        {
            let mut ps = cfg.pred_iter(pred.block);
            if let (Some(pp), None) = (ps.next(), ps.next()) {
                let d2 = func.dfg.insts[pp.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                let h2: Vec<usize> =
                    (0..d2.len()).filter(|&i| d2[i].block(&func.dfg.value_lists) == pred.block).collect();
                if let [j] = h2[..] {
                    fact = edge_fact(func, pp.inst, j == 0, a);
                }
            }
        }
        match (fact, known) {
            (Some(f), None) => known = Some(f),
            (Some(f), Some(k)) if f == k => {}
            _ => return None,
        }
    }
    known
}

/// Returns the number of `brif`s that became `jump`s.
pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    // Parameter -> (its block, its index).
    let mut home: rustc_hash::FxHashMap<Value, (Block, usize)> = rustc_hash::FxHashMap::default();
    for &b in &blocks {
        if Some(b) == entry {
            continue;
        }
        for (i, &p) in func.dfg.block_params(b).iter().enumerate() {
            home.insert(p, (b, i));
        }
    }
    let mut facts: rustc_hash::FxHashMap<Value, Option<bool>> = rustc_hash::FxHashMap::default();
    let mut n = 0;
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let InstructionData::Brif { arg, blocks: [bt, bf], .. } = func.dfg.insts[t] else {
            continue;
        };
        // The tested parameter (of this block or one that dominates it:
        // a parameter's value is fixed once its block is entered), and
        // whether non-zero takes the `then` edge.
        let c = func.dfg.resolve_aliases(arg);
        let (p, nz_then) = if home.contains_key(&c) {
            (c, true)
        } else if let Some((x, y, ne)) = eq_test(func, c) {
            if home.contains_key(&x) && iconst(func, y) == Some(0) {
                (x, ne)
            } else if home.contains_key(&y) && iconst(func, x) == Some(0) {
                (y, ne)
            } else if let Some(q) = added(func, x, y).or_else(|| added(func, y, x))
                && home.contains_key(&q)
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
        let (hb, pi) = home[&p];
        let Some(nz) = *facts.entry(p).or_insert_with(|| param_fact(func, &cfg, hb, pi)) else {
            continue;
        };
        if !pliron_cfg::bisect("edgeknown") {
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

/// `(a, b, true)` for `a != b`, `false` for `a == b`, operands in a fixed
/// order.
fn eq_key(func: &Function, v: Value) -> Option<(Value, Value, bool)> {
    let (a, b, ne) = eq_test(func, v)?;
    Some(if a.as_u32() <= b.as_u32() { (a, b, ne) } else { (b, a, ne) })
}

/// A `brif` that repeats the test its block's only way in already made.
///
/// ```text
/// block0: brif (len == 0), empty, block2
/// block2: brif (len == 0), exit, loop          ; one predecessor: never taken
/// ```
///
/// The second test is a rotated loop's guard: `p == p + len` until
/// `cmpcancel` rewrites it at the end of the pipeline, and the last
/// dominator GVN then finds it equal to the caller's `is_empty` check —
/// after every pass that folds decided branches has run
/// (`tests/parity/closing.rs`: `cbz x1` twice in a row). Climbs
/// single-predecessor edges (at most eight) looking for a `brif` on the
/// same value, or on an `icmp eq/ne` of the same operands.
pub fn retests(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let InstructionData::Brif { arg, blocks: [bt, bf], .. } = func.dfg.insts[t] else {
            continue;
        };
        let c = func.dfg.resolve_aliases(arg);
        let ck = eq_key(func, c);
        let mut cur = b;
        let mut known: Option<bool> = None;
        for _ in 0..8 {
            let mut ps = cfg.pred_iter(cur);
            let (Some(p), None) = (ps.next(), ps.next()) else { break };
            if p.block == cur {
                break;
            }
            if let InstructionData::Brif { arg: pa, blocks: [pt, pf], .. } = func.dfg.insts[p.inst] {
                let (tb, fb) = (pt.block(&func.dfg.value_lists), pf.block(&func.dfg.value_lists));
                if tb != fb {
                    let taken = tb == cur;
                    let pc = func.dfg.resolve_aliases(pa);
                    if pc == c {
                        known = Some(taken);
                        break;
                    }
                    if let (Some((x, y, ne)), Some((px, py, pne))) = (ck, eq_key(func, pc))
                        && (x, y) == (px, py)
                    {
                        // The operands differ on this edge iff `pne == taken`.
                        known = Some((pne == taken) == ne);
                        break;
                    }
                }
            }
            cur = p.block;
        }
        let Some(k) = known else { continue };
        if !pliron_cfg::bisect("edgeknown") {
            continue;
        }
        let keep = if k { bt } else { bf };
        let (kb, kargs): (Block, Vec<BlockArg>) =
            (keep.block(&func.dfg.value_lists), keep.args(&func.dfg.value_lists).collect());
        func.layout.remove_inst(t);
        FuncCursor::new(func).at_bottom(b).ins().jump(kb, &kargs);
        n += 1;
    }
    n
}

/// Selects whose condition is already decided where they stand.
///
/// A slice iterator's `next` — "at the end? stay : step" — if-converts to
/// `p' = select (icmp eq p, end), p, p + 1`, inside a loop that only runs
/// while `p != end`: the test is dead, and the select and its second
/// compare cost two instructions a trip (pulldown-cmark's
/// `scan_ch_repeat` loops: nine instructions where seven do).
/// Two sources of the fact:
/// - a dominating `brif` on an equal compare (same operands) whose edge
///   to this region is the block's only way in;
/// - a block parameter `p` whose every incoming edge is the "differs"
///   (or "equals") side of a `brif` comparing that edge's argument with
///   one common value `e`: then `p != e` (`p == e`) wherever `p` is
///   visible.
/// The select becomes an alias of the arm it always takes.
pub fn selects(func: &mut Function) -> usize {
    use cranelift_codegen::dominator_tree::DominatorTree;
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let blocks: Vec<Block> = func.layout.blocks().collect();
    if blocks.len() > 2000 {
        return 0;
    }
    // (operands, "differs" holds, region root).
    let mut facts: Vec<((Value, Value), bool, Block)> = Vec::new();
    for &b in &blocks {
        // Dominating branch: its single-predecessor successors.
        if let Some(t) = func.layout.last_inst(b)
            && let InstructionData::Brif { arg, blocks: [bt, bf], .. } = func.dfg.insts[t]
            && let Some((x, y, ne)) = eq_key(func, func.dfg.resolve_aliases(arg))
        {
            let (tb, fb) = (bt.block(&func.dfg.value_lists), bf.block(&func.dfg.value_lists));
            if tb != fb {
                for (s, taken) in [(tb, true), (fb, false)] {
                    let mut ps = cfg.pred_iter(s);
                    if let (Some(_), None) = (ps.next(), ps.next()) {
                        // `ne` true and taken: differs. `ne` false and not taken: differs.
                        facts.push(((x, y), ne == taken, s));
                    }
                }
            }
        }
        // Block parameters decided on every edge.
        if Some(b) == func.layout.entry_block() {
            continue;
        }
        for (pi, &p) in func.dfg.block_params(b).iter().enumerate() {
            let mut common: Option<(Value, bool)> = None;
            let mut ok = true;
            let mut any = false;
            for pred in cfg.pred_iter(b) {
                any = true;
                let dests =
                    func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                let hits: Vec<usize> = (0..dests.len()).filter(|&i| dests[i].block(&func.dfg.value_lists) == b).collect();
                let fact = (|| {
                    let [i] = hits[..] else { return None };
                    let BlockArg::Value(a) = dests[i].args(&func.dfg.value_lists).nth(pi)? else { return None };
                    let a = func.dfg.resolve_aliases(a);
                    let InstructionData::Brif { arg, .. } = func.dfg.insts[pred.inst] else { return None };
                    let (x, y, ne) = eq_test(func, func.dfg.resolve_aliases(arg))?;
                    let e = if x == a {
                        y
                    } else if y == a {
                        x
                    } else {
                        return None;
                    };
                    // The other operand must not be this block's own
                    // parameter (it would be a different value per edge).
                    if func.dfg.block_params(b).contains(&e) {
                        return None;
                    }
                    Some((e, ne == (i == 0)))
                })();
                match (fact, common) {
                    (Some(f), None) => common = Some(f),
                    (Some(f), Some(c)) if f == c => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && any && let Some((e, differs)) = common {
                // `e` reaches `b`: it is an operand in a predecessor that
                // `b` does not dominate (the loop's entry edge), or a
                // constant; require one such predecessor.
                let outside = cfg.pred_iter(b).any(|pr| !domtree.block_dominates(b, pr.block));
                if outside {
                    let key = if p.as_u32() <= e.as_u32() { (p, e) } else { (e, p) };
                    facts.push((key, differs, b));
                }
            }
        }
    }
    if facts.is_empty() {
        return 0;
    }
    let mut n = 0;
    // If-conversion also leaves the select *above* the branch that tests
    // the same condition (`p' = select (p == end), p, p + 1; brif (p ==
    // end), exit, body`): the select itself is not in the decided region,
    // its uses are. Every use in a decided block takes the arm directly.
    let mut sel: rustc_hash::FxHashMap<Value, ((Value, Value), bool, Value, Value)> = rustc_hash::FxHashMap::default();
    for &b in &blocks {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Ternary { opcode: Opcode::Select, args } = func.dfg.insts[i]
                && let Some((x, y, ne)) = eq_key(func, func.dfg.resolve_aliases(args[0]))
            {
                sel.insert(func.dfg.first_result(i), ((x, y), ne, args[1], args[2]));
            }
        }
    }
    for &b in &blocks {
        let live: Vec<&((Value, Value), bool, Block)> =
            facts.iter().filter(|f| domtree.block_dominates(f.2, b)).collect();
        if live.is_empty() {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for &i in &insts {
            let mut rmap: Vec<(Value, Value)> = Vec::new();
            for v in func.dfg.inst_values(i) {
                let rv = func.dfg.resolve_aliases(v);
                if let Some(&(key, ne, a, bb)) = sel.get(&rv)
                    && let Some(f) = live.iter().find(|f| f.0 == key)
                {
                    rmap.push((v, if f.1 == ne { a } else { bb }));
                }
            }
            if rmap.is_empty() || !pliron_cfg::bisect("edgeknown") {
                continue;
            }
            func.dfg.map_inst_values(i, |v| rmap.iter().find(|(o, _)| *o == v).map_or(v, |&(_, nv)| nv));
            n += 1;
        }
        for i in insts {
            if func.layout.inst_block(i).is_none() {
                continue;
            }
            let InstructionData::Ternary { opcode: Opcode::Select, args } = func.dfg.insts[i] else { continue };
            let Some((x, y, ne)) = eq_key(func, func.dfg.resolve_aliases(args[0])) else { continue };
            let Some(f) = live.iter().find(|f| f.0 == (x, y)) else { continue };
            if !pliron_cfg::bisect("edgeknown") {
                continue;
            }
            // The condition is true when "differs" matches the compare's sense.
            let pick = if f.1 == ne { args[1] } else { args[2] };
            let r = func.dfg.first_result(i);
            func.dfg.clear_results(i);
            func.dfg.change_to_alias(r, pick);
            func.layout.remove_inst(i);
            n += 1;
        }
    }
    n
}

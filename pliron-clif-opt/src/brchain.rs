//! Small `br_table` → compare-chain conversion. Cranelift lowers every
//! `br_table` to an indirect branch (`adr; ldrsw; add; br xN`) plus a
//! bounds test — roughly eight instructions and an indirect-branch
//! prediction on every execution — where LLVM turns small or sparse
//! switches into ordinary conditional branches. Iterator state machines
//! (`Option`/`match` discriminants in `chars()`/`split_whitespace()`-style
//! loops) hit this in the per-character path: the dispatch alone was ~25%
//! of the hot loop body and the `br` relies on indirect prediction.
//!
//! For at most `MAX_ENTRIES` live table entries the chain is shorter,
//! predictable, and semantically exact: `br_table` sends index `v >= n`
//! to the default block, and `icmp eq v, k` matches no index past the
//! last test, so the final `brif`'s else edge reproduces the default.
//! Entries identical to the default (same block, same args) are skipped —
//! matching them would be equivalent to taking the default.
//!
//! Table indices are tested in order; duplicate target blocks with
//! different args stay distinct tests since each edge keeps its own
//! arguments.
//!
//! `PLIRON_BRCHAIN=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Value, ValueDef,
};
use rustc_hash::FxHashMap;

use crate::jumpthread::pure_op;

/// Maximum number of live (non-default-equivalent) table entries that
/// get a compare chain. Above that the indirect branch wins on code
/// size: the chain costs `icmp`+`brif` per entry vs ~8 instructions
/// total for the table dispatch.
const MAX_ENTRIES: usize = 4;

/// Whether `a` and `b` name the same block with the same arguments —
/// then an explicit test for that index is redundant with the default.
fn same_target(func: &Function, a: BlockCall, b: BlockCall) -> bool {
    a.block(&func.dfg.value_lists) == b.block(&func.dfg.value_lists)
        && a.args(&func.dfg.value_lists)
            .eq(b.args(&func.dfg.value_lists))
}

fn dest(func: &Function, bc: BlockCall) -> (Block, Vec<BlockArg>) {
    (
        bc.block(&func.dfg.value_lists),
        bc.args(&func.dfg.value_lists).collect(),
    )
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: cranelift_codegen::ir::Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// One argument of an incoming edge as a dispatch decision: a constant
/// resolves to one table entry outright, a const-armed `select` narrows
/// to two (needing a `brif` in a `jump`-only pred).
#[derive(Clone, Copy)]
enum EdgeArg {
    K(i64),
    Sel(Value, i64, i64),
}

/// `br_table (… Param(blk, i))` where every incoming edge supplies an
/// `iconst` or a const-armed `select` for that param: each edge already
/// knows — or narrows to — its table entry, so thread the dispatch into
/// the preds and let the table block go dead. This is jump threading
/// for switches: `split_whitespace` merges
/// `state = if ws { 0 } else { 1 }` from two preds into exactly this
/// shape, and LLVM resolves the same state machine at the edges.
fn param_dispatch(func: &mut Function, cfg: &ControlFlowGraph, term: Inst) -> Option<usize> {
    let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[term] else {
        return None;
    };
    let jt = &func.dfg.jump_tables[table];
    let entries: Vec<BlockCall> = jt.as_slice().to_vec();
    let dflt_bc = jt.default_block();
    let n = entries.len() as u64;
    let w = func.dfg.value_type(arg).bits();
    let m = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    let entry = |k: i64| -> BlockCall {
        let k = (k as u64) & m;
        if k < n { entries[k as usize] } else { dflt_bc }
    };
    // Chase the dispatch value through conversions to its block param,
    // recording each conversion's result-mask width so the index const
    // can be normalized to what the table actually reads (a sign- or
    // over-wide-spelled `iconst` imm on the param is not the dispatched
    // index).
    let mut v = arg;
    let mut conv_masks: Vec<u32> = Vec::new();
    let (blk, num) = loop {
        v = func.dfg.resolve_aliases(v);
        match func.dfg.value_def(v) {
            ValueDef::Result(d, _)
                if matches!(
                    func.dfg.insts[d],
                    InstructionData::Unary {
                        opcode: cranelift_codegen::ir::Opcode::Uextend
                            | cranelift_codegen::ir::Opcode::Ireduce,
                        ..
                    }
                ) =>
            {
                let InstructionData::Unary { arg, .. } = func.dfg.insts[d] else {
                    unreachable!()
                };
                // `uextend`: the index is the operand's value zero-extended,
                // so normalize `k` to the operand width. `ireduce`: the
                // index truncates to this inst's result width.
                conv_masks.push(match func.dfg.insts[d].opcode() {
                    cranelift_codegen::ir::Opcode::Uextend => func.dfg.value_type(arg).bits(),
                    _ => func.dfg.value_type(v).bits(),
                });
                v = arg;
            }
            ValueDef::Param(b, num) => break (b, num),
            _ => return None,
        }
    };
    // `blk` dies once its edges are threaded: `term` must live in `blk`
    // (a dominated block's code would be skipped too), every
    // non-terminator inst must be pure, and no inst result it defines may
    // be used outside — the rebound-param machinery only covers `blk`'s
    // params.
    if func.layout.inst_block(term) != Some(blk) {
        return None;
    }
    for i in func.layout.block_insts(blk) {
        if i == term {
            continue;
        }
        if !pure_op(func, i) {
            return None;
        }
    }
    // A `k` edge arg is the param's constant — apply the conversion
    // chain's narrowing masks (walked outermost-first) before indexing.
    let norm = |k: i64| -> i64 {
        conv_masks.iter().rev().fold(k, |k, &b| {
            if b >= 64 {
                k
            } else {
                k & ((1i64 << b) - 1)
            }
        })
    };
    // Classify every incoming edge's argument for `param[num]`; a
    // select arg is only expressible where the pred's terminator is a
    // `jump` we can turn into `brif` — a `brif`/`br_table` pred has no
    // room for a second condition. Each edge's full arg list is kept
    // for substitution — later rewrites of sibling edges must not be
    // re-read.
    let mut work = Vec::new();
    for p in cfg.pred_iter(blk) {
        for (di, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) != blk {
                continue;
            }
            let src: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            // Non-`Value` edge args (try_call payloads) can't be
            // re-expressed on the resolved targets, and the arg list must
            // cover every `blk` param (`src[j]` substitutions index it).
            if src.len() != func.dfg.block_params(blk).len()
                || !src.iter().all(|a| matches!(a, BlockArg::Value(_)))
            {
                return None;
            }
            let ea = match src.get(num) {
                Some(BlockArg::Value(a)) => edge_arg(func, *a),
                _ => None,
            }?;
            let is_jump = matches!(func.dfg.insts[p.inst], InstructionData::Jump { .. });
            let ea = match ea {
                EdgeArg::K(_) => ea,
                EdgeArg::Sel(..) if is_jump => ea,
                // Two-way arg on an edge we can't split: unless both
                // arms resolve to the same call, threading is
                // impossible.
                EdgeArg::Sel(_, ka, kb) if same_target(func, entry(norm(ka)), entry(norm(kb))) => {
                    EdgeArg::K(ka)
                }
                _ => return None,
            };
            work.push((p.inst, di, src, ea));
        }
    }
    if work.is_empty() {
        return None;
    }
    // An edge arg on a resolved target may name a `blk` param (bound per
    // edge via `src`) or a value defined above this dispatch. Anything
    // defined inside `blk` — or anywhere not dominating the edge's pred —
    // would dangle on the new edge, so validate them all before
    // rewriting: a non-`blk`-param arg must dominate its edge's block.
    let dt_lazy = std::cell::RefCell::new(None::<DominatorTree>);
    for (pinst, _, _, ea) in &work {
        let Some(pblk) = func.layout.inst_block(*pinst) else {
            return None;
        };
        let ks: &[i64] = match *ea {
            EdgeArg::K(k) => &[k],
            EdgeArg::Sel(_, ka, kb) => &[ka, kb],
        };
        for &k in ks {
            let call = entry(norm(k));
            for a in call.args(&func.dfg.value_lists) {
                let BlockArg::Value(av) = a else {
                    continue;
                };
                let r = func.dfg.resolve_aliases(av);
                if let ValueDef::Param(b, _) = func.dfg.value_def(r)
                    && b == blk
                {
                    continue;
                }
                let db = match func.dfg.value_def(r) {
                    ValueDef::Param(pb, _) => pb,
                    ValueDef::Result(i, _) => match func.layout.inst_block(i) {
                        Some(db) => db,
                        None => return None,
                    },
                    _ => return None,
                };
                let mut guard = dt_lazy.borrow_mut();
                let dt = guard.get_or_insert_with(|| DominatorTree::with_function(func, cfg));
                if !dt.block_dominates(db, pblk) {
                    return None;
                }
            }
        }
    }
    // Resolved targets — threading removes `blk`, so a self-entry
    // (table target == `blk`) can't be expressed.
    let mut tblocks = Vec::new();
    for (_, _, _, ea) in &work {
        match *ea {
            EdgeArg::K(k) => tblocks.push(entry(norm(k)).block(&func.dfg.value_lists)),
            EdgeArg::Sel(_, ka, kb) => {
                tblocks.push(entry(norm(ka)).block(&func.dfg.value_lists));
                tblocks.push(entry(norm(kb)).block(&func.dfg.value_lists));
            }
        }
    }
    tblocks.sort_unstable();
    tblocks.dedup();
    if tblocks.contains(&blk) {
        return None;
    }
    // `blk`'s params may be named directly inside the region it
    // dominates (a `br_table`'s targets often use the merged loop-carried
    // state without taking it as their own params). Every such use is
    // rebound through the unique target dominating its block — that
    // target gets the param appended, and each rewritten edge supplies
    // the value it would have bound. A rebound target must itself be
    // dominated by `blk` — a target above the dispatch (a loop header
    // the table feeds back into) never sees `blk`'s bindings. A use
    // dominated by no eligible target would need a phi — bail instead.
    let params: Vec<Value> = func.dfg.block_params(blk).to_vec();
    let mut needed: FxHashMap<Block, Vec<Value>> = FxHashMap::default();
    let mut assign: Vec<(Block, Block)> = Vec::new();
    {
        let mut dt = None;
        for x in func.layout.blocks() {
            if x == blk {
                continue;
            }
            let mut used = Vec::new();
            for i in func.layout.block_insts(x) {
                for v in func.dfg.inst_values(i) {
                    let r = func.dfg.resolve_aliases(v);
                    // A result defined inside `blk` can't be rebound —
                    // its uses outside would dangle once `blk` dies.
                    if let ValueDef::Result(d, _) = func.dfg.value_def(r)
                        && func.layout.inst_block(d) == Some(blk)
                    {
                        return None;
                    }
                    if params.contains(&r) && !used.contains(&r) {
                        used.push(r);
                    }
                }
            }
            if used.is_empty() {
                continue;
            }
            let dt = dt.get_or_insert_with(|| DominatorTree::with_function(func, cfg));
            let Some(&t) = tblocks
                .iter()
                .find(|&&t| dt.block_dominates(blk, t) && dt.block_dominates(t, x))
            else {
                return None;
            };
            assign.push((x, t));
            let ns = needed.entry(t).or_default();
            for p in used {
                if !ns.contains(&p) {
                    ns.push(p);
                }
            }
        }
    }
    // A target getting rebound params must be reachable only through
    // this term's edges — any other pred would leave the new params
    // unbound.
    for &t in needed.keys() {
        if cfg.pred_iter(t).any(|p| p.inst != term) {
            return None;
        }
    }
    // Append the rebound params and rewrite their uses to the fresh
    // params of the dominating target.
    let mut newp: FxHashMap<(Block, Value), Value> = FxHashMap::default();
    for (t, ns) in &needed {
        for &p in ns {
            let ty = func.dfg.value_type(p);
            let np = func.dfg.append_block_param(*t, ty);
            newp.insert((*t, p), np);
        }
    }
    for (x, t) in &assign {
        let insts: Vec<Inst> = func.layout.block_insts(*x).collect();
        for i in insts {
            let repl: FxHashMap<Value, Value> = func
                .dfg
                .inst_values(i)
                .filter_map(|v| {
                    let r = func.dfg.resolve_aliases(v);
                    newp.get(&(*t, r)).map(|&np| (v, np))
                })
                .collect();
            if !repl.is_empty() {
                func.dfg
                    .map_inst_values(i, |v| repl.get(&v).copied().unwrap_or(v));
            }
        }
    }
    let mut n = 0;
    for (pinst, di, src, ea) in work {
        // Substitute `blk`'s params with this edge's own args inside the
        // resolved target's call, then supply the rebound params.
        let finish = |func: &Function, call: BlockCall| -> (Block, Vec<BlockArg>) {
            let tb = call.block(&func.dfg.value_lists);
            let mut args: Vec<BlockArg> = call
                .args(&func.dfg.value_lists)
                .map(|a| match a {
                    BlockArg::Value(av) => {
                        match func.dfg.value_def(func.dfg.resolve_aliases(av)) {
                            ValueDef::Param(b, j) if b == blk => src[j],
                            _ => a,
                        }
                    }
                    _ => a,
                })
                .collect();
            if let Some(ns) = needed.get(&tb) {
                for &p in ns {
                    let j = params.iter().position(|&x| x == p).unwrap();
                    args.push(src[j]);
                }
            }
            (tb, args)
        };
        match ea {
            EdgeArg::K(k) => {
                let (tb, targs) = finish(func, entry(norm(k)));
                let dfg = &mut func.dfg;
                let bc = BlockCall::new(tb, targs, &mut dfg.value_lists);
                dfg.insts[pinst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                    [di] = bc;
                n += 1;
            }
            EdgeArg::Sel(c, ka, kb) => {
                let (tb, targs) = finish(func, entry(norm(ka)));
                let (fb, fargs) = finish(func, entry(norm(kb)));
                let pblk = func.layout.inst_block(pinst)?;
                func.layout.remove_inst(pinst);
                let mut pos = FuncCursor::new(func).at_bottom(pblk);
                pos.ins().brif(c, tb, &targs, fb, &fargs);
                n += 1;
            }
        }
    }
    Some(n)
}

fn edge_arg(func: &Function, a: Value) -> Option<EdgeArg> {
    let a = func.dfg.resolve_aliases(a);
    if let Some(k) = iconst(func, a) {
        return Some(EdgeArg::K(k));
    }
    let d = func.dfg.value_def(a).inst()?;
    if let InstructionData::Ternary {
        opcode: cranelift_codegen::ir::Opcode::Select,
        args: [c, x, y],
    } = func.dfg.insts[d]
        && let (Some(kx), Some(ky)) = (iconst(func, x), iconst(func, y))
    {
        return Some(EdgeArg::Sel(func.dfg.resolve_aliases(c), kx, ky));
    }
    None
}

/// `br_table x` where `x` is `select c, ka, kb` (through one
/// `uextend`/`ireduce`): `x` only ever yields an arm constant, so the
/// dispatch is `c`'s truth value between `table[ka]` and `table[kb]` —
/// a single `brif` on `c`, and the `select` itself never materializes.
/// `split_whitespace`'s `state = if ws { 0 } else { 1 }` feeds exactly
/// this shape. Returns `(cond, true-target, false-target)`.
fn select_dispatch(
    func: &Function,
    cfg: &ControlFlowGraph,
    term: Inst,
) -> Option<(Value, (Block, Vec<BlockArg>), (Block, Vec<BlockArg>))> {
    let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[term] else {
        return None;
    };
    // Chase `v` through conversions and single-pred block params — the
    // select often feeds the table via `jump dst(sel, ..)` where the
    // table arg is `dst`'s param (`split_whitespace` shapes the dispatch
    // exactly this way).
    let mut v = arg;
    let mut conv_masks: Vec<u32> = Vec::new();
    for _ in 0..4 {
        v = func.dfg.resolve_aliases(v);
        match func.dfg.value_def(v) {
            ValueDef::Result(d, _)
                if matches!(
                    func.dfg.insts[d],
                    InstructionData::Unary {
                        opcode: cranelift_codegen::ir::Opcode::Uextend
                            | cranelift_codegen::ir::Opcode::Ireduce,
                        ..
                    }
                ) =>
            {
                let InstructionData::Unary { arg, .. } = func.dfg.insts[d] else {
                    unreachable!()
                };
                // Same normalization as `param_dispatch`: `uextend`
                // masks to the operand width, `ireduce` to the result.
                conv_masks.push(match func.dfg.insts[d].opcode() {
                    cranelift_codegen::ir::Opcode::Uextend => func.dfg.value_type(arg).bits(),
                    _ => func.dfg.value_type(v).bits(),
                });
                v = arg;
            }
            ValueDef::Param(b, num) => {
                let mut preds = cfg.pred_iter(b);
                let Some(p) = preds.next() else { break };
                if preds.next().is_some() {
                    break;
                }
                let mut found = None;
                for bc in func.dfg.insts[p.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if bc.block(&func.dfg.value_lists) == b {
                        found = bc.args(&func.dfg.value_lists).nth(num);
                    }
                }
                let Some(BlockArg::Value(a)) = found else { break };
                v = a;
            }
            _ => break,
        }
    }
    let d = func.dfg.value_def(v).inst()?;
    let InstructionData::Ternary {
        opcode: cranelift_codegen::ir::Opcode::Select,
        args: [c, a, b],
    } = func.dfg.insts[d]
    else {
        return None;
    };
    let (Some(ka), Some(kb)) = (iconst(func, a), iconst(func, b)) else {
        return None;
    };
    let jt = &func.dfg.jump_tables[table];
    let n = jt.as_slice().len() as u64;
    let dflt = jt.default_block();
    let w = func.dfg.value_type(arg).bits();
    let m = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    let norm = |k: i64| -> i64 {
        conv_masks.iter().rev().fold(k, |k, &b| {
            if b >= 64 {
                k
            } else {
                k & ((1i64 << b) - 1)
            }
        })
    };
    let call = |k: i64| -> (Block, Vec<BlockArg>) {
        let k = (norm(k) as u64) & m;
        if k < n {
            dest(func, jt.as_slice()[k as usize])
        } else {
            dest(func, dflt)
        }
    };
    Some((func.dfg.resolve_aliases(c), call(ka), call(kb)))
}

pub fn run(func: &mut Function) -> usize {
    let mut todo = Vec::new();
    for b in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[term] else {
            continue;
        };
        let jt = &func.dfg.jump_tables[table];
        let dflt = jt.default_block();
        // Live entries: drop ones equal to the default — those indices
        // fold into the default edge.
        let mut live = Vec::new();
        for (i, &entry) in jt.as_slice().iter().enumerate() {
            if !same_target(func, entry, dflt) {
                live.push((i as u64, dest(func, entry)));
            }
        }
        if live.is_empty() {
            // Every index lands on the default: plain jump.
            todo.push((b, term, arg, live, dest(func, dflt)));
            continue;
        }
        if live.len() > MAX_ENTRIES {
            continue;
        }
        todo.push((b, term, arg, live, dest(func, dflt)));
    }
    // `param_dispatch` duplicates the dispatch block per edge; on
    // tokenize's `state = if ws {0} else {1}` shape it measured a
    // ~17% regression (9.0ms vs 7.6 with it off) — the duplicated
    // param edges cost more than the removed indirection saves on
    // this corpus. Opt-in until the profitability guard is fixed.
    let pdispatch = crate::config::config().is("PLIRON_PDISPATCH", "1");
    let mut n = 0;
    let mut cfg_lazy = None;
    for (b, term, arg, live, (db, dargs)) in todo {
        let cfg = cfg_lazy.get_or_insert_with(|| ControlFlowGraph::with_function(func));
        if pdispatch && let Some(k) = param_dispatch(func, cfg, term) {
            n += k;
            // The rewrite changed edges — the cached cfg is stale.
            cfg_lazy = None;
            continue;
        }
        if let Some((c, (tb, targs), (fb, fargs))) = select_dispatch(func, cfg, term) {
            func.layout.remove_inst(term);
            let mut pos_c = FuncCursor::new(func).at_bottom(b);
            if tb == fb && targs == fargs {
                pos_c.ins().jump(tb, &targs);
            } else {
                pos_c.ins().brif(c, tb, &targs, fb, &fargs);
            }
            n += 1;
            // The rewrite changed edges — the cached cfg is stale.
            cfg_lazy = None;
            continue;
        }
        let ty = func.dfg.value_type(arg);
        func.layout.remove_inst(term);
        let mut cur = b;
        for (pos, (index, (eb, eargs))) in live.iter().enumerate() {
            // Last live test's else edge is the default; earlier tests
            // chain through a fresh compare block.
            let next = if pos + 1 == live.len() {
                None
            } else {
                let nb = func.dfg.make_block();
                func.layout.insert_block_after(nb, cur);
                Some(nb)
            };
            {
                let mut pos_c = FuncCursor::new(func).at_bottom(cur);
                let k = pos_c.ins().iconst(ty, *index as i64);
                let c = pos_c.ins().icmp(IntCC::Equal, arg, k);
                match next {
                    Some(nb) => pos_c.ins().brif(c, *eb, eargs, nb, &[]),
                    None => pos_c.ins().brif(c, *eb, eargs, db, &dargs),
                };
            }
            if let Some(nb) = next {
                cur = nb;
            }
        }
        if live.is_empty() {
            let mut pos_c = FuncCursor::new(func).at_bottom(b);
            pos_c.ins().jump(db, &dargs);
        }
        n += 1;
        // The rewrite changed edges — the cached cfg is stale.
        cfg_lazy = None;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::Function;

    fn parse(src: &str) -> Function {
        cranelift_reader::parse_functions(src).unwrap().remove(0)
    }

    /// Run `param_dispatch` on the function's (single) `br_table`.
    fn dispatch(f: &mut Function) -> Option<usize> {
        let cfg = ControlFlowGraph::with_function(f);
        let term = f
            .layout
            .blocks()
            .flat_map(|b| f.layout.block_insts(b))
            .find(|&i| matches!(f.dfg.insts[i], InstructionData::BranchTable { .. }))
            .expect("no br_table");
        param_dispatch(f, &cfg, term)
    }

    /// Two preds merge the dispatch value into `blk3(v3)`, which
    /// `br_table`s on it; `extra` is arbitrary content inside `blk3`,
    /// `tail` extra content inside `block4`.
    fn shape(extra: &str, tail: &str) -> Function {
        parse(&format!(
            "function %f(i64) {{
            block0(v0: i64):
                v1 = iconst.i64 0
                jump block3(v1)

            block1(v8: i64):
                v2 = iconst.i64 1
                jump block3(v2)

            block3(v3: i64):
                {extra}
                br_table v3, block4, [block5]

            block4:
                {tail}
                return

            block5:
                return
            }}"
        ))
    }

    #[test]
    fn refuses_side_effecting_dispatch_block() {
        // The store runs on every path through `blk` — threading preds
        // to the table targets would delete it.
        let mut f = shape("store.i64 notrap v0, v3", "");
        assert!(dispatch(&mut f).is_none(), "{}", f.display());
    }

    #[test]
    fn pure_dispatch_block_still_threads() {
        let mut f = shape("v9 = iadd.i64 v3, v3", "");
        assert!(dispatch(&mut f).is_some(), "{}", f.display());
    }

    #[test]
    fn refuses_blk_result_escape() {
        // `v9` is defined in `blk` and read by a table target — after
        // threading, `blk` dies and the use dangles.
        let mut f = shape("v9 = iadd.i64 v3, v3", "v10 = iadd.i64 v9, v9");
        assert!(dispatch(&mut f).is_none(), "{}", f.display());
    }

    #[test]
    fn refuses_entry_arg_defined_in_blk() {
        // A table entry passes `v9` (defined in `blk`) as a target arg —
        // copied verbatim onto a pred edge it would dangle.
        let mut f = parse(
            "function %f(i64) {
            block0(v0: i64):
                v1 = iconst.i64 0
                jump block3(v1)

            block1(v8: i64):
                v2 = iconst.i64 1
                jump block3(v2)

            block3(v3: i64):
                v9 = iadd.i64 v3, v3
                br_table v3, block4, [block5(v9)]

            block4:
                return

            block5(v10: i64):
                return
            }",
        );
        assert!(dispatch(&mut f).is_none(), "{}", f.display());
    }
}

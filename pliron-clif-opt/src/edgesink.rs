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

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Value, ValueDef,
};
use cranelift_codegen::loop_analysis::LoopAnalysis;
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
/// result.
fn sinkable_def(func: &Function, b: Block, v: Value) -> Option<Inst> {
    match func.dfg.value_def(v) {
        ValueDef::Result(d, _)
            if func.layout.inst_block(d) == Some(b)
                && func.dfg.inst_results(d).len() == 1
                && pure_op(func, d) =>
        {
            Some(d)
        }
        _ => None,
    }
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
fn push_to_use_blocks(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
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
            if apply(func, b, edge, &set) {
                changed += set.len();
            }
        }
    }
    changed + push_to_use_blocks(func)
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
}

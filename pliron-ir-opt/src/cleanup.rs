//! `PLIRON_INLINE_CLEAN=1`: a cheap per-function cleanup of the pliron IR,
//! run on every body before inlining and on each caller right after the
//! inliner finishes with it (bottom-up, so a callee is cleaned before it is
//! cloned into its callers). Two rewrites, both linear:
//!
//! - **DCE**: an op with results, none of them used, and no side effect
//!   (arithmetic, compares, casts, GEPs, aggregates, non-volatile loads) is
//!   erased; operands that become dead follow through a worklist. rustc's
//!   `assume(cond)` leaves its `icmp` dead (`intrinsic.rs` drops the
//!   assume), and inlining cloned every such op into every caller: 7.7k
//!   dead ops before inlining became 71k after on syn.
//! - **Block merge**: a block whose only predecessor edge is an
//!   unconditional `br` is spliced onto that predecessor (its arguments
//!   replaced by the branch operands), like LLVM's SimplifyCFG. Inlining
//!   leaves exactly this shape at every site (`br entry(args)`, `return`
//!   → `br cont(v)`), and rustc's MIR blocks are tiny: after inlining, 54%
//!   of syn's 422k blocks were single-`br`-predecessor blocks and 24% of
//!   all ops were `br`s.
//!
//! Skipped shapes: the entry block, landing pads (entered by unwinding, not
//! an edge), a block after an invoke (`call; br` must end the block), and
//! anything the frozen-region tables pin: a clone's entry (the region id)
//! or a block tagged with another region than its predecessor's.

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::{Op, OpId};
use pliron::operation::Operation;
use pliron::value::{DefiningEntity, Value};
use pliron_llvm::ops::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::has_body;
use crate::inline::{blocks, blocks_iter};
use crate::state::State;

/// Ops erasable when none of their results is used.
pub struct Pure {
    ids: FxHashSet<OpId>,
    load: OpId,
    br: OpId,
}

impl Pure {
    pub fn new() -> Self {
        let ids: FxHashSet<OpId> = [
            AddOp::get_opid_static(),
            SubOp::get_opid_static(),
            MulOp::get_opid_static(),
            ShlOp::get_opid_static(),
            LShrOp::get_opid_static(),
            AShrOp::get_opid_static(),
            AndOp::get_opid_static(),
            OrOp::get_opid_static(),
            XorOp::get_opid_static(),
            ICmpOp::get_opid_static(),
            FCmpOp::get_opid_static(),
            SelectOp::get_opid_static(),
            BitcastOp::get_opid_static(),
            ZExtOp::get_opid_static(),
            SExtOp::get_opid_static(),
            TruncOp::get_opid_static(),
            PtrToIntOp::get_opid_static(),
            IntToPtrOp::get_opid_static(),
            AddrSpaceCastOp::get_opid_static(),
            GetElementPtrOp::get_opid_static(),
            ExtractValueOp::get_opid_static(),
            InsertValueOp::get_opid_static(),
            FreezeOp::get_opid_static(),
            FAddOp::get_opid_static(),
            FSubOp::get_opid_static(),
            FMulOp::get_opid_static(),
            FDivOp::get_opid_static(),
            FNegOp::get_opid_static(),
            FPExtOp::get_opid_static(),
            FPTruncOp::get_opid_static(),
            SIToFPOp::get_opid_static(),
            UIToFPOp::get_opid_static(),
            FPToSIOp::get_opid_static(),
            FPToUIOp::get_opid_static(),
            InsertElementOp::get_opid_static(),
            ExtractElementOp::get_opid_static(),
            ShuffleVectorOp::get_opid_static(),
        ]
        .into_iter()
        .collect();
        Pure {
            ids,
            load: LoadOp::get_opid_static(),
            br: BrOp::get_opid_static(),
        }
    }

    fn erasable(&self, ctx: &Context, st: &State, op: Ptr<Operation>) -> bool {
        let id = Operation::get_opid(op, ctx);
        self.ids.contains(&id) || (id == self.load && !st.volatile.contains(op))
    }
}

impl Default for Pure {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts of what one `clean_fn` removed.
#[derive(Default, Clone, Copy)]
pub struct Cleaned {
    pub ops: usize,
    pub blocks: usize,
}

/// Erase unused side-effect-free ops of `f`, then merge single-`br`-pred
/// blocks. Does nothing under `-g` (debug info keeps block-level maps).
// r[impl iropt.cleanup]
pub fn clean_fn(ctx: &mut Context, st: &mut State, pure: &Pure, f: Ptr<Operation>) -> Cleaned {
    let mut c = Cleaned::default();
    if st.dbg.is_some() {
        return c;
    }
    c.ops = dce(ctx, st, pure, f);
    c.blocks = merge_blocks(ctx, st, pure, f);
    c
}

/// The constant bits of `v` (a `Bits` or `Zero` constant).
fn const_bits(st: &State, v: Value) -> Option<u128> {
    match st.consts.get(v)? {
        crate::state::ConstVal::Bits(b) => Some(*b),
        crate::state::ConstVal::Zero => Some(0),
        _ => None,
    }
}

/// `PLIRON_INLINE_SIMP`: a `cond_br` or `switch` on a constant becomes a
/// `br` to the taken edge, and the blocks no longer reachable go
/// (`erase_unreachable`). Returns the number of folded branches.
pub fn fold_branches(ctx: &mut Context, st: &mut State, f: Ptr<Operation>) -> usize {
    use pliron::r#type::Typed;
    let mut folds: Vec<(Ptr<Operation>, Ptr<BasicBlock>, Vec<Value>)> = vec![];
    for b in blocks(ctx, f) {
        let Some(term) = b.deref(ctx).get_terminator(ctx) else { continue };
        if let Some(cb) = Operation::get_op::<CondBrOp>(term, ctx) {
            let Some(k) = const_bits(st, term.deref(ctx).get_operand(ctx, 0)) else { continue };
            let (i, ods) = if k & 1 != 0 {
                (0, cb.get_true_dest_operands(ctx))
            } else {
                (1, cb.get_false_dest_operands(ctx))
            };
            folds.push((term, term.deref(ctx).get_successor(ctx, i), ods));
        } else if let Some(sw) = Operation::get_op::<SwitchOp>(term, ctx) {
            let c = term.deref(ctx).get_operand(ctx, 0);
            let Some(k) = const_bits(st, c) else { continue };
            let crate::types::TyK::Int(w) = crate::types::classify(ctx, c.get_type(ctx)) else { continue };
            if w == 0 || w > 128 {
                continue;
            }
            let m = if w == 128 { u128::MAX } else { (1u128 << w) - 1 };
            let cases = sw.cases(ctx);
            match cases.iter().position(|x| x.value.value().to_u128() & m == k & m) {
                Some(j) => folds.push((term, cases[j].dest, cases[j].dest_opds.clone())),
                None => folds.push((term, sw.default_dest(ctx), sw.default_dest_operands(ctx))),
            }
        }
    }
    let n = folds.len();
    for (term, dst, ods) in folds {
        let br = BrOp::new(ctx, dst, ods).get_operation();
        br.insert_before(ctx, term);
        Operation::erase(term, ctx);
    }
    if n > 0 {
        let bs = blocks(ctx, f);
        let om: FxHashMap<_, _> = bs.iter().map(|&b| (b, b.deref(ctx).iter(ctx).collect::<Vec<_>>())).collect();
        crate::helpers::erase_unreachable(ctx, st, &bs, &|b| om[&b].clone());
    }
    n
}

/// `PLIRON_INLINE_SIMP`: integer casts and compares of constants become
/// constants (instcombine covers only two- and three-operand arithmetic).
fn fold_const_ops(ctx: &mut Context, st: &mut State, f: Ptr<Operation>) -> usize {
    use crate::types::{TyK, classify};
    use pliron::r#type::Typed;
    use pliron_llvm::attributes::ICmpPredicateAttr as P;
    let width = |ctx: &Context, v: Value| match classify(ctx, v.get_type(ctx)) {
        TyK::Int(w) if (1..=128).contains(&w) => Some(w),
        _ => None,
    };
    let mask = |w: u32| if w == 128 { u128::MAX } else { (1u128 << w) - 1 };
    let sext = |b: u128, w: u32| -> i128 { ((b << (128 - w)) as i128) >> (128 - w) };
    let mut n = 0;
    for b in blocks(ctx, f) {
        let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
        for op in ops {
            if op.deref(ctx).get_num_results() != 1 {
                continue;
            }
            let r = op.deref(ctx).get_result(ctx, 0);
            let Some(rw) = width(ctx, r) else { continue };
            let args: Vec<Value> = op.deref(ctx).operands(ctx).collect();
            let k = if Operation::is_op::<ZExtOp>(op, ctx) || Operation::is_op::<TruncOp>(op, ctx) {
                let ([a], Some(w)) = (args.as_slice(), args.first().and_then(|&a| width(ctx, a))) else { continue };
                let Some(x) = const_bits(st, *a) else { continue };
                (x & mask(w)) & mask(rw)
            } else if Operation::is_op::<SExtOp>(op, ctx) {
                let ([a], Some(w)) = (args.as_slice(), args.first().and_then(|&a| width(ctx, a))) else { continue };
                let Some(x) = const_bits(st, *a) else { continue };
                (sext(x & mask(w), w) as u128) & mask(rw)
            } else if let Some(ic) = Operation::get_op::<ICmpOp>(op, ctx) {
                let ([a, c], Some(w)) = (args.as_slice(), args.first().and_then(|&a| width(ctx, a))) else { continue };
                let (Some(x), Some(y)) = (const_bits(st, *a), const_bits(st, *c)) else { continue };
                let (x, y) = (x & mask(w), y & mask(w));
                let (sx, sy) = (sext(x, w), sext(y, w));
                u128::from(match ic.predicate(ctx) {
                    P::EQ => x == y,
                    P::NE => x != y,
                    P::ULT => x < y,
                    P::ULE => x <= y,
                    P::UGT => x > y,
                    P::UGE => x >= y,
                    P::SLT => sx < sy,
                    P::SLE => sx <= sy,
                    P::SGT => sx > sy,
                    P::SGE => sx >= sy,
                })
            } else {
                continue;
            };
            let kv = crate::sroa::mk_const(ctx, st, r.get_type(ctx), crate::state::ConstVal::Bits(k));
            r.replace_all_uses_with(ctx, &kv);
            Operation::erase(op, ctx);
            n += 1;
        }
    }
    n
}

/// Whether `f`'s CFG has a cycle (a back edge in a DFS from the entry).
pub fn has_loop(ctx: &Context, f: Ptr<Operation>) -> bool {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else { return false };
    // 1 = on the DFS stack, 2 = done.
    let mut state: FxHashMap<Ptr<BasicBlock>, u8> = FxHashMap::default();
    let succs = |b: Ptr<BasicBlock>| -> Vec<Ptr<BasicBlock>> {
        b.deref(ctx).get_terminator(ctx).map(|t| t.deref(ctx).successors(ctx).collect()).unwrap_or_default()
    };
    let mut stack: Vec<(Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>)> = vec![(entry, succs(entry))];
    state.insert(entry, 1);
    while let Some((b, rest)) = stack.last_mut() {
        match rest.pop() {
            Some(s) => match state.get(&s) {
                Some(1) => return true,
                Some(_) => {}
                None => {
                    state.insert(s, 1);
                    let ss = succs(s);
                    stack.push((s, ss));
                }
            },
            None => {
                state.insert(*b, 2);
                stack.pop();
            }
        }
    }
    false
}

/// `PLIRON_INLINE_SIMP=1`: simplify a function the bottom-up inliner has
/// finished with, before its callers score it: SROA, const-global loads,
/// instcombine, constant branches, DCE and block merge. LLVM runs its
/// function simplification pipeline on each SCC before the callers' inline
/// decisions; scoring the unsimplified body charged pulldown's
/// `append_text` for all of a derived `PartialEq::eq` that folds to one
/// compare against its constant operand.
pub fn simplify_fn(
    ctx: &mut Context,
    st: &mut State,
    pure: &Pure,
    images: &mut crate::constload::ImageCache,
    f: Ptr<Operation>,
) {
    if st.dbg.is_some() {
        return;
    }
    // `PLIRON_SIMP_PARTS` (debug bisect mask, default all): 1 phisimp,
    // 2 sroa, 4 constload, 8 const casts/compares, 16 instcombine,
    // 32 constant branches, 64 DCE + block merge.
    let parts: u32 = crate::env_get("PLIRON_SIMP_PARTS").and_then(|v| v.parse().ok()).unwrap_or(127);
    if parts & 1 != 0 {
        crate::phisimp::run_fn(ctx, st, f);
    }
    if parts & 2 != 0 {
        crate::sroa::run_fn(ctx, st, f);
    }
    for _ in 0..2 {
        let mut changed = 0;
        if parts & 4 != 0 {
            changed += crate::constload::run_fn(ctx, st, f, images);
        }
        if parts & 8 != 0 {
            changed += fold_const_ops(ctx, st, f);
        }
        if parts & 16 != 0 {
            changed += crate::instcombine::run_one(ctx, st, f);
        }
        let folded = if parts & 32 != 0 { fold_branches(ctx, st, f) } else { 0 };
        changed += folded;
        if folded > 0 && parts & 1 != 0 {
            changed += crate::phisimp::run_fn(ctx, st, f);
        }
        if changed == 0 {
            break;
        }
    }
    if parts & 64 != 0 {
        clean_fn(ctx, st, pure, f);
    }
}

/// `clean_fn` over every optimizable body of the module.
pub fn clean_all(ctx: &mut Context, st: &mut State) -> Cleaned {
    let pure = Pure::new();
    let fns: Vec<Ptr<Operation>> = st
        .funcs
        .iter()
        .filter(|(n, f)| {
            has_body(ctx, f.op)
                && !st.dead_fns.contains(n.as_str())
                && !st.dead_unopt.contains(n.as_str())
                && !st.opt_skip.contains(&f.op)
        })
        .map(|(_, f)| f.op)
        .collect();
    let mut tot = Cleaned::default();
    for f in fns {
        let c = clean_fn(ctx, st, &pure, f);
        tot.ops += c.ops;
        tot.blocks += c.blocks;
    }
    if (tot.ops > 0 || tot.blocks > 0) && pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::stats", "clean {}: pre-inline erased {} ops, merged {} blocks", st.cgu, tot.ops, tot.blocks);
    }
    tot
}

fn dce(ctx: &mut Context, st: &mut State, pure: &Pure, f: Ptr<Operation>) -> usize {
    let mut work: Vec<Ptr<Operation>> = blocks_iter(ctx, f)
        .flat_map(|b| b.deref(ctx).iter(ctx))
        .collect();
    let mut n = 0;
    while let Some(op) = work.pop() {
        if !op.is_live(ctx) {
            continue;
        }
        {
            let o = op.deref(ctx);
            if o.get_num_results() == 0
                || o.results(ctx).any(|r| r.is_used(ctx))
                || o.get_parent_block().is_none()
            {
                continue;
            }
        }
        if !pure.erasable(ctx, st, op) {
            continue;
        }
        let opnds: Vec<Value> = op.deref(ctx).operands(ctx).collect();
        st.nonnull.remove(op);
        st.inbounds.remove(op);
        st.bool01.remove(op);
        st.invariant.remove(op);
        st.frozen_loads.remove(op);
        st.volatile.remove(op);
        Operation::erase(op, ctx);
        n += 1;
        for v in opnds {
            if let DefiningEntity::Op(d) = v.defining_entity()
                && d.is_live(ctx)
                && d.deref(ctx).get_parent_block().is_some()
            {
                work.push(d);
            }
        }
    }
    n
}

fn merge_blocks(ctx: &mut Context, st: &mut State, pure: &Pure, f: Ptr<Operation>) -> usize {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else {
        return 0;
    };
    // Predecessor edge counts and the one terminator feeding each block.
    let mut npred: FxHashMap<Ptr<BasicBlock>, u32> = FxHashMap::default();
    let mut pred_term: FxHashMap<Ptr<BasicBlock>, Ptr<Operation>> = FxHashMap::default();
    for &b in &bs {
        let Some(t) = b.deref(ctx).get_tail() else {
            continue;
        };
        for s in t.deref(ctx).successors(ctx) {
            *npred.entry(s).or_default() += 1;
            pred_term.insert(s, t);
        }
    }
    let pads: FxHashSet<Ptr<BasicBlock>> = st.invokes.iter().map(|(_, &(lp, _))| lp).collect();
    let pinned_val = |st: &State, v: Value| {
        st.region_frozen.contains(v)
            || st.region_noalias.contains(v)
            || st.frozen.contains(v)
            || st.noalias.contains(v)
            || st.nnargs.contains(v)
            || st.promoted.contains(v)
            || st.allocas.contains(v)
    };
    let scoped_noalias = crate::env_is("PLIRON_SCOPED_NOALIAS", "1");
    let mut n = 0;
    for &b in &bs {
        if b == entry || !b.is_live(ctx) || pads.contains(&b) {
            continue;
        }
        if npred.get(&b) != Some(&1) {
            continue;
        }
        let t = pred_term[&b];
        if Operation::get_opid(t, ctx) != pure.br {
            continue;
        }
        let Some(a) = t.deref(ctx).get_parent_block() else {
            continue;
        };
        if a == b {
            continue;
        }
        // `call; br` of an invoke must stay a block end.
        if let Some(prev) = t.get_prev(ctx)
            && st.invokes.contains(prev)
        {
            continue;
        }
        // Frozen regions: a clone entry is a region id (`region_parent`
        // key, `frozen_loads`/`region_frozen` target), so it stays. Any
        // other block may join its predecessor and take its region: the
        // merged block is never a loop preheader for a loop holding the
        // clone's tagged loads (a clone is entered only through its entry,
        // from the callsite block), so licm's in-region test can't newly
        // pass; where `b` was the deeper region the test only gets stricter.
        if st.region_parent.contains(b) {
            continue;
        }
        // `PLIRON_SCOPED_NOALIAS=1`: a region tag also scopes a `noalias`
        // fact loadfwd uses against stores and calls, so a block must not
        // take a region it was not cloned into (the caller's continuation
        // joining the clone's last block would run "inside" the callee).
        if scoped_noalias && st.region_blocks.get(b) != st.region_blocks.get(a) {
            continue;
        }
        let args: Vec<Value> = b.deref(ctx).arguments(ctx).collect();
        let opnds: Vec<Value> = t.deref(ctx).operands(ctx).collect();
        if args.len() != opnds.len() {
            continue;
        }
        if args.iter().any(|&v| pinned_val(st, v)) {
            continue;
        }
        // An operand defined by `b` itself (an unreachable self-loop).
        if opnds
            .iter()
            .any(|v| matches!(v.defining_entity(), DefiningEntity::Block(d) if d == b))
        {
            continue;
        }
        st.prof_edges.remove(t);
        Operation::erase(t, ctx);
        for (arg, v) in args.iter().zip(&opnds) {
            arg.replace_all_uses_with(ctx, v);
        }
        let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
        for op in ops {
            op.unlink(ctx);
            op.insert_at_back(a, ctx);
        }
        st.region_blocks.remove(b);
        st.prof_bcount.remove(b);
        BasicBlock::erase(b, ctx);
        n += 1;
    }
    n
}

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
pub fn clean_fn(ctx: &mut Context, st: &mut State, pure: &Pure, f: Ptr<Operation>) -> Cleaned {
    let mut c = Cleaned::default();
    if st.dbg.is_some() {
        return c;
    }
    c.ops = dce(ctx, st, pure, f);
    c.blocks = merge_blocks(ctx, st, pure, f);
    c
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

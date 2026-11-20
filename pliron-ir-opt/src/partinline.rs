//! Partial inlining of a callee's entry guard (`PLIRON_PARTINLINE=1`,
//! opt-in), LLVM's `PartialInlining` for the shape enum drop glue has:
//!
//! ```text
//! drop_glue(p):  tag = load p; c = icmp eq tag, 2; cond_br c, work, ret
//!          ret:  return
//! ```
//!
//! A call the inliner kept (the whole body, with the variant that owns a
//! `Box<dyn ..>` dropped inline, is over its threshold) becomes
//!
//! ```text
//! tag' = load arg; c' = icmp eq tag', 2; cond_br c', call, cont
//! call: drop_glue(arg); br cont
//! ```
//!
//! The callee still tests its own guard, so this is exact. What it buys:
//! the caller now sees the test, and the passes after it fold it where the
//! caller knows the tag (a dominating compare or store of the variant), so
//! the call and the stack round trip around it leave the hot path.
//! lol_html's `finish_tag_name` called `drop_glue::<TreeBuilderFeedback>`
//! on every call (21,082 times) for a variant known to own nothing, and the
//! opaque call kept the `Option` result it had just stored on the stack
//! from forwarding.
//!
//! Guards taken: the callee's entry block is only constants, `gep`s,
//! non-volatile `load`s, `zext`/`trunc` and `icmp`s, ending in a
//! `cond_br` with no block arguments whose one side is a block holding only
//! a bare `return`. Only void callees, direct calls, not recursive. Linear:
//! one scan for guards, one over call sites.

use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::CallOpInterface;
use pliron::context::{Context, Ptr};
use pliron::irbuild::cloning::{IrMapping, clone_operation};
use pliron::irbuild::inserter::OpInsertionPoint;
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::{IRRewriter, Rewriter};
use pliron::linked_list::ContainsLinkedList;
use pliron::location::Located;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::ops::*;
use rustc_hash::FxHashMap;

use crate::inline::{blocks, direct_callee, ops};
use crate::state::State;

/// Guard ops copied per site (the entry block before its `cond_br`).
const MAX_GUARD_OPS: usize = 8;
/// Call sites rewritten per CGU.
const MAX_SITES: usize = 4096;

/// A callee's entry guard.
struct Guard {
    /// The entry block's ops before the `cond_br`, in order.
    ops: Vec<Ptr<Operation>>,
    /// The entry block's arguments (the parameters).
    params: Vec<Value>,
    /// The `cond_br`'s condition.
    cond: Value,
    /// The callee returns when the condition is true (else when false).
    ret_on_true: bool,
}

fn bare_return(ctx: &Context, b: Ptr<BasicBlock>) -> bool {
    if b.deref(ctx).get_num_arguments() != 0 {
        return false;
    }
    let os = ops(ctx, b);
    os.len() == 1
        && Operation::get_op::<ReturnOp>(os[0], ctx).is_some()
        && os[0].deref(ctx).get_num_operands() == 0
}

fn guard_of(ctx: &Context, st: &State, f: Ptr<Operation>) -> Option<Guard> {
    let bs = blocks(ctx, f);
    let entry = *bs.first()?;
    let os = ops(ctx, entry);
    let (&term, body) = os.split_last()?;
    if body.len() > MAX_GUARD_OPS {
        return None;
    }
    let cbr = Operation::get_op::<CondBrOp>(term, ctx)?;
    let succs: Vec<Ptr<BasicBlock>> = term.deref(ctx).successors(ctx).collect();
    if succs.len() != 2 || succs[0] == succs[1] || term.deref(ctx).get_num_operands() != 1 {
        return None;
    }
    let ret_on_true = match (bare_return(ctx, succs[0]), bare_return(ctx, succs[1])) {
        (true, false) => true,
        (false, true) => false,
        _ => return None,
    };
    for &o in body {
        let pure = Operation::get_op::<GetElementPtrOp>(o, ctx).is_some()
            || Operation::get_op::<ICmpOp>(o, ctx).is_some()
            || Operation::get_op::<ZExtOp>(o, ctx).is_some()
            || Operation::get_op::<TruncOp>(o, ctx).is_some()
            || (Operation::get_op::<LoadOp>(o, ctx).is_some() && !st.volatile.contains(o));
        if !pure || o.deref(ctx).num_regions() != 0 {
            return None;
        }
    }
    let n = entry.deref(ctx).get_num_arguments();
    let params: Vec<Value> = (0..n).map(|i| entry.deref(ctx).get_argument(ctx, i)).collect();
    // Every operand is a parameter, a constant or an earlier guard op's
    // result: the clone can bind each of them at the call site.
    let mut known: rustc_hash::FxHashSet<Value> = params.iter().copied().collect();
    for &o in body {
        if o.deref(ctx).operands(ctx).any(|v| !known.contains(&v) && st.consts.get(v).is_none()) {
            return None;
        }
        known.extend(o.deref(ctx).results(ctx));
    }
    let _ = cbr;
    let cond = term.deref(ctx).get_operand(ctx, 0);
    if !known.contains(&cond) {
        return None;
    }
    Some(Guard { ops: body.to_vec(), params, cond, ret_on_true })
}

pub fn run(ctx: &mut Context, st: &mut State) -> usize {
    // Guards of the callees that have one.
    let mut guards: FxHashMap<String, Guard> = FxHashMap::default();
    let fns: Vec<(String, Ptr<Operation>)> = st
        .funcs
        .iter()
        .filter(|(_, fi)| crate::opt_body(ctx, st, fi.op))
        .map(|(s, fi)| (s.clone(), fi.op))
        .collect();
    let tron = pliron_cfg::tracing_on();
    for (s, f) in &fns {
        if let Some(g) = guard_of(ctx, st, *f) {
            if tron {
                tracing::debug!(target: "pliron::partinline", "guard {s}");
            }
            guards.insert(s.clone(), g);
        } else if tron && crate::inline::is_drop_glue_sym(s) {
            let entry_ops: Vec<String> = blocks(ctx, *f)
                .first()
                .map(|&b| ops(ctx, b).iter().map(|&o| Operation::get_opid(o, ctx).to_string()).collect())
                .unwrap_or_default();
            tracing::debug!(target: "pliron::partinline", "no guard {s}: entry {entry_ops:?}");
        }
    }
    if tron {
        tracing::debug!(target: "pliron::partinline", "{} functions, {} guards", fns.len(), guards.len());
    }
    if guards.is_empty() {
        return 0;
    }
    // Sites: direct void calls to a guarded callee from another function.
    let mut sites: Vec<(Ptr<Operation>, String)> = Vec::new();
    'f: for (caller, f) in &fns {
        for b in blocks(ctx, *f) {
            for op in ops(ctx, b) {
                // Void calls: one result, unused (calls carry a result even
                // for a callee returning nothing).
                if Operation::get_op::<CallOp>(op, ctx).is_none()
                    || op.deref(ctx).get_num_results() > 1
                    || (op.deref(ctx).get_num_results() == 1 && op.deref(ctx).get_result(ctx, 0).is_used(ctx))
                {
                    continue;
                }
                let Some(cs) = direct_callee(ctx, st, op) else { continue };
                if cs == caller || !guards.contains_key(cs) {
                    continue;
                }
                sites.push((op, cs.clone()));
                if sites.len() >= MAX_SITES {
                    break 'f;
                }
            }
        }
    }
    let expect = !crate::env_is("PLIRON_PARTINLINE_EXPECT", "0");
    let mut rw = IRRewriter::<DummyListener>::default();
    let mut n = 0;
    for (call, cs) in sites {
        let g = &guards[&cs];
        let args = Operation::get_op::<CallOp>(call, ctx).unwrap().args(ctx);
        if args.len() != g.params.len() || !pliron_cfg::bisect("partinline") {
            continue;
        }
        let block = call.deref(ctx).get_parent_block().unwrap();
        let region = block.deref(ctx).get_parent_region().unwrap();
        let loc = call.deref(ctx).loc();
        // The guard, cloned in front of the call with params bound to args.
        // Constants are side-table values: give the caller its own.
        let mut map = IrMapping::new();
        for (&p, &a) in g.params.iter().zip(&args) {
            map.map_value(p, a);
        }
        for &o in &g.ops {
            let operands: Vec<Value> = o.deref(ctx).operands(ctx).collect();
            for v in operands {
                if let Some(cv) = st.consts.get(v).cloned()
                    && map.lookup_value(v).is_none()
                {
                    let nv = crate::sroa::mk_const(ctx, st, v.get_type(ctx), cv);
                    map.map_value(v, nv);
                }
            }
            let no = clone_operation(o, ctx, &mut rw, &mut map);
            no.insert_before(ctx, call);
        }
        let Some(cond) = map.lookup_value(g.cond) else { continue };
        // Split: `block` ends in the guard's branch, the call sits in its own
        // block, everything after it continues in `cont`.
        let cont = rw.split_block(ctx, block, OpInsertionPoint::AfterOperation(call), None);
        let callb = BasicBlock::new(ctx, None, vec![]);
        callb.insert_at_back(region, ctx);
        call.unlink(ctx);
        call.insert_at_back(callb, ctx);
        let br = BrOp::new(ctx, cont, vec![]).get_operation();
        br.deref_mut(ctx).set_loc(loc.clone());
        br.insert_at_back(callb, ctx);
        let (t, f) = if g.ret_on_true { (cont, callb) } else { (callb, cont) };
        let cbr = CondBrOp::new(ctx, cond, t, vec![], f, vec![]).get_operation();
        cbr.deref_mut(ctx).set_loc(loc);
        cbr.insert_at_back(block, ctx);
        // Expect the side where the callee would return at once: the
        // inliner kept the call because the rest is big, and skipping is
        // what the copy is for. Without it layout put the skip out of line
        // (html: +32k executed jumps, one per `finish_tag_name` call).
        // `PLIRON_PARTINLINE_EXPECT=0`: no expectation.
        if expect {
            st.expect.insert(cbr, g.ret_on_true);
        }
        if let Some(c) = st.prof_bcount.get(block).copied() {
            st.prof_bcount.insert(cont, c);
        }
        n += 1;
        if tron {
            let caller = region.deref(ctx).get_parent_op();
            let cname = st.funcs.iter().find(|(_, fi)| fi.op == caller).map(|(s, _)| s.as_str()).unwrap_or("?");
            tracing::debug!(target: "pliron::partinline", "guard of {cs} copied into {cname}");
        }
    }
    n
}

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
//! non-volatile `load`s, `zext`/`trunc` and `icmp`s (with
//! `PLIRON_PARTINLINE_WIDE=1` also `select`, `sext` and integer
//! add/sub/and/or/xor), ending in a
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

/// One step of a guard, in execution order.
enum Step {
    /// A pure op, cloned.
    Op(Ptr<Operation>),
    /// A jump binds block argument `.0` to value `.1`.
    Bind(Value, Value),
}

/// A callee's entry guard.
struct Guard {
    /// The ops before the `cond_br` (and the jumps' bindings), in order.
    steps: Vec<Step>,
    /// The entry block's arguments (the parameters).
    params: Vec<Value>,
    /// The `cond_br`'s condition.
    cond: Value,
    /// The callee returns when the condition is true (else when false).
    ret_on_true: bool,
}

fn bare_return(ctx: &Context, st: &State, b: Ptr<BasicBlock>, wide: bool) -> bool {
    if !wide {
        if b.deref(ctx).get_num_arguments() != 0 {
            return false;
        }
        let os = ops(ctx, b);
        return os.len() == 1
            && Operation::get_op::<ReturnOp>(os[0], ctx).is_some()
            && os[0].deref(ctx).get_num_operands() == 0;
    }
    // `wide`: lifetime markers and jumps on the way to a void `return`.
    let mut b = b;
    for _ in 0..8 {
        let os = ops(ctx, b);
        let Some((&t, rest)) = os.split_last() else { return false };
        if !rest.iter().all(|&o| skippable(ctx, st, o)) {
            return false;
        }
        if Operation::get_op::<ReturnOp>(t, ctx).is_some() {
            return t.deref(ctx).get_num_operands() == 0;
        }
        if Operation::get_op::<BrOp>(t, ctx).is_none() {
            return false;
        }
        let Some(n) = t.deref(ctx).successors(ctx).next() else { return false };
        if n == b {
            return false;
        }
        b = n;
    }
    false
}

/// Ops a `wide` guard steps over without copying: the callee's allocas
/// and their lifetime markers.
fn skippable(ctx: &Context, st: &State, o: Ptr<Operation>) -> bool {
    Operation::get_op::<AllocaOp>(o, ctx).is_some()
        || matches!(st.intrinsics.get(o).map(|s| s.as_str()), Some("llvm.lifetime.start" | "llvm.lifetime.end"))
}

fn pure_guard_op(ctx: &Context, st: &State, o: Ptr<Operation>, wide: bool) -> bool {
    let pure = Operation::get_op::<GetElementPtrOp>(o, ctx).is_some()
        || Operation::get_op::<ICmpOp>(o, ctx).is_some()
        || Operation::get_op::<ZExtOp>(o, ctx).is_some()
        || Operation::get_op::<TruncOp>(o, ctx).is_some()
        || (wide
            && (Operation::get_op::<SelectOp>(o, ctx).is_some()
                || Operation::get_op::<SExtOp>(o, ctx).is_some()
                || Operation::get_op::<AddOp>(o, ctx).is_some()
                || Operation::get_op::<SubOp>(o, ctx).is_some()
                || Operation::get_op::<AndOp>(o, ctx).is_some()
                || Operation::get_op::<OrOp>(o, ctx).is_some()
                || Operation::get_op::<XorOp>(o, ctx).is_some()))
        || (Operation::get_op::<LoadOp>(o, ctx).is_some() && !st.volatile.contains(o));
    pure && o.deref(ctx).num_regions() == 0
}

/// `PLIRON_TRACE=pliron::partinline=trace`: which rule refused a guard.
fn nog(why: u32) -> Option<Guard> {
    if pliron_cfg::tracing_on() {
        tracing::trace!(target: "pliron::partinline", "no guard: rule {why}");
    }
    None
}

fn guard_of(ctx: &Context, st: &State, f: Ptr<Operation>, wide: bool) -> Option<Guard> {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else { return None };
    let n = entry.deref(ctx).get_num_arguments();
    let params: Vec<Value> = (0..n).map(|i| entry.deref(ctx).get_argument(ctx, i)).collect();
    // `wide`: the guard may run on through `br`s, each into a block whose
    // only way in it is (inlining leaves a body's real entry a jump or
    // two in, its params passed as block arguments), with pure ops and
    // allocas (not copied) before each jump.
    let mut steps: Vec<Step> = Vec::new();
    let mut nops = 0usize;
    let mut b = entry;
    let term = loop {
        let os = ops(ctx, b);
        let Some((&t, rest)) = os.split_last() else { return nog(100) };
        for &o in rest {
            if wide && skippable(ctx, st, o) {
                continue;
            }
            if !pure_guard_op(ctx, st, o, wide) {
                return nog(1);
            }
            nops += 1;
            steps.push(Step::Op(o));
        }
        if nops > MAX_GUARD_OPS {
            return nog(2);
        }
        if Operation::get_op::<CondBrOp>(t, ctx).is_some() {
            break t;
        }
        if !wide || Operation::get_op::<BrOp>(t, ctx).is_none() || steps.len() > 4 * MAX_GUARD_OPS {
            return nog(3);
        }
        let Some(nb) = t.deref(ctx).successors(ctx).next() else { return nog(101) };
        let na = nb.deref(ctx).get_num_arguments();
        if nb == b || nb == entry || t.deref(ctx).get_num_operands() != na || nb.preds(ctx).len() != 1 {
            return nog(4);
        }
        for i in 0..na {
            steps.push(Step::Bind(nb.deref(ctx).get_argument(ctx, i), t.deref(ctx).get_operand(ctx, i)));
        }
        b = nb;
    };
    let succs: Vec<Ptr<BasicBlock>> = term.deref(ctx).successors(ctx).collect();
    if succs.len() != 2 || succs[0] == succs[1] || term.deref(ctx).get_num_operands() != 1 {
        return nog(5);
    }
    let ret_on_true = match (bare_return(ctx, st, succs[0], wide), bare_return(ctx, st, succs[1], wide)) {
        (true, false) => true,
        (false, true) => false,
        _ => return nog(6),
    };
    // Every operand is a parameter, a constant or an earlier step's
    // result: the clone can bind each of them at the call site.
    let mut known: rustc_hash::FxHashSet<Value> = params.iter().copied().collect();
    let ok = |v: &Value, known: &rustc_hash::FxHashSet<Value>| known.contains(v) || st.consts.get(*v).is_some();
    for step in &steps {
        match *step {
            Step::Op(o) => {
                if !o.deref(ctx).operands(ctx).all(|v| ok(&v, &known)) {
                    return nog(7);
                }
                known.extend(o.deref(ctx).results(ctx));
            }
            Step::Bind(a, v) => {
                if !ok(&v, &known) {
                    return nog(8);
                }
                known.insert(a);
            }
        }
    }
    let cond = term.deref(ctx).get_operand(ctx, 0);
    if !known.contains(&cond) {
        return nog(9);
    }
    Some(Guard { steps, params, cond, ret_on_true })
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
    // `PLIRON_PARTINLINE_WIDE=1`: drop glue guards may also `select`,
    // `sext` and add/sub/and/or/xor (a niche decoded before the compare:
    // `Cow`'s drop glue is `tag = cap == MIN ? 1 : 0; if tag != 0 {
    // return }`), and run on through jumps, allocas and lifetime markers.
    let wide = crate::env_is("PLIRON_PARTINLINE_WIDE", "1");
    for (s, f) in &fns {
        // Wide shapes on drop glue only: there the guard usually skips the
        // call. On a plain function's fast-path test (serde_json's
        // `parse_str`, pulldown-cmark's byte scanner) the copy mostly
        // falls through to a call that tests again: json +0.23%.
        let wide = wide && crate::inline::is_drop_glue_sym(s);
        if let Some(g) = guard_of(ctx, st, *f, wide) {
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
        for step in &g.steps {
            match *step {
                Step::Bind(a, v) => {
                    let nv = match map.lookup_value(v) {
                        Some(nv) => nv,
                        None => {
                            let cv = st.consts.get(v).cloned().expect("guard_of checked the binding");
                            crate::sroa::mk_const(ctx, st, v.get_type(ctx), cv)
                        }
                    };
                    map.map_value(a, nv);
                }
                Step::Op(o) => {
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
            }
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

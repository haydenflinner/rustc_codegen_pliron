//! Pliron-level inliner. Cranelift does no inlining, so small callees defined
//! in the same CGU are copied into their call sites before lowering. Callees are
//! processed bottom-up, so inlined bodies are inlined transitively; at invoke sites
//! the callee's calls become invokes to the same landing pad.
//! `PLIRON_INLINE=<max ops>` tunes the size limit; `0` disables it.

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::irbuild::cloning::{IrMapping, clone_blocks_into};
use pliron::irbuild::inserter::OpInsertionPoint;
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::{IRRewriter, Rewriter};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron_llvm::ops::{BrOp, CallOp, ReturnOp};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::context::State;
use crate::lower::has_body;

const DEFAULT_LIMIT: usize = 320;
const SMALL_LIMIT: usize = 40;

pub(crate) fn blocks(ctx: &Context, f: Ptr<Operation>) -> Vec<Ptr<BasicBlock>> {
    f.deref(ctx).get_region(0).deref(ctx).iter(ctx).collect()
}

pub(crate) fn ops(ctx: &Context, b: Ptr<BasicBlock>) -> Vec<Ptr<Operation>> {
    b.deref(ctx).iter(ctx).collect()
}

pub(crate) fn direct_callee<'a>(
    ctx: &Context,
    st: &'a State<'_>,
    op: Ptr<Operation>,
) -> Option<&'a String> {
    let c = Operation::get_op::<CallOp>(op, ctx)?;
    let CallOpCallable::Direct(id) = c.callee(ctx) else {
        return None;
    };
    st.ident_to_sym.get(&id.to_string())
}

/// `Some(has_eh)` if `sym` may be inlined; `has_eh` = it has invokes or reads
/// the exception pointer. `PLIRON_INLINE_EH=0` rejects those callees outright.
fn eligible(ctx: &Context, st: &State<'_>, sym: &str, limit: usize, once: bool) -> Option<bool> {
    let f = &st.funcs[sym];
    if f.no_inline || f.linkage == Linkage::Preemptible || !has_body(ctx, f.op) {
        return None;
    }
    let (mut n, mut eh) = (0, false);
    for b in blocks(ctx, f.op) {
        for op in ops(ctx, b) {
            n += 1;
            let intr = st.intrinsics.get(&op);
            if intr.is_some_and(|s| s.starts_with("llvm.va_"))
                || direct_callee(ctx, st, op).is_some_and(|c| c == sym)
            {
                return None;
            }
            eh |= st.invokes.contains_key(&op) || intr.is_some_and(|s| s.starts_with("pliron.eh"));
        }
    }
    if eh && !crate::pass_enabled("PLIRON_INLINE_EH") {
        return None;
    }
    (n <= if f.always_inline || once {
        limit * 10
    } else {
        limit
    })
    .then_some(eh)
}

fn call_counts(ctx: &Context, st: &State<'_>) -> FxHashMap<String, usize> {
    let mut n: FxHashMap<String, usize> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op) || st.dead_fns.contains(sym) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
                if let Some(c) = direct_callee(ctx, st, op) {
                    *n.entry(c.clone()).or_default() += 1;
                }
            }
        }
    }
    n
}

/// Marks local fns that nothing references any more (typically fully inlined)
/// so lowering skips them. Iterates because a dead fn's calls don't count.
/// `PLIRON_DEADFN=0` disables it.
pub fn dead_fns(ctx: &Context, st: &mut State<'_>) {
    let taken = crate::lower::address_taken(ctx, st);
    loop {
        let counts = call_counts(ctx, st);
        let dead: Vec<String> = st
            .funcs
            .iter()
            .filter(|(n, f)| {
                f.linkage == Linkage::Local
                    && has_body(ctx, f.op)
                    && !st.dead_fns.contains(n.as_str())
                    && !taken.contains(n.as_str())
                    && !counts.contains_key(n.as_str())
            })
            .map(|(n, _)| n.clone())
            .collect();
        if dead.is_empty() {
            break;
        }
        st.dead_fns.extend(dead);
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!(
            "deadfn {}: {} of {} fns not lowered",
            st.cgu,
            st.dead_fns.len(),
            st.funcs.len()
        );
    }
}

/// Functions with bodies, callees before callers (cycles broken arbitrarily).
fn post_order(ctx: &Context, st: &State<'_>) -> Vec<String> {
    let mut seen = rustc_data_structures::fx::FxHashSet::default();
    let mut out = Vec::new();
    for root in st.funcs.keys() {
        if !has_body(ctx, st.funcs[root].op) || !seen.insert(root.clone()) {
            continue;
        }
        let mut stack = vec![(root.clone(), callees(ctx, st, root), 0usize)];
        while let Some((sym, cs, i)) = stack.last_mut() {
            if let Some(c) = cs.get(*i).cloned() {
                *i += 1;
                if has_body(ctx, st.funcs[&c].op) && seen.insert(c.clone()) {
                    let cc = callees(ctx, st, &c);
                    stack.push((c, cc, 0));
                }
            } else {
                out.push(sym.clone());
                stack.pop();
            }
        }
    }
    out
}

fn callees(ctx: &Context, st: &State<'_>, sym: &str) -> Vec<String> {
    blocks(ctx, st.funcs[sym].op)
        .into_iter()
        .flat_map(|b| ops(ctx, b))
        .filter_map(|op| direct_callee(ctx, st, op).cloned())
        .filter(|c| st.funcs.contains_key(c))
        .collect()
}

/// `small`: size-sensitive output (wasm, -Copt-level=s/z) keeps the old 40-op limit and no
/// single-caller inlining; rustc.wasm tripled in code size (past V8's 1 GB module cap) without it.
pub fn run(ctx: &mut Context, st: &mut State<'_>, small: bool) {
    let limit = std::env::var("PLIRON_INLINE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { SMALL_LIMIT } else { DEFAULT_LIMIT });
    if limit == 0 {
        return;
    }
    // Local fns with one direct call site and no address use get 10x the limit:
    // inlining them removes the out-of-line copy (`PLIRON_INLINE_ONCE=0` disables).
    let once = std::env::var("PLIRON_INLINE_ONCE").map_or(!small, |v| v != "0");
    let single: FxHashSet<String> = if once {
        let taken = crate::lower::address_taken(ctx, st);
        let counts = call_counts(ctx, st);
        st.funcs
            .iter()
            .filter(|(n, f)| {
                f.linkage == Linkage::Local
                    && !taken.contains(n.as_str())
                    && counts.get(n.as_str()) == Some(&1)
            })
            .map(|(n, _)| n.clone())
            .collect()
    } else {
        FxHashSet::default()
    };
    // PLIRON_INLINE_BU=0: one flat round, non-invoke sites only (the old heuristic).
    let bottom_up = crate::pass_enabled("PLIRON_INLINE_BU");
    let mut ok: FxHashMap<String, (Ptr<Operation>, bool)> = FxHashMap::default();
    let order: Vec<String> = if bottom_up {
        post_order(ctx, st)
    } else {
        for sym in st.funcs.keys() {
            if let Some(eh) = eligible(ctx, st, sym, limit, single.contains(sym)) {
                ok.insert(sym.clone(), (st.funcs[sym].op, eh));
            }
        }
        st.funcs
            .iter()
            .filter(|(_, f)| has_body(ctx, f.op))
            .map(|(s, _)| s.clone())
            .collect()
    };
    let mut rw = IRRewriter::<DummyListener>::default();
    for sym in order {
        let caller = st.funcs[&sym].op;
        let mut sites = Vec::new();
        for b in blocks(ctx, caller) {
            for op in ops(ctx, b) {
                if !bottom_up && st.invokes.contains_key(&op) {
                    continue;
                }
                let Some(cs) = direct_callee(ctx, st, op) else {
                    continue;
                };
                let Some(&(callee, eh)) = ok.get(cs) else {
                    continue;
                };
                // A callee with its own landing pads only goes into plain call sites.
                if eh && st.invokes.contains_key(&op) {
                    continue;
                }
                if callee != caller && st.calls[&op].fn_ty == st.funcs[cs].ty {
                    sites.push((op, callee));
                }
            }
        }
        for (call, callee) in sites {
            inline_call(ctx, st, &mut rw, call, callee);
        }
        if bottom_up {
            if let Some(eh) = eligible(ctx, st, &sym, limit, single.contains(&sym)) {
                ok.insert(sym.clone(), (caller, eh));
            }
        }
    }
}

fn inline_call(
    ctx: &mut Context,
    st: &mut State<'_>,
    rw: &mut IRRewriter<DummyListener>,
    call: Ptr<Operation>,
    callee: Ptr<Operation>,
) {
    let block = call.deref(ctx).get_parent_block().unwrap();
    let region = block.deref(ctx).get_parent_region().unwrap();
    let args = Operation::get_op::<CallOp>(call, ctx).unwrap().args(ctx);
    let res = (call.deref(ctx).get_num_results() > 0).then(|| call.deref(ctx).get_result(0));
    let unwind = st.invokes.remove(&call);
    let cont = rw.split_block(ctx, block, OpInsertionPoint::AfterOperation(call), None);

    let src = blocks(ctx, callee);
    let mut map = IrMapping::new();
    clone_blocks_into(&src, region, ctx, rw, &mut map);
    for &b in &src {
        let nb = map.lookup_block(b).unwrap();
        for (op, new) in ops(ctx, b).into_iter().zip(ops(ctx, nb)) {
            if let Some(c) = st.calls.get(&op).cloned() {
                st.calls.insert(new, c);
                if let Some(&(lp, catch)) = st.invokes.get(&op) {
                    st.invokes
                        .insert(new, (map.lookup_block(lp).unwrap(), catch));
                } else if let Some(u) = unwind {
                    st.invokes.insert(new, u);
                }
            }
            if let Some(s) = st.intrinsics.get(&op).cloned() {
                st.intrinsics.insert(new, s);
            }
            if let Some(r) = st.rmw.get(&op).copied() {
                st.rmw.insert(new, r);
            }
            if let Some(e) = st.expect.get(&op).copied() {
                st.expect.insert(new, e);
            }
            if st.volatile.contains(&op) {
                st.volatile.insert(new);
            }
            let n = op.deref(ctx).get_num_results();
            for i in 0..n {
                let (v, nv) = (op.deref(ctx).get_result(i), new.deref(ctx).get_result(i));
                if let Some(a) = st.allocas.get(&v).copied() {
                    st.allocas.insert(nv, a);
                }
            }
            if let Some(r) = Operation::get_op::<ReturnOp>(new, ctx) {
                let rv: Vec<_> = r.retval(ctx).into_iter().collect();
                let br = BrOp::new(ctx, cont, rv).get_operation();
                br.insert_before(ctx, new);
                Operation::erase(new, ctx);
            }
        }
    }
    if let Some(res) = res {
        let ty = res.get_type(ctx);
        BasicBlock::push_argument(cont, ctx, ty);
        let arg = cont.deref(ctx).get_argument(0);
        res.replace_all_uses_with(ctx, &arg);
    }
    let entry = map.lookup_block(src[0]).unwrap();
    let br = BrOp::new(ctx, entry, args).get_operation();
    br.insert_before(ctx, call);
    st.calls.remove(&call);
    Operation::erase(call, ctx);
}

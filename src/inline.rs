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
use pliron::r#type::{Typed, TypedHandle};
use pliron::value::Value;
use pliron_llvm::ops::{BrOp, CallOp, GetElementPtrOp, LoadOp, ReturnOp};
use pliron_llvm::types::FuncType;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::context::{ConstVal, State};
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
fn eligible(
    ctx: &Context,
    st: &State<'_>,
    sym: &str,
    limit: usize,
    once: bool,
) -> Option<(bool, usize)> {
    let f = &st.funcs[sym];
    if f.no_inline || f.linkage == Linkage::Preemptible || !has_body(ctx, f.op) {
        return None;
    }
    let (mut n, mut eh) = (0, false);
    for b in blocks(ctx, f.op) {
        for op in ops(ctx, b) {
            n += 1;
            let intr = st.intrinsics.get(&op);
            if intr.is_some_and(|s| s.starts_with("llvm.va_") || s.starts_with("pliron.va."))
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
    .then_some((eh, n))
}

/// Parameter indices of `f` that (through loads / GEPs) feed an indirect call's
/// target: a constant address there (a vtable, a fn pointer) lets `devirt`
/// make the call direct once `f` is inlined, as LLVM's inline-cost bonus does.
fn devirt_params(ctx: &Context, f: Ptr<Operation>) -> Vec<usize> {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else {
        return Vec::new();
    };
    let params: Vec<Value> = entry.deref(ctx).arguments().collect();
    let mut out = Vec::new();
    for &b in &bs {
        for op in ops(ctx, b) {
            let Some(c) = Operation::get_op::<CallOp>(op, ctx) else {
                continue;
            };
            let CallOpCallable::Indirect(mut p) = c.callee(ctx) else {
                continue;
            };
            for _ in 0..4 {
                if let Some(i) = params.iter().position(|&a| a == p) {
                    if !out.contains(&i) {
                        out.push(i);
                    }
                    break;
                }
                match p.defining_op() {
                    Some(d)
                        if Operation::is_op::<LoadOp>(d, ctx)
                            || Operation::is_op::<GetElementPtrOp>(d, ctx) =>
                    {
                        p = d.deref(ctx).get_operand(0);
                    }
                    _ => break,
                }
            }
        }
    }
    out
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
pub fn run(
    ctx: &mut Context,
    st: &mut State<'_>,
    small: bool,
    only: Option<&FxHashSet<Ptr<Operation>>>,
    always_only: bool,
) {
    let limit = std::env::var("PLIRON_INLINE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { SMALL_LIMIT } else { DEFAULT_LIMIT });
    // -O0 still honours `#[inline(always)]` like LLVM's AlwaysInliner: wasm-bindgen
    // needs its describe markers inlined into each monomorphised shim.
    let limit = if always_only { usize::MAX / 16 } else { limit };
    if limit == 0 {
        return;
    }
    // Local fns with one direct call site and no address use get 10x the limit:
    // inlining them removes the out-of-line copy (`PLIRON_INLINE_ONCE=0` disables).
    let once = !always_only && std::env::var("PLIRON_INLINE_ONCE").map_or(!small, |v| v != "0");
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
    let bottom_up = !always_only && crate::pass_enabled("PLIRON_INLINE_BU");
    // EH-invoke inlining grows code ~14% (regex-syntax), so size-sensitive output skips it.
    let eh_invoke = std::env::var("PLIRON_INLINE_EH_INVOKE").map_or(!small, |v| v != "0");
    let cap: usize = std::env::var("PLIRON_INLINE_CALLER_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    // Callees up to `bonus`x the limit whose indirect call target comes from a
    // parameter are inlined where that argument is a constant address
    // (`PLIRON_INLINE_DEVIRT=<x>`; e.g. hashbrown's `find_or_find_insert_index_inner`
    // taking `&mut dyn FnMut`).
    let bonus: usize = std::env::var("PLIRON_INLINE_DEVIRT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { 0 } else { 4 });
    let mut ok: FxHashMap<String, (Ptr<Operation>, bool, usize)> = FxHashMap::default();
    let mut dv: FxHashMap<String, (Ptr<Operation>, bool, usize, Vec<usize>)> = FxHashMap::default();
    let devirt_ok = |ctx: &Context, st: &State<'_>, sym: &str, dv: &mut FxHashMap<_, _>| {
        if bonus <= 1 {
            return;
        }
        if let Some((eh, n)) = eligible(ctx, st, sym, limit * bonus, false) {
            let ps = devirt_params(ctx, st.funcs[sym].op);
            if !ps.is_empty() {
                dv.insert(sym.to_string(), (st.funcs[sym].op, eh, n, ps));
            }
        }
    };
    let order: Vec<String> = if bottom_up {
        post_order(ctx, st)
    } else {
        for sym in st.funcs.keys() {
            if always_only && !st.funcs[sym].always_inline {
                continue;
            }
            if let Some((eh, n)) = eligible(ctx, st, sym, limit, single.contains(sym)) {
                ok.insert(sym.clone(), (st.funcs[sym].op, eh, n));
            } else {
                devirt_ok(ctx, st, sym, &mut dv);
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
        let mut size: usize = blocks(ctx, caller).iter().map(|&b| ops(ctx, b).len()).sum();
        for b in blocks(ctx, caller) {
            for op in ops(ctx, b) {
                if !bottom_up && st.invokes.contains_key(&op) {
                    continue;
                }
                if only.is_some_and(|o| !o.contains(&op)) {
                    continue;
                }
                let Some(cs) = direct_callee(ctx, st, op) else {
                    continue;
                };
                let Some((callee, eh, n)) = ok.get(cs).copied().or_else(|| {
                    let (f, eh, n, ps) = dv.get(cs)?;
                    let args = Operation::get_op::<CallOp>(op, ctx)?.args(ctx);
                    ps.iter()
                        .any(|&i| {
                            args.get(i).is_some_and(|a| {
                                matches!(st.consts.get(a), Some(ConstVal::Sym { .. }))
                            })
                        })
                        .then_some((*f, *eh, *n))
                }) else {
                    continue;
                };
                // A callee with landing pads may go into an invoke whose pad is a
                // cleanup: its `_Unwind_Resume` becomes an invoke of that pad. Not
                // into a catch pad: unwinder phase 1 would never see that catch.
                if eh
                    && st
                        .invokes
                        .get(&op)
                        .is_some_and(|&(_, catch)| catch || !eh_invoke)
                {
                    continue;
                }
                // Stop growing a caller past the cap: huge generated functions
                // (cranelift's ISLE lowering) would otherwise blow up memory.
                if callee != caller && st.calls[&op].fn_ty == st.funcs[cs].ty && size + n <= cap {
                    size += n;
                    sites.push((op, callee));
                }
            }
        }
        for (call, callee) in sites {
            inline_call(ctx, st, &mut rw, call, callee);
        }
        if bottom_up {
            if let Some((eh, n)) = eligible(ctx, st, &sym, limit, single.contains(&sym)) {
                ok.insert(sym.clone(), (caller, eh, n));
            } else {
                devirt_ok(ctx, st, &sym, &mut dv);
            }
        }
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        left_stats(ctx, st, &ok);
    }
}

/// Indirect calls whose callee is a constant function address (a vtable slot
/// folded by `constload`, e.g. hashbrown's `&mut dyn FnMut` probe callback)
/// become direct calls. Returns the rewritten calls, for a second inline round.
pub fn devirt(ctx: &mut Context, st: &mut State<'_>) -> FxHashSet<Ptr<Operation>> {
    let mut out = FxHashSet::default();
    let fns: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    for f in fns {
        for b in blocks(ctx, f) {
            for c in ops(ctx, b) {
                let Some(call) = Operation::get_op::<CallOp>(c, ctx) else {
                    continue;
                };
                let CallOpCallable::Indirect(p) = call.callee(ctx) else {
                    continue;
                };
                let Some(ConstVal::Sym { sym, off: 0 }) = st.consts.get(&p) else {
                    continue;
                };
                let (Some(fi), Some(info), Some(id)) = (
                    st.funcs.get(sym),
                    st.calls.get(&c),
                    st.sym_to_ident.get(sym),
                ) else {
                    continue;
                };
                if info.fn_ty != fi.ty {
                    continue;
                }
                let (id, fty) = (id.clone(), info.fn_ty);
                let fty = TypedHandle::<FuncType>::from_handle(fty, ctx).unwrap();
                let args = call.args(ctx);
                let nc = CallOp::new(ctx, CallOpCallable::Direct(id), fty, args).get_operation();
                nc.insert_before(ctx, c);
                if c.deref(ctx).get_num_results() > 0 {
                    let (old, new) = (c.deref(ctx).get_result(0), nc.deref(ctx).get_result(0));
                    old.replace_all_uses_with(ctx, &new);
                }
                let info = st.calls.remove(&c).unwrap();
                st.calls.insert(nc, info);
                if let Some(u) = st.invokes.remove(&c) {
                    st.invokes.insert(nc, u);
                }
                if let Some(e) = st.expect.remove(&c) {
                    st.expect.insert(nc, e);
                }
                if st.last_call == Some(c) {
                    st.last_call = Some(nc);
                }
                Operation::erase(c, ctx);
                out.insert(nc);
                if std::env::var_os("PLIRON_STATS_DEVIRT").is_some() {
                    let caller = st
                        .funcs
                        .iter()
                        .find(|(_, x)| x.op == f)
                        .map(|(n, _)| n.clone());
                    eprintln!("devirt {caller:?} -> {sym}");
                }
            }
        }
    }
    if std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("devirt {}: {} calls", st.cgu, out.len());
    }
    out
}

/// Why direct calls to defined functions were left in place (`PLIRON_STATS`).
fn left_stats(
    ctx: &Context,
    st: &State<'_>,
    ok: &FxHashMap<String, (Ptr<Operation>, bool, usize)>,
) {
    let mut why: FxHashMap<&str, usize> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
                let Some(cs) = direct_callee(ctx, st, op) else {
                    continue;
                };
                let g = &st.funcs[cs];
                let r = if !has_body(ctx, g.op) {
                    "external"
                } else if g.no_inline {
                    "no_inline"
                } else if g.linkage == Linkage::Preemptible {
                    "preemptible"
                } else if cs == sym {
                    "self"
                } else if let Some(&(_, eh, _)) = ok.get(cs) {
                    if eh && st.invokes.contains_key(&op) {
                        "eh callee at invoke (catch pad / EH_INVOKE=0)"
                    } else if st.calls[&op].fn_ty != g.ty {
                        "fn type mismatch"
                    } else {
                        "eligible but left"
                    }
                } else {
                    "too big / recursive / va"
                };
                *why.entry(r).or_default() += 1;
            }
        }
    }
    eprintln!("inline left {}: {why:?}", st.cgu);
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
            if st.nonnull.contains(&op) {
                st.nonnull.insert(new);
            }
            if st.inbounds.contains(&op) {
                st.inbounds.insert(new);
            }
            if st.bool01.contains(&op) {
                st.bool01.insert(new);
            }
            let n = op.deref(ctx).get_num_results();
            for i in 0..n {
                let (v, nv) = (op.deref(ctx).get_result(i), new.deref(ctx).get_result(i));
                if let Some(a) = st.allocas.get(&v).copied() {
                    st.allocas.insert(nv, a);
                }
                if let Some(t) = st.promoted.get(&v).copied() {
                    st.promoted.insert(nv, t);
                }
                if let Some(c) = st.consts.get(&v).cloned() {
                    st.consts.insert(nv, c);
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

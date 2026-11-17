//! memcpyopt call-slot forwarding: `memcpy(%dst, %tmp, n)`/`memmove`
//! where `%tmp` is an `n`-byte alloca forwards every write the temp's
//! use-cone carries — `store`s, `memset`/`memcpy`/`memmove` destinations
//! and `sret` call arguments — onto `%dst` directly, then drops the copy
//! and the temp. `call f(sret %tmp); memcpy(%dst, %tmp, n)` becomes
//! `call f(sret %dst)`; a struct built field-by-field in a temp then
//! whole-copied (`store`, `memcpy`, `memmove` into a parent field or the
//! caller's `sret` slot) writes the field straight into its home.
//! Unwritten ranges of `[0, n)` refine from the temp's uninit alloca
//! bytes to `%dst`'s old ones; reads of the temp rebase onto `%dst` and
//! see the same bytes or an equally-valid undef refinement. Writers must
//! dominate the copy, each writer's committed range must be clear of
//! `dst` touches until the copy would have run, and copy sources plus
//! non-`sret` call arguments must be provably disjoint from `%dst`.
//! `PLIRON_MEMCPYOPT=0` disables it.

use pliron::{
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    operation::Operation,
    value::Value,
};
use pliron::builtin::op_interfaces::BranchOpInterface;
use pliron::op::op_cast;
use pliron::r#type::Typed;
use pliron_llvm::ops::{AllocaOp, AtomicLoadOp, AtomicStoreOp, CallOp, GetElementPtrOp, ICmpOp, LoadOp, StoreOp};
use rustc_hash::{FxHashMap, FxHashSet};
use pliron::basic_block::BasicBlock;

use crate::domcheck::Dom;
use crate::opt_body;
use crate::sroa::{const_int, gep_offset};
use crate::state::{ArgExt, State};
use crate::types::size_align;

/// `PLIRON_TRACE=pliron::memcpyopt=debug`: why a copy was not forwarded.
macro_rules! why {
    ($($t:tt)*) => {{
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::memcpyopt", "fwd: {}", format!($($t)*));
        }
        return false;
    }};
}

/// The domtree for `f`, built on first use.
fn dom_of<'a>(
    ctx: &Context,
    st: &State,
    slot: &'a mut Option<Dom>,
    f: Ptr<Operation>,
) -> Option<&'a Dom> {
    if slot.is_none() {
        *slot = Some(Dom::new(ctx, st, f)?);
    }
    slot.as_ref()
}

/// The per-function incoming-edge map for block arguments, built on
/// first need — a parameter the forwards below never resolve never pays
/// for the walk.
fn inc_of<'a>(
    ctx: &Context,
    f: Ptr<Operation>,
    slot: &'a mut Option<IncMap>,
) -> Option<&'a IncMap> {
    if slot.is_none() {
        *slot = Some(incoming_map(ctx, f));
    }
    slot.as_ref()
}

/// `a` strictly dominates `b` — same block settled by op order, others
/// by the domtree.
fn op_dominates(
    ctx: &Context,
    st: &State,
    dom: &mut Option<Dom>,
    f: Ptr<Operation>,
    a: Ptr<Operation>,
    b: Ptr<Operation>,
) -> bool {
    crate::domcheck::op_dominates(ctx, st, dom, f, a, b)
}

/// Is any op in `set` strictly between `prod` and `m`? Ops before `prod`
/// still see `dst`'s old bytes, which the forward doesn't change; ops
/// dominated by `m` see the producer's bytes either way. An op in between
/// could observe the difference. Same-block position is settled by op
/// order, cross-block by the domtree (`m` being strictly dominated by
/// `prod`, a third block is "before prod" only if it dominates `prod`'s).
fn any_between(
    ctx: &Context,
    st: &State,
    dom: &mut Option<Dom>,
    f: Ptr<Operation>,
    m: Ptr<Operation>,
    prod: Ptr<Operation>,
    set: &FxHashSet<Ptr<Operation>>,
) -> bool {
    let pathwin = pliron_cfg::env_is("PLIRON_MEMCPYOPT_PATHWIN", "1");
    any_between_with(ctx, st, dom, f, m, prod, set, pathwin)
}

#[allow(clippy::too_many_arguments)]
fn any_between_with(
    ctx: &Context,
    st: &State,
    dom: &mut Option<Dom>,
    f: Ptr<Operation>,
    m: Ptr<Operation>,
    prod: Ptr<Operation>,
    set: &FxHashSet<Ptr<Operation>>,
    pathwin: bool,
) -> bool {
    let (mb, pb) = (
        m.deref(ctx).get_parent_block().unwrap(),
        prod.deref(ctx).get_parent_block().unwrap(),
    );
    let mut same_m: FxHashSet<Ptr<Operation>> = FxHashSet::default();
    let mut same_p: FxHashSet<Ptr<Operation>> = FxHashSet::default();
    let mut other: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
    for &uo in set {
        match uo.deref(ctx).get_parent_block() {
            Some(b) if b == mb => {
                same_m.insert(uo);
            }
            Some(b) if b == pb => {
                same_p.insert(uo);
            }
            Some(b) => {
                other.insert(b);
            }
            None => return true,
        }
    }
    if !same_m.is_empty() {
        // One walk: uses before `prod` (same block only) or after `m`.
        // `prod` in another block dominates this one, so the window is
        // already open at the block head.
        let mut window = pb != mb;
        let mut cur = mb.deref(ctx).iter(ctx).next();
        while let Some(o) = cur {
            if pb == mb && o == prod {
                window = true;
            }
            if o == m {
                window = false;
            }
            if !window {
                same_m.remove(&o);
            }
            cur = o.get_next(ctx);
        }
        if !same_m.is_empty() {
            return true;
        }
    }
    if !same_p.is_empty() {
        let mut cur = pb.deref(ctx).iter(ctx).next();
        while let Some(o) = cur {
            if o == prod {
                break;
            }
            same_p.remove(&o);
            cur = o.get_next(ctx);
        }
        if !same_p.is_empty() {
            return true;
        }
    }
    if let Some(d) = (!other.is_empty()).then(|| dom_of(ctx, st, dom, f)).flatten() {
        let cand: Vec<Ptr<BasicBlock>> = other
            .iter()
            .copied()
            .filter(|&b| !(d.dominates(mb, b) || d.dominates(b, pb)))
            .collect();
        if cand.is_empty() || !pathwin {
            return !cand.is_empty();
        }
        // `PLIRON_MEMCPYOPT_PATHWIN`: the dominator test calls every
        // sibling block "between" — in a function with several `return`
        // sites each `store tmp; memcpy(sret, tmp)` pair was refused for
        // the other sites' copies into the same `sret` (the dsa
        // interpreter's `step_one` copied its 160-byte result on every
        // step). A block is between only if it lies on a path from
        // `prod` to `m`: with both in one block there is none, and
        // otherwise it reaches `m`'s block backwards without crossing
        // `prod`'s (which dominates `m`'s, so such a block is also
        // reachable from it). Routes that leave through `m` and come
        // back are the callers' wrap checks, not this window.
        if pb == mb {
            return false;
        }
        let mut seen: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
        let mut stack: Vec<Ptr<BasicBlock>> = mb.preds(ctx).into_iter().collect();
        while let Some(b) = stack.pop() {
            if b == pb || !seen.insert(b) {
                continue;
            }
            if seen.len() > 256 {
                return true;
            }
            stack.extend(b.preds(ctx));
        }
        cand.iter().any(|b| seen.contains(b))
    } else {
        !other.is_empty()
    }
}

/// The byte width `op` reads through pointer operand `i` of a chain that
/// started at `tmp`, or `None` when `op` is not a pure reader (stores and
/// copy destinations write; calls, compares and block args can observe
/// `tmp`'s address itself).
fn leaf_read(ctx: &Context, st: &State, op: Ptr<Operation>, i: usize) -> Option<u64> {
    if st.volatile.contains(op) {
        return None;
    }
    if Operation::is_op::<LoadOp>(op, ctx) && i == 0 {
        return Some(size_align(ctx, op.deref(ctx).get_result(ctx, 0).get_type(ctx)).0);
    }
    if i == 1
        && matches!(
            st.intrinsics.get(op).map(|s| s.as_str()),
            Some("llvm.memcpy" | "llvm.memmove")
        )
    {
        return op
            .deref(ctx)
            .operands(ctx)
            .nth(2)
            .and_then(|v| const_int(ctx, st, v))
            .and_then(|n| u64::try_from(n).ok());
    }
    // A call operand that is a frozen `&T` (`arg_ro` + `arg_noalias`): the
    // callee reads up to `arg_extent` bytes at the call's position and the
    // borrow's scope ends when it returns — a bounded read. Invokes and
    // black_box barriers stay opaque; `sret`/`ByVal` operands are not
    // reads (leaf_write and escape respectively).
    if st.invokes.contains(op) {
        return None;
    }
    let ci = st.calls.get(op)?;
    if ci.bb {
        return None;
    }
    let nopd = op.deref(ctx).get_num_operands();
    let first_arg = nopd - ci.exts.params.len().min(nopd);
    let ai = i.checked_sub(first_arg)?;
    if ci.exts.params.get(ai) != Some(&ArgExt::None) {
        return None;
    }
    let ro = ci.arg_ro.as_ref().and_then(|v| v.get(ai)).copied() == Some(true);
    let na = ci.arg_noalias.as_ref().and_then(|v| v.get(ai)).copied() == Some(true);
    if !(ro && na) {
        return None;
    }
    ci.arg_extent.as_ref().and_then(|e| e.get(ai)).map(|&(n, _)| n)
}

/// The block argument terminator `op`'s operand `i` forwards, when `i`
/// lies in a successor's operand range. Other operand positions — a
/// `cond_br` condition, a `br_table` index — are scalar uses, not
/// pointer forwarding.
fn succ_param(ctx: &Context, op: Ptr<Operation>, i: usize) -> Option<Value> {
    let o = Operation::get_op_dyn(op, ctx);
    let br = op_cast::<dyn BranchOpInterface>(&*o)?;
    for (si, s) in op.deref(ctx).successors(ctx).enumerate() {
        let r = br.successor_operand_range(ctx, si);
        if r.contains(&i) {
            return Some(s.deref(ctx).get_argument(ctx, i - r.start));
        }
    }
    None
}

/// The byte width `op` writes through a `tmp`-derived pointer at operand
/// `i`, when it is a pure writer: a typed store, a memory intrinsic's
/// destination, or a call's `sret` argument — the whole return region
/// per `arg_extent`. Volatile ops, invokes, and uses that merely carry
/// the pointer (callee operands, non-`sret` args, GEP indices) refuse.
fn leaf_write(ctx: &Context, st: &State, op: Ptr<Operation>, i: usize) -> Option<u64> {
    if st.volatile.contains(op) || st.invokes.contains(op) {
        return None;
    }
    if Operation::is_op::<StoreOp>(op, ctx) {
        return (i == 1)
            .then(|| size_align(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx)).0);
    }
    if i == 0
        && matches!(
            st.intrinsics.get(op).map(|s| s.as_str()),
            Some("llvm.memcpy" | "llvm.memmove" | "llvm.memset")
        )
    {
        return op
            .deref(ctx)
            .operands(ctx)
            .nth(2)
            .and_then(|v| const_int(ctx, st, v))
            .and_then(|n| u64::try_from(n).ok());
    }
    let ci = st.calls.get(op)?;
    let nopd = op.deref(ctx).get_num_operands();
    let first_arg = nopd - ci.exts.params.len().min(nopd);
    let ai = i.checked_sub(first_arg)?;
    if ci.exts.params.get(ai) != Some(&ArgExt::SRet) {
        return None;
    }
    ci.arg_extent
        .as_ref()
        .and_then(|e| e.get(ai))
        .map(|&(n, _)| n)
}

/// `tmp`'s use-cone, classified for forwarding: every path through
/// constant GEPs must end at a pure read or write of `[0, n)` — the
/// copy's extent, which is also `tmp`'s alloca size. Returns the chain
/// roots to rebase (every non-marker direct use), the writer ops with
/// their `[lo, hi)` ranges in `tmp`, the reader ops likewise, and the
/// lifetime markers to erase. Anything that can observe `tmp`'s
/// address — compares, `ptrtoint`, block args, call operands,
/// non-constant GEPs — refuses the cone.
fn tmp_flow(
    ctx: &Context,
    st: &State,
    m: Ptr<Operation>,
    tmp: Value,
    n: u64,
) -> Option<(
    Vec<(Ptr<Operation>, usize)>,
    Vec<(Ptr<Operation>, u64, u64)>,
    Vec<(Ptr<Operation>, u64, u64)>,
    Vec<Ptr<Operation>>,
)> {
    let mut roots: Vec<(Ptr<Operation>, usize)> = Vec::new();
    let mut writers: Vec<(Ptr<Operation>, u64, u64)> = Vec::new();
    let mut readers: Vec<(Ptr<Operation>, u64, u64)> = Vec::new();
    let mut marks: Vec<Ptr<Operation>> = Vec::new();
    let mut seen = 0usize;
    // `(value, offset)` pairs already walked — a block param can carry the
    // temp's pointer back into its own block (loop-carried), so the cone
    // is a graph, not a tree.
    let mut done: FxHashSet<(Value, u64)> = FxHashSet::default();
    let mut work: Vec<(Value, u64, bool)> = vec![(tmp, 0, true)];
    while let Some((p, bo, root)) = work.pop() {
        if !done.insert((p, bo)) {
            continue;
        }
        for u in p.uses_iter(ctx) {
            seen += 1;
            if seen > 512 {
                return None;
            }
            let op = u.user_op();
            let i = u.find_index(ctx);
            if op == m {
                continue;
            }
            if matches!(
                st.intrinsics.get(op).map(|s| s.as_str()),
                Some("llvm.lifetime.start" | "llvm.lifetime.end")
            ) {
                if root {
                    marks.push(op);
                }
                continue;
            }
            if root {
                roots.push((op, i));
            }
            if Operation::is_op::<GetElementPtrOp>(op, ctx) && i == 0 {
                let nb = gep_offset(ctx, st, op).and_then(|o| (bo as i64).checked_add(o))?;
                if nb < 0 {
                    return None;
                }
                work.push((op.deref(ctx).get_result(ctx, 0), nb as u64, false));
            } else if let Some(param) = succ_param(ctx, op, i) {
                // A branch argument carries the derived pointer to the
                // target's parameter — the parameter's own uses classify
                // from there. Only the temp's edge is rebased; other edges
                // keep feeding the param their own pointers, so merged
                // params stay correct.
                work.push((param, bo, false));
            } else if let Some(w) = leaf_read(ctx, st, op, i) {
                if bo + w > n {
                    return None;
                }
                readers.push((op, bo, bo + w));
            } else if let Some(w) = leaf_write(ctx, st, op, i) {
                if bo + w > n {
                    return None;
                }
                writers.push((op, bo, bo + w));
            } else {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(
                        target: "pliron::memcpyopt",
                        "tmp use is opaque: {}",
                        Operation::get_opid(op, ctx)
                    );
                }
                return None;
            }
        }
    }
    Some((roots, writers, readers, marks))
}

#[cfg(test)]
thread_local! {
    /// Unit tests turn `PLIRON_MEMCPYOPT_MOVEMARK` on for one case.
    static TEST_MOVEMARK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn movemark() -> bool {
    #[cfg(test)]
    if TEST_MOVEMARK.with(|c| c.get()) {
        return true;
    }
    crate::env_is("PLIRON_MEMCPYOPT_MOVEMARK", "1")
}

/// Move `mk`, a `lifetime.start` of `dbase` that sits between the
/// forwarded writers and their copy, to just ahead of the first writer.
/// Only when every writer is in `mk`'s block ahead of it, no operation
/// from the first writer up to `mk` touches `dbase` (the writers still
/// address the temp here) or is another of its markers, and `mk`'s
/// operands are defined ahead of the new place. `false` leaves it alone.
fn move_start_marker(
    ctx: &mut Context,
    st: &State,
    dbase: Value,
    size: u64,
    mk: Ptr<Operation>,
    wset: &FxHashSet<Ptr<Operation>>,
    dmarks: &FxHashSet<Ptr<Operation>>,
) -> bool {
    if st.intrinsics.get(mk).map(|s| s.as_str()) != Some("llvm.lifetime.start") {
        return false;
    }
    let Some(b) = mk.deref(ctx).get_parent_block() else {
        return false;
    };
    let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
    let Some(mi) = ops.iter().position(|&o| o == mk) else {
        return false;
    };
    let Some(first) = ops[..mi].iter().position(|o| wset.contains(o)) else {
        return false;
    };
    // Every writer is in this block, ahead of the marker.
    if ops[..mi].iter().filter(|o| wset.contains(o)).count() != wset.len() {
        return false;
    }
    let Some(touch) = base_touches(ctx, st, dbase, size, 0, size) else {
        return false;
    };
    let window: FxHashSet<Ptr<Operation>> = ops[first..mi].iter().copied().collect();
    if window.iter().any(|o| dmarks.contains(o) || touch.contains_key(o)) {
        return false;
    }
    // An operand computed inside the window would be used before its
    // definition.
    let n = mk.deref(ctx).get_num_operands();
    for k in 0..n {
        let v = mk.deref(ctx).get_operand(ctx, k);
        if v.defining_op().is_some_and(|d| window.contains(&d)) {
            return false;
        }
    }
    mk.unlink(ctx);
    mk.insert_before(ctx, ops[first]);
    true
}

/// Forward `m = memcpy|memmove(dst, tmp, n)` where `tmp` is an `n`-byte
/// alloca: rebase every `tmp`-rooted pointer to `dbase + doff` and drop
/// `m`, the temp and its markers. Each writer's bytes then land in `dst`
/// at the writer's own position instead of at `m`, which is exact for
/// the ranges any writer covers and a valid undef refinement for the
/// rest (the unwritten tail is `tmp`'s uninit alloca bytes either way).
/// Soundness rests on four checks, all below: every writer dominates `m`
/// (post-`m` `tmp` writes would leak into `dst`); a copy writer's source
/// is provably disjoint from `dst` (an `llvm.memcpy` would otherwise
/// trade guaranteed-disjoint semantics for possibly-overlapping, and an
/// aliased read would observe `dst`'s new bytes early — `memmove` gets
/// the same check); no op touches `dst`'s copy range between a writer
/// and `m` (it would see the write early); and `tmp`'s use-cone is only
/// reads and writes inside `[0, n)` through constant GEPs.
fn try_forward(
    ctx: &mut Context,
    st: &mut State,
    f: Ptr<Operation>,
    dom: &mut Option<Dom>,
    pin: &mut Option<IncMap>,
    sret_args: &[Value],
    m: Ptr<Operation>,
) -> bool {
    if st.volatile.contains(m)
        || !matches!(
            st.intrinsics.get(m).map(|s| s.as_str()),
            Some("llvm.memcpy" | "llvm.memmove")
        )
        || st.invokes.contains(m)
    {
        return false;
    }
    let o: Vec<Value> = m.deref(ctx).operands(ctx).collect();
    let (dst, src) = (o[0], o[1]);
    let Some(n) = const_int(ctx, st, o[2]).and_then(|n| u64::try_from(n).ok()) else {
        return false;
    };
    let Some(a) = src.defining_op() else {
        return false;
    };
    if !Operation::is_op::<AllocaOp>(a, ctx) || st.allocas.get(src).map(|x| x.0) != Some(n) {
        return false;
    }
    let Some((roots, writers, readers, marks)) = tmp_flow(ctx, st, m, src, n) else {
        why!("src has opaque or out-of-range uses");
    };
    let all_dom = writers
        .iter()
        .all(|&(w, _, _)| op_dominates(ctx, st, dom, f, w, m));
    // A `memcpy(tmp, tmp)` would rebase into `memcpy(p, p)` — overlap an
    // `llvm.memcpy` forbids. Rare enough to refuse outright.
    if readers.iter().any(|r| writers.iter().any(|w| w.0 == r.0)) {
        why!("self-copy through the temp");
    }
    // Where `tmp`'s range lands in `dst`: the caller's `sret` arg at
    // offset 0, or a constant-offset field of an alloca it fits inside.
    // `PLIRON_MEMCPYOPT_NOALIASDST=1`: also into the pointee of one of the
    // function's own `noalias` parameters (`&mut T`), see `noalias_dst`.
    let mut param_dst: Option<u64> = None;
    let (dbase, doff) = if sret_args.contains(&dst) {
        (dst, 0)
    } else if let Some((p, off, size)) = noalias_dst(ctx, st, f, dst, n) {
        param_dst = Some(size);
        (p, off)
    } else {
        let Some((base, off)) = alloca_base(ctx, st, inc_of(ctx, f, pin), dst) else {
            if pliron_cfg::tracing_on() {
                // What `dst` is, after the GEPs above it (constant or not):
                // the census of 2026-11-17 for `PLIRON_MEMCPYOPT_NOALIASDST`.
                let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head();
                let mut r = dst;
                let mut via = "";
                for _ in 0..16 {
                    match r.defining_op() {
                        Some(d) if Operation::is_op::<GetElementPtrOp>(d, ctx) => {
                            via = if gep_offset(ctx, st, d).is_some() && via != " via var gep" { " via gep" } else { " via var gep" };
                            r = d.deref(ctx).get_operand(ctx, 0);
                        }
                        _ => break,
                    }
                }
                let kind: String = match r.defining_op() {
                    None if r.defining_block().is_some_and(|b| Some(b) == entry) => {
                        match st.noalias.get(r) {
                            Some(&(_, true)) => "entry-param noalias".to_string(),
                            Some(_) => "entry-param noalias readonly".to_string(),
                            None if sret_args.contains(&r) => "entry-param sret".to_string(),
                            None => "entry-param".to_string(),
                        }
                    }
                    None if st.region_noalias.contains(r) => "clone-arg noalias (scoped)".to_string(),
                    None => "block-arg".to_string(),
                    Some(d) if Operation::is_op::<AllocaOp>(d, ctx) => "alloca".to_string(),
                    Some(d) if Operation::is_op::<LoadOp>(d, ctx) => "load".to_string(),
                    Some(d) if st.calls.get(d).is_some() => "call".to_string(),
                    Some(d) => Operation::get_opid(d, ctx).to_string(),
                };
                tracing::debug!(target: "pliron::memcpyopt", "dst unbased: {kind}{via}");
            }
            why!("dst has no provable base");
        };
        if base == src
            || !st
                .allocas
                .get(base)
                .is_some_and(|x| off <= x.0 && n <= x.0 - off)
        {
            why!("dst field out of bounds or based on src");
        }
        (base, off)
    };
    // Writer-specific hazards: a copy writer's source must be provably
    // disjoint from `dst` (and not `tmp`-derived, or the rebase dangles
    // it), and a call writer's pointer arguments must be too — otherwise
    // the callee could observe its own `sret` writes through them once
    // they land in `dst` instead of `tmp`.
    for &(w, _, _) in &writers {
        if st.calls.get(w).is_some() {
            let nopd = w.deref(ctx).get_num_operands();
            let first_arg = nopd - st.calls.get(w).unwrap().exts.params.len().min(nopd);
            for (j, x) in w.deref(ctx).operands(ctx).enumerate() {
                if j < first_arg {
                    continue;
                }
                if crate::types::leaf_is_ptr(ctx, x.get_type(ctx)).contains(&true) {
                    if !(disjoint(ctx, st, inc_of(ctx, f, pin), sret_args, x, dst)
                        || (param_dst.is_some() && alloca_bases(ctx, st, inc_of(ctx, f, pin), x).is_some()))
                    {
                        why!("call writer's arg may alias dst");
                    }
                    // `concat = f(concat)`: the arg's alloca holds a byval
                    // copy of `dst` (`memcpy(arg_tmp, dst)`), so forwarding
                    // lands `sret` in `dst` one step too early — `try_byval`
                    // can then no longer home the arg into `dst` without
                    // aliasing `sret` and the byval arg. Keep the copy-back
                    // so the home fires.
                    if let Some(bs) = alloca_bases(ctx, st, inc_of(ctx, f, pin), x) {
                        for xa in bs {
                            let copied = xa.uses_iter(ctx).any(|u| {
                                let cu = u.user_op();
                                u.find_index(ctx) == 0
                                    && matches!(
                                        st.intrinsics.get(cu).map(|s| s.as_str()),
                                        Some("llvm.memcpy" | "llvm.memmove")
                                    )
                                    && {
                                        let s = cu.deref(ctx).get_operand(ctx, 1);
                                        s == dbase
                                            || alloca_base(
                                                ctx,
                                                st,
                                                inc_of(ctx, f, pin),
                                                s,
                                            )
                                            .is_some_and(|(b, _)| b == dbase)
                                            || alloca_bases(
                                                ctx,
                                                st,
                                                inc_of(ctx, f, pin),
                                                s,
                                            )
                                            .is_some_and(|b| b.contains(&dbase))
                                    }
                            });
                            if copied {
                                why!("call arg holds a byval copy of dst");
                            }
                        }
                    }
                }
            }
        } else if matches!(
            st.intrinsics.get(w).map(|s| s.as_str()),
            Some("llvm.memcpy" | "llvm.memmove")
        ) {
            let x = w.deref(ctx).operands(ctx).nth(1).unwrap();
            if alloca_bases(ctx, st, inc_of(ctx, f, pin), x)
                .is_some_and(|bs| bs.contains(&src))
                || !(disjoint(ctx, st, inc_of(ctx, f, pin), sret_args, x, dst)
                    || (param_dst.is_some() && alloca_bases(ctx, st, inc_of(ctx, f, pin), x).is_some()))
            {
                why!("copy writer's src may alias dst");
            }
        }
    }
    // `dbase`'s definition must reach every op a rebased pointer is
    // inserted before (allocas are entry-hoisted and the `sret` arg
    // dominates everything, so this only bites for odd IR).
    let dom_ok = dbase
        .defining_op()
        .is_none_or(|bd| roots.iter().all(|&(u, _)| op_dominates(ctx, st, dom, f, bd, u)));
    if !dom_ok {
        why!("dst base does not dominate all rebase sites");
    }
    let size = st.allocas.get(dbase).map(|x| x.0).or(param_dst).unwrap_or(u64::MAX);
    // The caller sees a `noalias` parameter's pointee when the function
    // returns or unwinds, which a local slot it cannot reach is not: the
    // rewritten writers must not become visible before the copy would
    // have run. See `exits_between`.
    if param_dst.is_some() {
        for &(w, _, _) in &writers {
            if may_unwind(ctx, st, w) {
                why!("noalias dst: a writer may unwind after a partial write");
            }
        }
        if let Some(e) = exits_between(ctx, st, &writers, m) {
            why!("noalias dst: the caller may see it between a writer and the copy ({e})");
        }
    }
    let mut all_touch = None;
    // `dst` copied into a call's byval temp (`memcpy(arg_tmp, dst)`,
    // arg_tmp passed to a call) is a `try_byval` home: sinking a writer
    // into `dst` makes the call reach `dst` directly, which then blocks
    // the home — `sret` and the byval arg can't share `dst`. Keep the
    // copy so `try_byval` fires (it only runs under PLIRON_BYVALHOME).
    if pliron_cfg::env_is("PLIRON_BYVALHOME", "1") {
        let Some(t) = base_touches(ctx, st, dbase, size, 0, size) else {
            why!("dst use-cone not fully walkable");
        };
        let home_src = t.keys().any(|&c| {
            matches!(
                st.intrinsics.get(c).map(|s| s.as_str()),
                Some("llvm.memcpy" | "llvm.memmove")
            ) && alloca_bases(
                ctx,
                st,
                inc_of(ctx, f, pin),
                c.deref(ctx).get_operand(ctx, 1),
            )
            .is_some_and(|bs| bs.contains(&dbase))
                && alloca_base(
                    ctx,
                    st,
                    inc_of(ctx, f, pin),
                    c.deref(ctx).get_operand(ctx, 0),
                )
                .is_some_and(|(xa, _)| reaches_call(ctx, xa))
        });
        if home_src {
            why!("dst is a byval-home source");
        }
        all_touch = Some(t);
    }
    // Full-range writers get a check that subsumes windows, reader
    // dominance and the stale-read rule alike — and also accepts
    // sibling-arm writers (`if c { f(sret tmp) } else { g(sret tmp) };
    // memcpy(dst, tmp)`): walking backwards from every consumer of the
    // copy's value (`m` and each `tmp` reader), the first `dst`-range
    // touch on every route must be a writer. Unlike block-dominance
    // windows the scan sees real routes only (a `dst` write on a dead
    // side branch is never visited) and it sees loop wraps (a `dst`
    // touch dominated by `m` still poisons a route that reaches `m`
    // again). A route that dies at a pred-less block (entry or dead
    // code) never ran a writer, so `tmp` is still alloca-undef there
    // and `dst`'s bytes are a legal refinement. Partial-coverage
    // writers fall back to the dominance path below.
    //
    // `PLIRON_MEMCPYOPT_SIBARMS=1` takes partial writers that do not all
    // dominate `m` through the same scan, with "first touch is a writer"
    // generalized to byte coverage (`if c { tmp.a = x } else { tmp.a = y };
    // tmp.b = z; memcpy(dst, tmp)`). The coverage condition: walking
    // backward from a consumer (`m` with the bytes `[0, n)`, a `tmp`
    // reader with its own range), a route carries the set of bytes no
    // writer seen on it so far covers; a writer removes its range, and
    // the route is cut once the set is empty. Up to that cut, no other op
    // may touch `dst`'s copy range (`poison`, as above). Why that is
    // enough: at the consumer, each byte holds the value of the last
    // writer of that byte on the route taken, which is the one the scan
    // met first for it; after the rebase that writer wrote the byte to
    // `dst` instead, and nothing between it and the consumer touched
    // `dst` (every op between them was scanned while the byte was still
    // uncovered, or it is a later writer of other bytes). A byte still
    // uncovered where the route ends at a pred-less block was written by
    // no writer on that route: `tmp`'s alloca-undef in the original, so
    // `dst`'s own bytes are a refinement. Routes that wrap around a loop
    // keep walking with their set, so a writer of the previous iteration
    // counts only for bytes nothing later on the route rewrote. Writers
    // before the cut on a route are earlier writes of bytes a later
    // writer covers: they land in `dst` earlier than before, which the
    // reader mirror check below (a `dst` read that a writer reaches) and,
    // for a parameter `dst`, `exits_between` keep unobservable. The scan
    // A writer the scan never meets wrote only bytes that later writers
    // rewrite (see the check after the scan). With full-range writers the
    // set has one piece and this is the scan as it was.
    let full = !writers.is_empty() && writers.iter().all(|&(_, lo, hi)| lo == 0 && hi == n);
    let sib = !full && !all_dom && !writers.is_empty() && sibarms_on();
    let cover = full || sib;
    if cover {
        // Profitability, not soundness: a `tmp` written only by scalar
        // ops could still promote — its copies and readers would
        // scalarize away — so sinking it into a `dst` whose address a
        // call takes trades promotion-eligible bytes for a pinned slot
        // and hardened `memcpy`s (byvalhome's `x = f(x)` shape). A
        // call-written `tmp` already escapes, so nothing is lost by
        // moving the write into `dst`.
        //
        // `PLIRON_MEMCPYOPT_BIGTMP=1`: a `tmp` larger than sroa's
        // `MAX_SIZE` never promotes, so it is not protected either:
        // `driftsort_main`'s 4 KiB `AlignedStorage` scratch, filled by an
        // inlined `memset` and copied whole into the slot that
        // `drift::sort` receives, kept both 4 KiB slots (frame 8 KiB
        // against LLVM's 4 KiB; stanza `bigtmp`).
        let bigtmp = pliron_cfg::env_is("PLIRON_MEMCPYOPT_BIGTMP", "1");
        let tmp_esc = writers
            .iter()
            .any(|&(w, _, _)| st.calls.get(w).is_some() || st.invokes.contains(w))
            || (bigtmp && st.allocas.get(src).is_some_and(|x| x.0 > crate::sroa::MAX_SIZE));
        if !tmp_esc {
            let t = match all_touch {
                Some(t) => t,
                None => match base_touches(ctx, st, dbase, size, 0, size) {
                    Some(t) => t,
                    None => {
                        why!("dst use-cone not fully walkable");
                    }
                },
            };
            if t
                .keys()
                .any(|&t| st.calls.get(t).is_some() || st.invokes.contains(t))
            {
                why!("tmp could promote; dst escapes to a call");
            }
        }
        let Some(touch) = base_touches(ctx, st, dbase, size, doff, n) else {
            why!("dst use-cone not fully walkable");
        };
        let wset: FxHashSet<Ptr<Operation>> = writers.iter().map(|&(w, _, _)| w).collect();
        let mut poison: FxHashSet<Ptr<Operation>> = touch.keys().copied().collect();
        for &w in &wset {
            poison.remove(&w);
        }
        poison.remove(&m);
        // Byte coverage as a bit set over the pieces between writer range
        // ends (one piece for full-range writers).
        let mut bounds: Vec<u64> = vec![0, n];
        for &(_, lo, hi) in &writers {
            bounds.extend([lo, hi]);
        }
        bounds.sort_unstable();
        bounds.dedup();
        if bounds.len() > 65 {
            why!("too many writer pieces");
        }
        let pieces = |lo: u64, hi: u64, overlap: bool| -> u64 {
            let mut k = 0u64;
            for (i, w) in bounds.windows(2).enumerate() {
                let inside = if overlap { w[0] < hi && lo < w[1] } else { lo <= w[0] && w[1] <= hi };
                if inside {
                    k |= 1 << i;
                }
            }
            k
        };
        let mut wmask: FxHashMap<Ptr<Operation>, u64> = FxHashMap::default();
        for &(w, lo, hi) in &writers {
            *wmask.entry(w).or_default() |= pieces(lo, hi, false);
        }
        let mut stack: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>, u64)> = Vec::new();
        if let Some(mb) = m.deref(ctx).get_parent_block() {
            stack.push((mb, Some(m), pieces(0, n, true)));
        }
        for &(r, ro, rh) in &readers {
            if let Some(rb) = r.deref(ctx).get_parent_block() {
                stack.push((rb, Some(r), pieces(ro, rh, true)));
            }
        }
        // A node is `(block, stop-before, uncovered)`: the seed scans only
        // ops before its consumer; a block reached on a route sits whole
        // on that route and scans all its ops (so a post-`m` touch in
        // `m`'s own block still poisons a wrap).
        let mut seen: FxHashSet<(Ptr<BasicBlock>, Option<Ptr<Operation>>, u64)> =
            FxHashSet::default();
        let mut steps = 0usize;
        while let Some((b, upto, left)) = stack.pop() {
            if !seen.insert((b, upto, left)) {
                continue;
            }
            let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
            let end = upto
                .and_then(|u| ops.iter().position(|&o| o == u))
                .unwrap_or(ops.len());
            let mut left = left;
            for &o in ops[..end].iter().rev() {
                steps += 1;
                if steps > 8192 {
                    why!("route scan over budget");
                }
                if let Some(&k) = wmask.get(&o) {
                    left &= !k;
                    if left == 0 {
                        break;
                    }
                    continue;
                }
                if poison.contains(&o) {
                    why!("dst touched on an uncovered route");
                }
            }
            if left == 0 {
                continue;
            }
            stack.extend(b.preds(ctx).into_iter().map(|p| (p, None, left)));
        }
        // A writer the scan never met has every byte rewritten by a later
        // writer on every route into a consumer. Its early `dst` bytes,
        // like those of an arm writer whose arm can also leave the
        // function, are seen only by a `dst` read it reaches (the mirror
        // check below) or, for memory the caller owns, at an exit: the
        // `noalias` parameter case checked `exits_between` above, and an
        // `sret` destination gets the same check here. Full-range writers
        // keep the rule they had.
        if sib && sret_args.contains(&dbase) {
            for &(w, _, _) in &writers {
                if may_unwind(ctx, st, w) {
                    why!("sret dst: a writer may unwind after a partial write");
                }
            }
            if let Some(e) = exits_between(ctx, st, &writers, m) {
                why!("sret dst: the caller may see it between a writer and the copy ({e})");
            }
        }
        // `dst` readers get the mirror check: the transform drops `m`'s
        // write, so a `dst` read sees the last write among {plain `dst`
        // writes, `m`} before it — after the rebase, among {plain `dst`
        // writes, `tmp` writers}. Walking backward from each op that may
        // read the copy range, the first byte-changing touch must be `m`
        // (the reader sits in the copied window) or a plain `dst` write
        // (it hides anything earlier). A `tmp` writer there means the
        // write sat between `m` and the reader — its new bytes would
        // leak into `dst` ahead of schedule. Pure reads are transparent
        // (they change nothing) and `Other` is too — it may not write,
        // so it can't cut the route — while calls and escapes are
        // seeded like plain readers: their own consistency is proven by
        // their own backward scan. Routes reaching pred-less blocks
        // leave `dst` alloca-undef on both sides.
        let mut rseen: FxHashSet<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> =
            FxHashSet::default();
        let mut stack: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> = Vec::new();
        for (&r, &kind) in &touch {
            if kind == TouchKind::Write {
                continue;
            }
            if let Some(rb) = r.deref(ctx).get_parent_block() {
                stack.push((rb, Some(r)));
            }
        }
        while let Some((b, upto)) = stack.pop() {
            if !rseen.insert((b, upto)) {
                continue;
            }
            let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
            let end = upto
                .and_then(|u| ops.iter().position(|&o| o == u))
                .unwrap_or(ops.len());
            let mut cut = false;
            for &o in ops[..end].iter().rev() {
                steps += 1;
                if steps > 8192 {
                    why!("route scan over budget");
                }
                if wset.contains(&o) {
                    why!("tmp write reaches a dst reader after the copy");
                }
                // `m` is `Write` — a reader in the copied window stops
                // there; a plain `dst` write hides anything earlier.
                // `Read` and `Other` are transparent: neither provably
                // writes the bytes.
                if let Some(&TouchKind::Write) = touch.get(&o) {
                    cut = true;
                    break;
                }
            }
            if cut {
                continue;
            }
            stack.extend(b.preds(ctx).into_iter().map(|p| (p, None)));
        }
    } else if !all_dom {
        why!("writer does not dominate the copy");
    } else {
        // Between a writer and `m`, nothing may touch the range that writer
        // commits to `dst` — it would observe the early write. Ops outside
        // the range keep reading `dst`'s old bytes, which is what they saw
        // before `m` ran anyway.
        for &(w, lo, hi) in &writers {
            let Some(t) = base_touches(ctx, st, dbase, size, doff + lo, hi - lo) else {
                why!("dst use-cone not fully walkable");
            };
            let mut set: FxHashSet<Ptr<Operation>> = t.keys().copied().collect();
            set.remove(&m);
            set.remove(&w);
            if any_between(ctx, st, dom, f, m, w, &set) {
                why!("dst touched in writer-to-copy window");
            }
            // The window above is the first visit only. A copy in a loop
            // its writer sits outside of runs again without the writer:
            // it re-commits `tmp`'s bytes over whatever the loop did to
            // `dst` since (`let mut x = empty();` at the top of a loop
            // body whose `empty()` value was built once before the loop
            // — regex-syntax's `alternate_literals` kept the previous
            // iteration's moved-out `Vec` and freed it twice). With `m`
            // gone nothing restores them, so on every route from `m`
            // back to `m` that does not pass the writer, nothing may
            // change the range: walk backward from `m`, cut at the
            // writer, refuse at a `Write`/`Other` touch. Reads are fine
            // — they see the bytes `m` would have written.
            let mut stack: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> = Vec::new();
            if let Some(mb) = m.deref(ctx).get_parent_block() {
                stack.push((mb, Some(m)));
            }
            let mut seen: FxHashSet<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> =
                FxHashSet::default();
            let mut steps = 0usize;
            while let Some((b, upto)) = stack.pop() {
                if !seen.insert((b, upto)) {
                    continue;
                }
                let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
                let end = upto
                    .and_then(|u| ops.iter().position(|&o| o == u))
                    .unwrap_or(ops.len());
                let mut cut = false;
                for &o in ops[..end].iter().rev() {
                    steps += 1;
                    if steps > 8192 {
                        why!("wrap scan over budget");
                    }
                    if o == w {
                        cut = true;
                        break;
                    }
                    if o != m && t.get(&o).is_some_and(|&k| k != TouchKind::Read) {
                        why!("dst changed on a route that reaches the copy again");
                    }
                }
                if cut {
                    continue;
                }
                stack.extend(b.preds(ctx).into_iter().map(|p| (p, None)));
            }
        }
    }
    // A reader after `m` reads `tmp`'s final bytes — `dst`'s after the
    // rebase — so the copy must dominate it (a path skipping `m` could
    // miss writers entirely) and nothing may touch its range in `dst`
    // in between (a write would substitute bytes `tmp` never held).
    // Under the writer-cut rule the route scan has already proven the
    // same property for every reader, so these checks are only needed
    // when it did not run.
    // `PLIRON_MEMCPYOPT_RDWRAP=0`: without the wrap scan of post-copy
    // readers below (the rule until 2026-11-16; for bisecting only).
    let rdwrap = pliron_cfg::pass_enabled("PLIRON_MEMCPYOPT_RDWRAP");
    for &(r, ro, rh) in &readers {
        if cover {
            continue;
        }
        if op_dominates(ctx, st, dom, f, r, m) {
            // A reader before `m` sees `dst`'s new bytes early. That is
            // the correct value only where a writer covering the byte
            // dominates `r`: a byte covered by some writer but not by
            // one dominating `r` may hold defined leftovers from an
            // earlier dynamic visit (a loop wrap — `r` dominates `m`,
            // so `m` can reach `r` through a back edge whose `dst`
            // touches sit in no writer window), and the rebase would
            // swap them for `dst`'s own bytes. Bytes no writer covers
            // are always alloca-undef — refining them to `dst`'s bytes
            // is legal.
            let mut left = vec![(ro, rh)];
            for &(w, lo, hi) in &writers {
                if !op_dominates(ctx, st, dom, f, w, r) {
                    continue;
                }
                left = left
                    .iter()
                    .flat_map(|&(a, b)| {
                        [
                            (a < lo.min(b)).then_some((a, lo.min(b))),
                            (hi.max(a) < b).then_some((hi.max(a), b)),
                        ]
                    })
                    .flatten()
                    .collect();
            }
            let stale = left
                .iter()
                .any(|&(a, b)| writers.iter().any(|&(_, lo, hi)| a < hi && lo < b));
            if stale {
                why!("pre-copy read of possibly-stale bytes");
            }
            continue;
        }
        if !op_dominates(ctx, st, dom, f, m, r) {
            why!("post-copy read not dominated by the copy");
        }
        let Some(t) = base_touches(ctx, st, dbase, size, doff + ro, rh - ro) else {
            why!("dst use-cone not fully walkable");
        };
        let mut set: FxHashSet<Ptr<Operation>> = t.keys().copied().collect();
        set.remove(&m);
        set.remove(&r);
        for &(w, _, _) in &writers {
            set.remove(&w);
        }
        if any_between(ctx, st, dom, f, r, m, &set) {
            why!("dst touched between copy and post-copy read");
        }
        // The window above is the first visit only: it calls everything
        // the reader's block dominates "after the reader". A reader in a
        // loop the copy sits outside of runs again, and what the loop did
        // to `dst` after the reader is then ahead of it (`let mut body =
        // String::new();` before a loop whose body builds another empty
        // `String` from the same constant temporary and then appends to
        // `body`: rebased, the second string started as a copy of `body`
        // — two owners of one heap block, dsa's `emit_shrink`). `tmp`
        // itself never changes there (no writer lies on a route from the
        // copy to a reader it dominates), so on every route from `r` back
        // to `m` nothing may change the reader's range of `dst`: walk
        // backward from `r`, cut at `m`, refuse at a `Write`/`Other`
        // touch. Reads are fine. `r`'s own block is scanned whole when a
        // route comes back to it.
        if rdwrap {
            let mut stack: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> = Vec::new();
            if let Some(rb) = r.deref(ctx).get_parent_block() {
                stack.push((rb, Some(r)));
            }
            let mut seen: FxHashSet<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> =
                FxHashSet::default();
            let mut steps = 0usize;
            while let Some((b, upto)) = stack.pop() {
                if !seen.insert((b, upto)) {
                    continue;
                }
                let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
                let end = upto
                    .and_then(|u| ops.iter().position(|&o| o == u))
                    .unwrap_or(ops.len());
                let mut cut = false;
                for &o in ops[..end].iter().rev() {
                    steps += 1;
                    if steps > 8192 {
                        why!("reader wrap scan over budget");
                    }
                    if o == m {
                        cut = true;
                        break;
                    }
                    if t.get(&o).is_some_and(|&k| k != TouchKind::Read) {
                        why!("dst changed on a route from the copy to a post-copy read");
                    }
                }
                if cut {
                    continue;
                }
                stack.extend(b.preds(ctx).into_iter().map(|p| (p, None)));
            }
        }
    }
    // `dst`'s own lifetime markers. The rebase moves `tmp`'s writes into
    // `dst` at the writers' positions, and `tmp`'s later reads onto `dst`:
    // a `dst` marker between a writer and a consumer of the copy's value
    // (`m` or a `tmp` reader) then calls bytes dead that are in fact
    // carried across it — `lifetime.start(dst)` right before the copy is
    // rustc's usual shape for a moved temporary. slotmerge trusts such a
    // marker as a kill and hands the bytes to another slot (mimas
    // `install_assoc`: a `Vec` header built early, moved into a struct
    // whose storage starts late; the header was overwritten by a 36-byte
    // temporary and the interpreter recursed forever). Walk back from
    // every consumer, cutting at writers; a marker on the way (or a scan
    // over budget) drops all of `dst`'s markers, which leaves the slot
    // live wherever its bytes are used.
    // `PLIRON_MEMCPYOPT_DSTMARK=0` skips this: a miscompile, kept (like
    // `PLIRON_MEMCPYOPT_RDWRAP=0`) for bisecting and to show the IR-pass
    // differential check (`harness/xcorpus.py`) finding it.
    if st.allocas.get(dbase).is_some() && pliron_cfg::pass_enabled("PLIRON_MEMCPYOPT_DSTMARK") {
        let dmarks: FxHashSet<Ptr<Operation>> = dbase
            .uses_iter(ctx)
            .map(|u| u.user_op())
            .filter(|&o| {
                matches!(
                    st.intrinsics.get(o).map(|s| s.as_str()),
                    Some("llvm.lifetime.start" | "llvm.lifetime.end")
                )
            })
            .collect();
        if !dmarks.is_empty() {
            let wset: FxHashSet<Ptr<Operation>> = writers.iter().map(|&(w, _, _)| w).collect();
            let mut stack: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> = Vec::new();
            for c in std::iter::once(m).chain(readers.iter().map(|r| r.0)) {
                if let Some(cb) = c.deref(ctx).get_parent_block() {
                    stack.push((cb, Some(c)));
                }
            }
            let mut seen: FxHashSet<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> =
                FxHashSet::default();
            let mut steps = 0usize;
            // Markers met on the way back from a consumer; `over`: the
            // scan ran out of budget and may have missed some.
            let mut hits: Vec<Ptr<Operation>> = Vec::new();
            let mut over = false;
            'scan: while let Some((b, upto)) = stack.pop() {
                if !seen.insert((b, upto)) {
                    continue;
                }
                let ops: Vec<Ptr<Operation>> = b.deref(ctx).iter(ctx).collect();
                let end = upto
                    .and_then(|u| ops.iter().position(|&o| o == u))
                    .unwrap_or(ops.len());
                let mut cut = false;
                for &o in ops[..end].iter().rev() {
                    steps += 1;
                    if steps > 8192 {
                        over = true;
                        break 'scan;
                    }
                    if dmarks.contains(&o) {
                        if !hits.contains(&o) {
                            hits.push(o);
                        }
                        cut = true;
                        break;
                    }
                    if wset.contains(&o) {
                        cut = true;
                        break;
                    }
                }
                if !cut {
                    stack.extend(b.preds(ctx).into_iter().map(|p| (p, None)));
                }
            }
            if over || !hits.is_empty() {
                // `PLIRON_MEMCPYOPT_MOVEMARK=1` (opt-in): the usual shape
                // is one block, `writers..; lifetime.start(dst); copy`.
                // The marker says `dst`'s bytes are dead where it stands;
                // with nothing touching `dst` between the first writer
                // and the marker they are as dead ahead of that writer,
                // so the marker moves there and the slot keeps its
                // bounds. Anything else drops the markers as before.
                let moved = !over
                    && hits.len() == 1
                    && movemark()
                    && move_start_marker(ctx, st, dbase, size, hits[0], &wset, &dmarks);
                if moved {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::memcpyopt", "fwd: moved the dst start marker of {dbase:?} ahead of the writers");
                    }
                } else {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::memcpyopt", "fwd: dropping {} dst markers of {dbase:?}", dmarks.len());
                    }
                    for o in dmarks {
                        st.intrinsics.remove(o);
                        Operation::erase(o, ctx);
                    }
                }
            }
        }
    }
    for &(u, i) in &roots {
        let p = crate::sroa::ptr_plus(ctx, st, dbase, doff, u);
        Operation::replace_operand(u, ctx, i, p);
    }
    st.intrinsics.remove(m);
    Operation::erase(m, ctx);
    for o in marks {
        st.intrinsics.remove(o);
        Operation::erase(o, ctx);
    }
    if pliron_cfg::tracing_on() {
        tracing::debug!(target: "pliron::memcpyopt", "fwd: FORWARDED {m:?} dst {dbase:?}@{doff} src {src:?}");
    }
    st.allocas.remove(src);
    st.promoted.remove(src);
    Operation::erase(a, ctx);
    true
}

/// Whether `p`'s pointer cone reaches a call operand, through constant
/// GEPs and branch successor arguments (bounded; overflow answers yes —
/// the callers use this only to refuse a transform).
fn reaches_call(ctx: &Context, p: Value) -> bool {
    let mut work = vec![p];
    let mut seen: FxHashSet<Value> = FxHashSet::default();
    let mut steps = 0usize;
    while let Some(v) = work.pop() {
        if !seen.insert(v) {
            continue;
        }
        for u in v.uses_iter(ctx) {
            steps += 1;
            if steps > 256 {
                return true;
            }
            let op = u.user_op();
            if Operation::is_op::<CallOp>(op, ctx) {
                return true;
            }
            if Operation::is_op::<GetElementPtrOp>(op, ctx) && u.find_index(ctx) == 0 {
                work.push(op.deref(ctx).get_result(ctx, 0));
            } else if let Some(param) = succ_param(ctx, op, u.find_index(ctx)) {
                work.push(param);
            }
        }
    }
    false
}

/// Every block parameter's incoming edges in `f`, as `(terminator,
/// successor index, operand position)` triples — one pass over the
/// terminators, built on first need. The edge VALUES must not be cached:
/// `replace_all_uses_with` rewrites operand lists and `try_forward`
/// erases ops, so the operand is fetched lazily from the (never-erased)
/// terminator at resolution time.
type IncMap = FxHashMap<Value, Vec<(Ptr<Operation>, usize, usize)>>;

fn incoming_map(ctx: &Context, f: Ptr<Operation>) -> IncMap {
    let mut m: IncMap = FxHashMap::default();
    for b in f.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx) {
        let Some(t) = b.deref(ctx).get_terminator(ctx) else {
            continue;
        };
        let o = Operation::get_op_dyn(t, ctx);
        let Some(br) = op_cast::<dyn BranchOpInterface>(&*o) else {
            continue;
        };
        for (si, s) in t.deref(ctx).successors(ctx).enumerate() {
            for k in 0..br.successor_operands(ctx, si).len() {
                m.entry(s.deref(ctx).get_argument(ctx, k))
                    .or_default()
                    .push((t, si, k));
            }
        }
    }
    m
}

/// The value terminator `t` forwards into successor `si`'s `k`-th
/// argument, or None if the edge's operand list no longer matches the
/// successor's arity (refuse rather than guess).
fn inc_value(ctx: &Context, t: Ptr<Operation>, si: usize, k: usize) -> Option<Value> {
    let o = Operation::get_op_dyn(t, ctx);
    let br = op_cast::<dyn BranchOpInterface>(&*o)?;
    let v = br.successor_operands(ctx, si);
    let na = t.deref(ctx).get_successor(ctx, si).deref(ctx).get_num_arguments();
    if v.len() != na {
        return None;
    }
    v.get(k).copied()
}

/// `PLIRON_MEMCPYOPT_NOALIASDST=1`: `dst` as a constant offset into the
/// pointee of one of `f`'s own `noalias` parameters, writable and
/// dereferenceable over `[off, off + n)`: `(param, off, dereferenceable
/// bytes)`.
///
/// Why that is as good as a local slot: the forward's safety argument
/// enumerates every op that may touch the destination object
/// (`base_touches` over the uses of `dbase`). For an alloca that is every
/// use of it; rustc's `noalias` on a `&mut T` parameter promises the same
/// for the duration of the call: no access to the pointee while the
/// function runs goes through a pointer not derived from the parameter,
/// so its uses here are all of them (a callee handed a derived pointer is
/// such a use). What a local slot does not have is an observer outside
/// the function: the caller reads the pointee after a return or an
/// unwind. `try_forward` therefore also refuses when a redirected writer
/// may unwind, or a return or an unwinding op lies on a path from a
/// writer that does not pass the copy (`exits_between`).
///
/// Only the function's own entry parameters count: the `noalias` of an
/// inlined callee's parameter (`State::region_noalias`, a clone-entry
/// block argument) holds for the inlined body only, and its copies and
/// writers may well sit outside it.
fn noalias_dst(
    ctx: &Context,
    st: &State,
    f: Ptr<Operation>,
    dst: Value,
    n: u64,
) -> Option<(Value, u64, u64)> {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_MEMCPYOPT_NOALIASDST", "1"));
    if !*ON && !TEST_NOALIASDST.with(|c| c.get()) {
        return None;
    }
    let mut v = dst;
    let mut off: i64 = 0;
    for _ in 0..16 {
        match v.defining_op() {
            Some(d) if Operation::is_op::<GetElementPtrOp>(d, ctx) => {
                off = off.checked_add(gep_offset(ctx, st, d)?)?;
                v = d.deref(ctx).get_operand(ctx, 0);
            }
            Some(_) => return None,
            None => break,
        }
    }
    let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head()?;
    if v.defining_block() != Some(entry) || st.region_noalias.contains(v) {
        return None;
    }
    let &(deref, writable) = st.noalias.get(v)?;
    let off = u64::try_from(off).ok()?;
    (writable && deref > 0 && off.checked_add(n)? <= deref).then_some((v, off, deref))
}

thread_local! {
    /// Unit tests turn `PLIRON_MEMCPYOPT_NOALIASDST` on without the env.
    static TEST_NOALIASDST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Unit tests set `PLIRON_MEMCPYOPT_SIBARMS` (default on) per case:
    /// `Some(on)` overrides the environment, `None` reads it.
    static TEST_SIBARMS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `PLIRON_MEMCPYOPT_SIBARMS` (default on since 2026-11-17, `=0` off):
/// partial writers that do not all dominate the copy go through the
/// coverage scan of `try_forward`.
fn sibarms_on() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_MEMCPYOPT_SIBARMS", "1"));
    TEST_SIBARMS.with(|c| c.get()).unwrap_or(*ON)
}

/// May `op` unwind out of the function (or into a landing pad, whose
/// path the caller then sees through `resume`)? Invokes and calls not
/// proven `nounwind` may; intrinsics only when they throw. Under
/// `panic=abort` (`State::panic_abort`) no call does: rustc already marks
/// the declared callees `nounwind`, and the rest (indirect calls) cannot
/// unwind past a Rust frame either.
fn may_unwind(ctx: &Context, st: &State, op: Ptr<Operation>) -> bool {
    if st.invokes.contains(op) {
        return true;
    }
    if st.panic_abort {
        return false;
    }
    if let Some(name) = st.intrinsics.get(op) {
        return name.contains("throw") || name.contains("rethrow") || name.starts_with("pliron.eh.rethrow");
    }
    if Operation::is_op::<CallOp>(op, ctx) {
        return !crate::inline::direct_callee(ctx, st, op)
            .and_then(|c| st.funcs.get(c))
            .is_some_and(|fi| fi.nounwind);
    }
    false
}

/// Is there a path from one of `writers` to a point where the caller
/// observes memory (a return, an op that may unwind) that does not pass
/// through the copy `m`? Walked forward from each writer, stopping at
/// `m`; a path that ends in `unreachable` observes nothing. Over budget
/// counts as "yes". Returns what was found: `"return"`, `"unwind"` or
/// `"budget"`.
fn exits_between(
    ctx: &Context,
    st: &State,
    writers: &[(Ptr<Operation>, u64, u64)],
    m: Ptr<Operation>,
) -> Option<&'static str> {
    let mut seen: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
    let mut steps = 0usize;
    let mut stack: Vec<Option<Ptr<Operation>>> = writers.iter().map(|&(w, _, _)| w.get_next(ctx)).collect();
    while let Some(start) = stack.pop() {
        let mut cur = start;
        let mut last = None;
        while let Some(o) = cur {
            steps += 1;
            if steps > 8192 {
                return Some("budget");
            }
            if o == m {
                last = None;
                break;
            }
            if Operation::is_op::<pliron_llvm::ops::ReturnOp>(o, ctx) {
                return Some("return");
            }
            if may_unwind(ctx, st, o) {
                return Some("unwind");
            }
            last = Some(o);
            cur = o.get_next(ctx);
        }
        let Some(t) = last else { continue };
        for s in t.deref(ctx).successors(ctx) {
            if seen.insert(s) {
                stack.push(s.deref(ctx).iter(ctx).next());
            }
        }
    }
    None
}

/// The alloca `v` points into and the constant byte offset, through a chain
/// of constant GEPs — and, when `inc` is given, through block parameters
/// every incoming edge resolves to the same alloca and offset.
fn alloca_base(
    ctx: &Context,
    st: &State,
    inc: Option<&IncMap>,
    v: Value,
) -> Option<(Value, u64)> {
    let mut memo = FxHashMap::default();
    let mut on = FxHashSet::default();
    alloca_base_d(ctx, st, inc, v, &mut memo, &mut on)
}

fn alloca_base_d(
    ctx: &Context,
    st: &State,
    inc: Option<&IncMap>,
    mut v: Value,
    memo: &mut FxHashMap<Value, Option<(Value, u64)>>,
    on: &mut FxHashSet<Value>,
) -> Option<(Value, u64)> {
    let mut off: i64 = 0;
    for _ in 0..16 {
        if let Some(d) = v.defining_op() {
            if Operation::is_op::<AllocaOp>(d, ctx) {
                return (st.allocas.contains(v) && off >= 0).then_some((v, off as u64));
            }
            if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
                return None;
            }
            off = off.checked_add(gep_offset(ctx, st, d)?)?;
            v = d.deref(ctx).get_operand(ctx, 0);
            continue;
        }
        // Block argument: it is `base + off` only when every incoming edge
        // forwards a pointer resolving to the same base and offset. A
        // loop-carried param's backedge value recurses into the param
        // itself — `on` spots the cycle and refuses.
        if let Some(&r) = memo.get(&v) {
            return r;
        }
        let mut res: Option<(Value, u64)> = None;
        if on.insert(v) {
            if let Some(edges) = inc.and_then(|m| m.get(&v)).filter(|v| !v.is_empty()) {
                let mut ok = true;
                for &(t, si, k) in edges {
                    let bo = inc_value(ctx, t, si, k)
                        .and_then(|iv| alloca_base_d(ctx, st, inc, iv, memo, on));
                    match (res, bo) {
                        (None, Some(b)) => res = Some(b),
                        (Some(a), Some(b)) if a == b => {}
                        _ => {
                            res = None;
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    res = None;
                }
            }
            on.remove(&v);
        }
        memo.insert(v, res);
        let (b, po) = res?;
        off = off.checked_add(i64::try_from(po).ok()?)?;
        return (off >= 0).then(|| (b, off as u64));
    }
    None
}

/// Every alloca `v` may point into, through constant GEPs and block
/// parameters — the union of `alloca_base`'s per-edge resolutions rather
/// than their agreement, so a param merging different slots still
/// resolves. `None` when any edge is opaque, a cycle bites, or more than
/// 16 bases meet (bounded: ambiguity grows monotonically).
fn alloca_bases(
    ctx: &Context,
    st: &State,
    inc: Option<&IncMap>,
    v: Value,
) -> Option<FxHashSet<Value>> {
    let mut memo: FxHashMap<Value, Option<FxHashSet<Value>>> = FxHashMap::default();
    let mut on = FxHashSet::default();
    alloca_bases_d(ctx, st, inc, v, &mut memo, &mut on)
        .filter(|s| !s.is_empty() && s.len() <= 16)
}

fn alloca_bases_d(
    ctx: &Context,
    st: &State,
    inc: Option<&IncMap>,
    mut v: Value,
    memo: &mut FxHashMap<Value, Option<FxHashSet<Value>>>,
    on: &mut FxHashSet<Value>,
) -> Option<FxHashSet<Value>> {
    for _ in 0..16 {
        if let Some(d) = v.defining_op() {
            if Operation::is_op::<AllocaOp>(d, ctx) {
                return st.allocas.contains(v).then(|| [v].into_iter().collect());
            }
            if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
                return None;
            }
            v = d.deref(ctx).get_operand(ctx, 0);
            continue;
        }
        if let Some(r) = memo.get(&v) {
            return r.clone();
        }
        if !on.insert(v) {
            memo.insert(v, None);
            return None;
        }
        let mut acc: Option<FxHashSet<Value>> = Some(FxHashSet::default());
        if let Some(edges) = inc.and_then(|m| m.get(&v)).filter(|e| !e.is_empty()) {
            for &(t, si, k) in edges {
                let Some(iv) = inc_value(ctx, t, si, k)
                    .and_then(|iv| alloca_bases_d(ctx, st, inc, iv, memo, on))
                else {
                    acc = None;
                    break;
                };
                let a = acc.as_mut().unwrap();
                a.extend(iv);
                if a.len() > 16 {
                    acc = None;
                    break;
                }
            }
        } else {
            acc = None;
        }
        on.remove(&v);
        memo.insert(v, acc.clone());
        return acc;
    }
    None
}

/// `x` and `dst` provably point at different memory: distinct alloca bases
/// (stack slots never overlap, whatever the offsets), or an alloca base
/// against the caller's `sret` argument (a caller-owned pointer cannot
/// address this frame). Two `sret`-side pointers may alias each other —
/// not disjoint.
fn disjoint(
    ctx: &Context,
    st: &State,
    inc: Option<&IncMap>,
    sret_args: &[Value],
    x: Value,
    dst: Value,
) -> bool {
    match (
        alloca_bases(ctx, st, inc, x),
        alloca_bases(ctx, st, inc, dst),
        sret_args.contains(&x),
        sret_args.contains(&dst),
    ) {
        (Some(a), Some(b), _, _) => a.is_disjoint(&b),
        (Some(_), _, _, true) | (_, Some(_), true, _) => true,
        _ => false,
    }
}

/// What an op may do to the bytes a tracked pointer addresses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TouchKind {
    /// Only reads (a `load`, a `memcpy`'s source operand).
    Read,
    /// Only writes (a `store`, a `memcpy`/`memset`'s destination).
    Write,
    /// May do either or something else entirely: a call operand, the
    /// pointer escaping as a scalar (`gep` index, stored value), or an
    /// access through a derived pointer whose offset tracking was lost.
    Other,
}

/// Every op that may touch the range, classified — `Read` and `Other`
/// may observe the bytes, `Write` and `Other` may change them.
type Touches = FxHashMap<Ptr<Operation>, TouchKind>;

/// Every op that may touch bytes `[off, off+len)` of the alloca `base`,
/// walked through GEP chains (`None` = the walk overflowed or the base's
/// address reached something unbounded, so "not provably clear"). An op
/// whose range is unknown — a call taking the pointer, a store of the
/// pointer as a value, a non-constant GEP's consumer — counts as touching
/// everything.
fn base_touches(
    ctx: &Context,
    st: &State,
    base: Value,
    size: u64,
    off: u64,
    len: u64,
) -> Option<Touches> {
    let mut out: Touches = FxHashMap::default();
    let mut seen = 0usize;
    // The address left the use-def cone (stored as a value, used as an
    // index, turned into an integer): from then on any access through a
    // pointer we cannot place may be an access to `base`.
    let mut escaped = false;
    // (pointer value, its byte offset in base, offset known) — a graph,
    // not a tree: branch operands carry the pointer into block params,
    // and a loop-carried param can carry it back in.
    let mut done: FxHashSet<(Value, u64, bool)> = FxHashSet::default();
    let mut work: Vec<(Value, u64, bool)> = vec![(base, 0, true)];
    while let Some((p, bo, known)) = work.pop() {
        if !done.insert((p, bo, known)) {
            continue;
        }
        for u in p.uses_iter(ctx) {
            seen += 1;
            if seen > 512 {
                return None;
            }
            let op = u.user_op();
            let i = u.find_index(ctx);
            if let Some(param) = succ_param(ctx, op, i) {
                work.push((param, bo, known));
                continue;
            }
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if i != 0 {
                    // The pointer is an index — it escapes as a scalar.
                    out.insert(op, TouchKind::Other);
                    escaped = true;
                    continue;
                }
                let r = op.deref(ctx).get_result(ctx, 0);
                match gep_offset(ctx, st, op).and_then(|go| (bo as i64).checked_add(go)) {
                    Some(nb) if known && nb >= 0 && nb as u64 <= size => {
                        work.push((r, nb as u64, true))
                    }
                    _ => work.push((r, 0, false)),
                }
                continue;
            }
            if matches!(
                st.intrinsics.get(op).map(|s| s.as_str()),
                Some("llvm.lifetime.start" | "llvm.lifetime.end")
            ) {
                continue;
            }
            if !known {
                out.insert(op, TouchKind::Other);
                continue;
            }
            // Byte range of the access, when it is statically known.
            let (range, kind) = if Operation::is_op::<LoadOp>(op, ctx) {
                (
                    Some((bo, size_align(ctx, op.deref(ctx).get_result(ctx, 0).get_type(ctx)).0)),
                    TouchKind::Read,
                )
            } else if Operation::is_op::<StoreOp>(op, ctx) && i == 1 {
                (
                    Some((bo, size_align(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx)).0)),
                    TouchKind::Write,
                )
            } else if i <= 1
                && matches!(
                    st.intrinsics.get(op).map(|s| s.as_str()),
                    Some("llvm.memcpy" | "llvm.memmove" | "llvm.memset")
                )
            {
                (
                    op.deref(ctx)
                        .operands(ctx)
                        .nth(2)
                        .and_then(|v| const_int(ctx, st, v))
                        .and_then(|n| u64::try_from(n).ok())
                        .map(|n| (bo, n)),
                    if i == 0 { TouchKind::Write } else { TouchKind::Read },
                )
            } else {
                // A call operand is a touch at the call (as before); a
                // compare observes nothing. Everything else — the address
                // stored as a value above all — publishes it.
                if !(Operation::is_op::<CallOp>(op, ctx)
                    || st.invokes.contains(op)
                    || st.intrinsics.get(op).is_some()
                    || Operation::is_op::<ICmpOp>(op, ctx))
                {
                    escaped = true;
                }
                (None, TouchKind::Other)
            };
            if range.is_none_or(|(lo, n)| lo < off + len && off < lo.saturating_add(n)) {
                let e = out.entry(op).or_insert(kind);
                if *e != kind {
                    *e = TouchKind::Other;
                }
            }
        }
    }
    if escaped {
        // Reads and writes of `base` can now come through pointers the
        // cone never saw (`let r = &mut x;` kept in a closure environment
        // or an argument array, reloaded and dereferenced — fuzz seeds
        // 30621/30696/31403 read the destination through such a pointer
        // inside the writer-to-copy window). Every access whose pointer
        // does not resolve to a different alloca, and every call, may
        // touch the range.
        let Some(region) = base
            .defining_op()
            .and_then(|a| a.deref(ctx).get_parent_block())
            .and_then(|b| b.deref(ctx).get_parent_region())
        else {
            return None;
        };
        let mut n = 0usize;
        let elsewhere = |p: Value| alloca_base(ctx, st, None, p).is_some_and(|(b, _)| b != base);
        for b in region.deref(ctx).iter(ctx) {
            for op in b.deref(ctx).iter(ctx) {
                n += 1;
                if n > 40_000 {
                    return None;
                }
                if out.contains_key(&op) {
                    continue;
                }
                let o = op.deref(ctx);
                let wild = if Operation::is_op::<LoadOp>(op, ctx) {
                    !elsewhere(o.get_operand(ctx, 0))
                } else if Operation::is_op::<StoreOp>(op, ctx) {
                    !elsewhere(o.get_operand(ctx, 1))
                } else if let Some(name) = st.intrinsics.get(op) {
                    if name.starts_with("llvm.mem") {
                        !(elsewhere(o.get_operand(ctx, 0))
                            && (name == "llvm.memset" || elsewhere(o.get_operand(ctx, 1))))
                    } else {
                        !(name.starts_with("llvm.lifetime")
                            || name.starts_with("llvm.dbg")
                            || name == "llvm.assume"
                            || name.starts_with("llvm.expect"))
                    }
                } else {
                    Operation::is_op::<CallOp>(op, ctx)
                        || st.invokes.contains(op)
                        || Operation::is_op::<AtomicLoadOp>(op, ctx)
                        || Operation::is_op::<AtomicStoreOp>(op, ctx)
                };
                if wild {
                    out.insert(op, TouchKind::Other);
                }
            }
        }
    }
    Some(out)
}

// `PLIRON_TRACE=pliron::memcpyopt=debug`: why a by-value copy was not
// forwarded (`nope!` inside `home_users`, `bail!` in `try_byval`, `why!`
// in `try_forward`).
macro_rules! nope {
    ($($t:tt)*) => {{
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::memcpyopt", "byval: {}", format!($($t)*));
        }
        return None;
    }};
}
macro_rules! bail {
    ($($t:tt)*) => {{
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::memcpyopt", "byval: {}", format!($($t)*));
        }
        return false;
    }};
}

/// One op's touch of a home alloca: the byte range it may read and/or
/// write, or a lifetime marker on the whole object (`kill`: the bytes are
/// undefined past it).
#[derive(Clone, Copy)]
struct Touch {
    read: bool,
    write: bool,
    lo: u64,
    hi: u64,
    kill: bool,
}

/// Every op that touches alloca `a` (through constant GEPs) with the byte
/// ranges it may read or write; `None` when `a`'s address can leave the
/// function's view: stored as a value, turned into an integer, compared,
/// carried by a block argument, or passed to a call that may retain it
/// (neither rustc's `captures(none)` nor our own `nocapture` inference
/// clears the operand) or whose reach is unknown (a raw pointer).
fn home_users(
    ctx: &Context,
    st: &State,
    a: Value,
    size: u64,
) -> Option<FxHashMap<Ptr<Operation>, Vec<Touch>>> {
    let mut out: FxHashMap<Ptr<Operation>, Vec<Touch>> = FxHashMap::default();
    let mut work = vec![(a, 0u64)];
    let mut seen = 0usize;
    let touch = |read, write, lo: u64, n: u64| Touch {
        read,
        write,
        lo,
        hi: lo.saturating_add(n),
        kill: false,
    };
    while let Some((p, base)) = work.pop() {
        for u in p.uses_iter(ctx) {
            seen += 1;
            if seen > 512 {
                nope!("home: gep as a non-base operand");
            }
            let op = u.user_op();
            let i = u.find_index(ctx);
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if i != 0 {
                    nope!("home: gep past the object");
                }
                let o = gep_offset(ctx, st, op)?;
                let nb = (base as i64).checked_add(o)?;
                if nb < 0 || nb as u64 > size {
                    nope!("home: address stored as a value");
                }
                work.push((op.deref(ctx).get_result(ctx, 0), nb as u64));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                let w = size_align(ctx, op.deref(ctx).get_result(ctx, 0).get_type(ctx)).0;
                out.entry(op).or_default().push(touch(true, false, base, w));
            } else if Operation::is_op::<StoreOp>(op, ctx) {
                if i != 1 {
                    nope!("home: memset byte operand");
                }
                let w = size_align(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx)).0;
                out.entry(op).or_default().push(touch(false, true, base, w));
            } else if let Some(name) = st.intrinsics.get(op).map(|s| s.as_str()) {
                let opnds: Vec<Value> = op.deref(ctx).operands(ctx).collect();
                match name {
                    "llvm.lifetime.start" | "llvm.lifetime.end" => {
                        // A marker on the object itself with its full size
                        // (or no size) ends every byte's value; one on a
                        // field says nothing about the rest.
                        let whole = i == 1
                            && base == 0
                            && const_int(ctx, st, opnds[0]).is_none_or(|n| n < 0 || n as u64 >= size);
                        out.entry(op).or_default().push(Touch {
                            read: false,
                            write: false,
                            lo: 0,
                            hi: 0,
                            kill: whole,
                        });
                    }
                    "llvm.memcpy" | "llvm.memmove" | "llvm.memset" if i <= 1 => {
                        // memset's operand 1 is the byte, not a pointer.
                        if i == 1 && name == "llvm.memset" {
                            nope!("home: unknown intrinsic");
                        }
                        let n = const_int(ctx, st, opnds[2])
                            .and_then(|n| u64::try_from(n).ok())
                            .unwrap_or(size);
                        out.entry(op).or_default().push(touch(i == 1, i == 0, base, n));
                    }
                    _ => return None,
                }
            } else if let Some(c) = Operation::get_op::<CallOp>(op, ctx) {
                let direct = c.get_attr_llvm_call_callee(ctx).is_some();
                if !direct && i == 0 {
                    nope!("home: indirect callee operand");
                }
                let ai = i - usize::from(!direct);
                let info = st.calls.get(op)?;
                // Drop glue on the home (`drop_in_place`, `drop_glue<T>`):
                // a drop retains nothing, and it never reads the bytes a
                // by-value callee wrote. The local was moved into that
                // call (its drop flag is false from there until a new
                // value is stored) or it is `Copy` and has no drop glue; a
                // partially moved struct drops its remaining fields one by
                // one. An inlined drop shows up as plain loads and counts.
                let drop = info
                    .callee
                    .as_deref()
                    .is_some_and(|s| s.contains("4core3ptr9drop_glue") || s.contains("13drop_in_place"));
                let nocap = drop
                    || info.nocap.as_ref().is_some_and(|n| n.get(ai) == Some(&true))
                    || c
                        .get_attr_llvm_call_callee(ctx)
                        .and_then(|id| st.ident_to_sym.get(AsRef::<str>::as_ref(id.as_ref())))
                        .and_then(|sym| st.nocapture.get(sym))
                        .is_some_and(|s| s.contains(&(ai as u32)));
                let (n, _) = info.arg_extent.as_ref().and_then(|e| e.get(ai)).copied()?;
                if !nocap || n == 0 {
                    nope!("home: call operand may retain or has no extent ({:?} #{ai})", info.callee);
                }
                out.entry(op).or_default().push(touch(!drop, !drop, base, n));
            } else {
                nope!("home: address escapes into {}", Operation::get_opid(op, ctx));
            }
        }
    }
    Some(out)
}

/// Bytes `[lo, hi)` of a home alloca are dead after `call`: on every path
/// from it they are fully overwritten, or the object's lifetime ends, or
/// the function leaves, before any read of them. Reads and writes are the
/// alloca's own `users` — nothing else can reach its bytes. A budget bounds
/// the walk; running out means "not shown dead".
fn dead_after(
    ctx: &Context,
    st: &State,
    users: &FxHashMap<Ptr<Operation>, Vec<Touch>>,
    call: Ptr<Operation>,
    lo: u64,
    hi: u64,
) -> Option<bool> {
    const MAX_BLOCKS: usize = 64;
    let mut rewritten = false;
    const MAX_OPS: usize = 4096;
    let mut ops_seen = 0usize;
    let mut visited: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
    // (block, first op to scan; `None` = from the block's top)
    let mut work: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> = Vec::new();
    let Some(cb) = call.deref(ctx).get_parent_block() else {
        return None;
    };
    work.push((cb, call.get_next(ctx)));
    if let Some(&(pad, _)) = st.invokes.get(call) {
        work.push((pad, None));
    }
    while let Some((b, from)) = work.pop() {
        if from.is_none() && !visited.insert(b) {
            continue;
        }
        if visited.len() > MAX_BLOCKS {
            return None;
        }
        let mut cur = match from {
            Some(op) => Some(op),
            None => b.deref(ctx).get_head(),
        };
        let mut stopped = false;
        let mut last = None;
        while let Some(op) = cur {
            ops_seen += 1;
            if ops_seen > MAX_OPS {
                return None;
            }
            last = Some(op);
            if let Some(ts) = users.get(&op) {
                if ts.iter().any(|t| t.read && t.lo < hi && lo < t.hi) {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::memcpyopt", "byval: home read after the call by {} {:?}",
                            Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.clone()));
                    }
                    return None;
                }
                if ts.iter().any(|t| t.kill || (t.write && t.lo <= lo && hi <= t.hi)) {
                    rewritten |= ts.iter().any(|t| t.write && t.lo <= lo && hi <= t.hi);
                    stopped = true;
                    break;
                }
            }
            if let Some(&(pad, _)) = st.invokes.get(op) {
                work.push((pad, None));
            }
            cur = op.get_next(ctx);
        }
        if stopped {
            continue;
        }
        // Fell off the block's end: its terminator's successors.
        if let Some(t) = last {
            for s in t.deref(ctx).successors(ctx) {
                work.push((s, None));
            }
        }
    }
    Some(rewritten)
}

/// `PLIRON_BYVALHOME=1`: `memcpy(%t, %s, n); call f(.., %t, ..)`, where `%t`
/// is a fresh `n`-byte alloca the callee takes by value and never writes
/// (`CallInfo::arg_byval_ro`, rustc's deduced `readonly`), becomes
/// `call f(.., %s, ..)`. `%s` points `off` bytes into an alloca `%a` whose
/// address never leaves the function's view (`home_users`), and no write
/// reaches `%a`'s bytes between the copy and the call, so the callee reads
/// exactly the bytes the copy would have carried and leaves them as they
/// were. `%a` goes into `State::homes` so SROA keeps it one stack object:
/// split, it would scalarize every other access and copy the scalars into
/// a fresh temp at this very call — nine loop-carried SSA values for
/// regex-syntax's `concat = self.push_group(concat)?`.
fn try_byval(ctx: &mut Context, st: &mut State, m: Ptr<Operation>) -> bool {
    if st.volatile.contains(m) || st.intrinsics.get(m).map(|s| s.as_str()) != Some("llvm.memcpy")
    {
        return false;
    }
    let o: Vec<Value> = m.deref(ctx).operands(ctx).collect();
    let (dst, src) = (o[0], o[1]);
    let Some(n) = const_int(ctx, st, o[2]).and_then(|n| u64::try_from(n).ok()) else {
        bail!("copy length not constant");
    };
    let Some(t_op) = dst.defining_op() else {
        return false;
    };
    if n == 0 || !Operation::is_op::<AllocaOp>(t_op, ctx) {
        return false;
    }
    let Some(&(tsize, talign)) = st.allocas.get(dst) else {
        return false;
    };
    if tsize != n || talign == 0 {
        bail!("temp size {tsize} != copy length {n}");
    }
    // `%t`'s uses: this copy as destination, lifetime markers, one call
    // operand.
    let mut call = None;
    let mut markers = Vec::new();
    for u in dst.uses_iter(ctx) {
        let op = u.user_op();
        let i = u.find_index(ctx);
        if op == m {
            if i != 0 {
                bail!("temp is the copy source");
            }
            continue;
        }
        if matches!(
            st.intrinsics.get(op).map(|s| s.as_str()),
            Some("llvm.lifetime.start" | "llvm.lifetime.end")
        ) {
            markers.push(op);
            continue;
        }
        if call.is_some() || !Operation::is_op::<CallOp>(op, ctx) {
            bail!("temp has another use: {}", Operation::get_opid(op, ctx));
        }
        call = Some((op, i));
    }
    let Some((call, idx)) = call else {
        return false;
    };
    let Some(c) = Operation::get_op::<CallOp>(call, ctx) else {
        bail!("temp use is not a call");
    };
    let direct = c.get_attr_llvm_call_callee(ctx).is_some();
    if !direct && idx == 0 {
        bail!("temp is the callee");
    }
    let ai = idx - usize::from(!direct);
    let Some(ci) = st.calls.get(call) else {
        bail!("call without CallInfo");
    };
    let ro = ci.arg_byval_ro.as_ref().is_some_and(|v| v.get(ai) == Some(&true));
    let ext = ci.arg_extent.as_ref().and_then(|e| e.get(ai)).copied();
    if ext.map(|e| e.0) != Some(n) {
        bail!("operand extent {ext:?} is not the copy length {n}");
    }
    let block = m.deref(ctx).get_parent_block().unwrap();
    if call.deref(ctx).get_parent_block() != Some(block) {
        bail!("call in another block");
    }
    let Some((a, off)) = alloca_base(ctx, st, None, src) else {
        bail!("source is not a constant offset into an alloca");
    };
    let Some(&(asize, aalign)) = st.allocas.get(a) else {
        return false;
    };
    if a == dst
        || st.promoted.contains(a)
        || off.checked_add(n).is_none_or(|e| e > asize)
        || aalign < talign
        || off % talign != 0
    {
        bail!("home range/alignment/promotion");
    }
    let Some(users) = home_users(ctx, st, a, asize) else {
        bail!("home escapes");
    };
    // Another operand of the same call reaching into `%a`: the callee
    // could write the bytes it reads.
    if users.contains_key(&call) {
        bail!("another operand of the call reaches the home");
    }
    // No write into `[off, off+n)` between the copy and the call. Ops that
    // don't touch `%a` can't reach its bytes — its address never escaped.
    let mut cur = m.get_next(ctx);
    loop {
        let Some(op) = cur else {
            bail!("block ends before the call");
        };
        if op == call {
            break;
        }
        if let Some(ts) = users.get(&op)
            && ts.iter().any(|t| t.write && t.lo < off + n && off < t.hi)
        {
            bail!("home written between the copy and the call");
        }
        cur = op.get_next(ctx);
    }
    // The callee may write its by-value copy — now `%a`'s bytes — unless
    // rustc deduced the param `readonly`; otherwise those bytes must be
    // dead after the call (MIR moved the local out: nothing reads it
    // before a whole new value is stored).
    // The callee may write its by-value copy — now `%a`'s bytes — unless
    // rustc deduced the param `readonly`; otherwise those bytes must be
    // dead after the call (MIR moved the local out: nothing reads it
    // before a whole new value is stored). Either way the local must get
    // a new value on some path after the call: that is the loop-carried
    // shape this pays for. A temp that is consumed and never rewritten
    // (`let c = make(); consume(c)`) promotes better: its scalars feed
    // the copy directly and the call's is the only copy there is.
    let dead = dead_after(ctx, st, &users, call, off, off + n);
    match dead {
        Some(true) => {}
        Some(false) if n >= 64 => {}
        Some(false) => bail!("home not rewritten after the call"),
        None if ro => bail!("readonly callee, home not shown rewritten"),
        None => bail!("home read after the call before a new value"),
    }
    if pliron_cfg::tracing_on() {
        tracing::debug!(target: "pliron::memcpyopt", "byval: homed n {n} off {off} asize {asize} ro {ro} callee {:?}", ci.callee);
    }
    Operation::replace_operand(call, ctx, idx, src);
    for mk in markers {
        st.intrinsics.remove(mk);
        Operation::erase(mk, ctx);
    }
    st.intrinsics.remove(m);
    Operation::erase(m, ctx);
    st.allocas.remove(dst);
    st.promoted.remove(dst);
    Operation::erase(t_op, ctx);
    st.homes.insert(a);
    true
}

// r[impl iropt.memcpyopt]
pub fn run(ctx: &mut Context, st: &mut State) {
    let mut n = 0;
    // The symbol is only for the trace: the `fwd:` lines that follow a
    // `fn` line belong to that function.
    let trace = pliron_cfg::tracing_on();
    let funcs: Vec<_> = st
        .funcs
        .iter()
        .map(|(name, f)| (f.op, f.exts.params.clone(), trace.then(|| name.clone())))
        .collect();
    for (f, params, name) in funcs {
        if !opt_body(ctx, st, f) {
            continue;
        }
        if let Some(name) = name {
            tracing::debug!(target: "pliron::memcpyopt", "fn {name}");
        }
        let bname = crate::bisect_name(st, f);
        let Some(entry) = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head() else {
            continue;
        };
        let sret_args: Vec<Value> = params
            .iter()
            .enumerate()
            .filter(|(i, e)| **e == ArgExt::SRet && *i < entry.deref(ctx).get_num_arguments())
            .map(|(i, _)| entry.deref(ctx).get_argument(ctx, i))
            .collect();
        let blocks: Vec<_> = f.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx).collect();
        let mut dom = None;
        let mut pin = None;
        for b in blocks {
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            for op in ops {
                // Only candidate copies consume bisect units.
                if matches!(
                    st.intrinsics.get(op).map(|s| s.as_str()),
                    Some("llvm.memcpy" | "llvm.memmove")
                ) && !crate::bisect_in("memcpyopt", &bname)
                {
                    continue;
                }
                n += try_forward(ctx, st, f, &mut dom, &mut pin, &sret_args, op) as usize;
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats","memcpyopt {}: {n} call slots forwarded", st.cgu)
        };
    }
}

/// `PLIRON_BYVALHOME=1`: [`try_byval`] over every function. Runs right
/// before SROA — after `phisimp` has removed the block arguments the
/// inliner bound callee params through (a home alloca's address must not
/// flow through one), and after the `nocapture` inference `home_users`
/// consults for the home's other call operands.
pub fn run_byval(ctx: &mut Context, st: &mut State) {
    let mut n = 0;
    let funcs: Vec<_> = st.funcs.values().map(|f| f.op).collect();
    for f in funcs {
        if !opt_body(ctx, st, f) {
            continue;
        }
        let blocks: Vec<_> = f.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx).collect();
        for b in blocks {
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            for op in ops {
                n += try_byval(ctx, st, op) as usize;
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats","byvalhome {}: {n} by-value args homed", st.cgu)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::{blocks, ops};
    use pliron::builtin::ops::ModuleOp;
    use pliron::irfmt::parsers::spaced;
    use pliron::op::Op;
    use pliron::parsable::parse_from_str;
    use pliron_llvm::ops::{CallIntrinsicOp, FuncOp};

    /// Parse `input`; register the memcpy in `st.intrinsics` and the two
    /// 8-byte allocas in `st.allocas` (builder-side tables the parser
    /// leaves empty). Returns (func, memcpy op).
    fn fixture(input: &str, st: &mut State, ctx: &mut Context) -> (Ptr<Operation>, Ptr<Operation>) {
        let op = parse_from_str(spaced(Operation::top_level_parser()), ctx, input).expect("parse");
        let mop = Operation::get_op::<ModuleOp>(op, ctx).unwrap();
        let f = blocks(ctx, mop.get_operation())
            .into_iter()
            .flat_map(|b| ops(ctx, b))
            .find(|&o| Operation::is_op::<FuncOp>(o, ctx))
            .unwrap();
        let mut m = None;
        for b in blocks(ctx, f) {
            for o in ops(ctx, b) {
                if Operation::is_op::<AllocaOp>(o, ctx) {
                    st.allocas.insert(o.deref(ctx).get_result(ctx, 0), (8, 8));
                }
                if Operation::is_op::<CallIntrinsicOp>(o, ctx) {
                    st.intrinsics.insert(o, "llvm.memcpy".to_string());
                    st.consts.insert(
                        o.deref(ctx).get_operand(ctx, 2),
                        crate::state::ConstVal::Bits(8),
                    );
                    m = Some(o);
                }
            }
        }
        (f, m.unwrap())
    }

    const HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
            flag = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;
            llvm.br ^loop()
          ^loop():
    "#;
    const TAIL: &str = "        llvm.cond_br if flag ^loop() else ^exit()\n          ^exit():\n            llvm.return\n        }\n    }\n";
    const MC: &str = "            m = llvm.call_intrinsic @\"llvm.memcpy\" (dst, tmp, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n";

    /// Loop-carried stale read: the load sits before the writer, so on
    /// iteration 2+ it reads last iteration's store — defined bytes the
    /// rebase would swap for `dst`'s own bytes (the post-copy `dst`
    /// store on the wrap path lands in no window). Must refuse.
    #[test]
    fn stale_pre_copy_read_refused() {
        let input = format!(
            "{HEAD}            r = llvm.load tmp : builtin.integer i64;\n            llvm.store *tmp <- v;\n{MC}            llvm.store *dst <- v;\n{TAIL}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// Same loop with the load after the store: the covering writer
    /// dominates the read, so the rebase hands it the same bytes.
    #[test]
    fn covered_pre_copy_read_forwarded() {
        let input = format!(
            "{HEAD}            llvm.store *tmp <- v;\n            r = llvm.load tmp : builtin.integer i64;\n{MC}            llvm.store *dst <- v;\n{TAIL}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// A partial writer before the loop, the copy inside it, and `dst`
    /// changed after the copy: the next iteration's copy restored
    /// `tmp`'s bytes, the rebased code would keep the changed ones
    /// (regex-syntax `alternate_literals`: a double free). Must refuse.
    #[test]
    fn partial_writer_outside_loop_refused() {
        let pre = "            four = llvm.constant <builtin.integer <3: i32>> : builtin.integer i32;\n            llvm.store *tmp <- four;\n            llvm.br ^loop()\n          ^loop():\n";
        let head = HEAD.replace("            llvm.br ^loop()\n          ^loop():\n", pre);
        assert_ne!(head, HEAD);
        let input = format!("{head}{MC}            llvm.store *dst <- v;\n{TAIL}");
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// Same shape with `dst` only read after the copy: every iteration
    /// sees the writer's bytes either way — still forwarded.
    #[test]
    fn partial_writer_outside_loop_read_only_forwarded() {
        let pre = "            four = llvm.constant <builtin.integer <3: i32>> : builtin.integer i32;\n            llvm.store *tmp <- four;\n            llvm.br ^loop()\n          ^loop():\n";
        let head = HEAD.replace("            llvm.br ^loop()\n          ^loop():\n", pre);
        let input = format!("{head}{MC}            r = llvm.load dst : builtin.integer i64;\n{TAIL}");
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// `HEAD` with a partial writer of `tmp` and the copy ahead of the
    /// loop; the caller supplies the loop body.
    fn copy_before_loop(body: &str) -> String {
        let pre = format!("            four = llvm.constant <builtin.integer <3: i32>> : builtin.integer i32;\n            llvm.store *tmp <- four;\n{MC}            llvm.br ^loop()\n          ^loop():\n");
        let head = HEAD.replace("            llvm.br ^loop()\n          ^loop():\n", &pre);
        assert_ne!(head, HEAD);
        format!("{head}{body}{TAIL}")
    }

    /// A partial writer and the copy before the loop, a `tmp` reader in
    /// it, and `dst` changed after the reader: the reader's second visit
    /// still read the writer's bytes from `tmp`, rebased it would read
    /// what the loop left in `dst` (dsa `emit_shrink`: `to_string`'s
    /// empty buffer became a copy of the `String` the loop appends to,
    /// and the allocator met a block with two owners). Must refuse.
    #[test]
    fn post_copy_reader_in_loop_with_dst_write_refused() {
        let input = copy_before_loop(
            "            r = llvm.load tmp : builtin.integer i64;\n            llvm.store *dst <- v;\n",
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// Same shape with `dst` only read in the loop: every visit of the
    /// reader sees the writer's bytes in `dst` — still forwarded.
    #[test]
    fn post_copy_reader_in_loop_read_only_forwarded() {
        let input = copy_before_loop(
            "            r = llvm.load tmp : builtin.integer i64;\n            r2 = llvm.load dst : builtin.integer i64;\n",
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// The `dst` write sits ahead of the reader in the loop body: the
    /// first visit already reads changed bytes. Must refuse (the window
    /// check, before and after the wrap scan).
    #[test]
    fn post_copy_reader_in_loop_dst_write_first_refused() {
        let input = copy_before_loop(
            "            llvm.store *dst <- v;\n            r = llvm.load tmp : builtin.integer i64;\n",
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// The same loop with a writer that covers all of `tmp`: the route
    /// scan of the full-writer path meets the loop's `dst` write on the
    /// way back from the reader and refuses, with or without the wrap
    /// scan of the partial path.
    #[test]
    fn full_writer_post_copy_reader_in_loop_refused() {
        let pre = format!("            llvm.store *tmp <- v;\n{MC}            llvm.br ^loop()\n          ^loop():\n");
        let head = HEAD.replace("            llvm.br ^loop()\n          ^loop():\n", &pre);
        assert_ne!(head, HEAD);
        let input = format!(
            "{head}            r = llvm.load tmp : builtin.integer i64;\n            llvm.store *dst <- v;\n{TAIL}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    const ESC_HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            cell = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
    "#;

    /// `dst`'s address sits in memory and is reloaded: the load through
    /// the reloaded pointer reads `dst` between the writer and the copy,
    /// where the rebase would already have written it (fuzz seeds
    /// 30621/30696/31403). Must refuse.
    #[test]
    fn escaped_dst_read_in_window_refused() {
        let input = format!(
            "{ESC_HEAD}            llvm.store *cell <- dst;\n            llvm.store *tmp <- v;\n            p = llvm.load cell : llvm.ptr (0);\n            r = llvm.load p : builtin.integer i64;\n{MC}            llvm.return\n        }}\n    }}\n"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// The same loads with `dst`'s address never stored: nothing can
    /// reach `dst` through `p`, so the forward stands.
    #[test]
    fn unescaped_dst_wild_load_forwarded() {
        let input = format!(
            "{ESC_HEAD}            llvm.store *tmp <- v;\n            p = llvm.load cell : llvm.ptr (0);\n            r = llvm.load p : builtin.integer i64;\n{MC}            llvm.return\n        }}\n    }}\n"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    const ARM_HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
            flag = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;
            llvm.cond_br if flag ^a() else ^b()
    "#;

    /// `if c { tmp = f() } else { tmp = g() }; memcpy(dst, tmp)`: no
    /// single writer dominates the copy but each arm does — a writer
    /// cut — so the forward is still exact.
    #[test]
    fn sibling_arm_writers_forwarded() {
        let input = format!(
            "{ARM_HEAD}          ^a():\n            llvm.store *tmp <- v;\n            llvm.br ^join()\n          ^b():\n            llvm.store *tmp <- v;\n            llvm.br ^join()\n          ^join():\n{MC}            llvm.return\n        }}\n    }}\n"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// Same arms but the `else` arm writes `dst` instead of `tmp`: the
    /// route's last `dst` touch isn't a writer — refuse.
    #[test]
    fn armed_dst_touch_refused() {
        let input = format!(
            "{ARM_HEAD}          ^a():\n            llvm.store *tmp <- v;\n            llvm.br ^join()\n          ^b():\n            llvm.store *dst <- v;\n            llvm.br ^join()\n          ^join():\n{MC}            llvm.return\n        }}\n    }}\n"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    const BR_HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
            flag = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;
            llvm.store *dst <- v;
            llvm.store *tmp <- v;
            llvm.cond_br if flag ^a() else ^b()
    "#;

    /// `dst` read in the join, where the `b` route skips the copy: the
    /// writer's rebase into `dst` would reach that read without `m`
    /// ever having run — the read sees `tmp`'s bytes where it saw
    /// `dst`'s own. The first write-touch on the `b` route is the
    /// `tmp` writer, not `m` — refuse.
    #[test]
    fn bypass_dst_read_refused() {
        let input = format!(
            "{BR_HEAD}          ^a():\n{MC}            llvm.br ^join()\n          ^b():\n            llvm.br ^join()\n          ^join():\n            r = llvm.load dst : builtin.integer i64;\n            llvm.return\n        }}\n    }}\n"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// Same copy with the `dst` read inside the copied arm: its only
    /// routes pass `m` itself, so the rebase is invisible to it —
    /// forward.
    #[test]
    fn window_dst_read_forwarded() {
        let input = format!(
            "{BR_HEAD}          ^a():\n{MC}            r = llvm.load dst : builtin.integer i64;\n            llvm.br ^join()\n          ^b():\n            llvm.br ^join()\n          ^join():\n            llvm.return\n        }}\n    }}\n"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
    }

    /// `entry -> a -> a2 -> join`, `entry -> b -> join`; `tmp` is written
    /// in `a`, `dst` in `b`, the copy sits in `a` or in `a2`.
    fn window_fixture(copy_in_a2: bool) -> (Context, State, Ptr<Operation>, Ptr<Operation>, Ptr<Operation>, Ptr<Operation>) {
        let (in_a, in_a2) = if copy_in_a2 { ("", MC) } else { (MC, "") };
        let input = format!(
            "{ESC_HEAD}            flag = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;\n            llvm.cond_br if flag ^a() else ^b()\n          ^a():\n            llvm.store *tmp <- v;\n{in_a}            llvm.br ^a2()\n          ^a2():\n{in_a2}            llvm.br ^join()\n          ^b():\n            llvm.store *dst <- v;\n            llvm.br ^join()\n          ^join():\n            llvm.return\n        }}\n    }}\n"
        );
        let mut ctx = Context::new();
        let mut st = State::default();
        let (f, m) = fixture(&input, &mut st, &mut ctx);
        let stores: Vec<Ptr<Operation>> = blocks(&ctx, f)
            .into_iter()
            .flat_map(|b| ops(&ctx, b))
            .filter(|&o| Operation::is_op::<StoreOp>(o, &ctx))
            .collect();
        // Layout order: the `tmp` store in `a`, then the `dst` store in `b`.
        (ctx, st, f, m, stores[0], stores[1])
    }

    /// A `dst` write in a sibling arm is not between a writer and a copy
    /// that share a block, nor between two blocks of the other arm: no
    /// path from the writer to the copy passes it. The dominator test
    /// alone says it is (every `return` site of the dsa interpreter's
    /// `step_one` kept its 160-byte copy into `sret` for that).
    #[test]
    fn sibling_arm_touch_is_not_in_the_window() {
        for copy_in_a2 in [false, true] {
            let (ctx, st, f, m, w, sib) = window_fixture(copy_in_a2);
            let set: FxHashSet<Ptr<Operation>> = [sib].into_iter().collect();
            assert!(any_between_with(&ctx, &st, &mut None, f, m, w, &set, false));
            assert!(!any_between_with(&ctx, &st, &mut None, f, m, w, &set, true), "copy_in_a2={copy_in_a2}");
        }
    }

    /// A touch on the path from the writer's block to the copy's block
    /// stays in the window.
    #[test]
    fn touch_on_the_path_stays_in_the_window() {
        let input = format!(
            "{ESC_HEAD}            llvm.store *tmp <- v;\n            llvm.br ^mid()\n          ^mid():\n            llvm.store *dst <- v;\n            llvm.br ^tail()\n          ^tail():\n{MC}            llvm.return\n        }}\n    }}\n"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        let stores: Vec<Ptr<Operation>> = blocks(ctx, f)
            .into_iter()
            .flat_map(|b| ops(ctx, b))
            .filter(|&o| Operation::is_op::<StoreOp>(o, ctx))
            .collect();
        let set: FxHashSet<Ptr<Operation>> = [stores[1]].into_iter().collect();
        assert!(any_between_with(ctx, st, &mut None, f, m, stores[0], &set, true));
    }

    const LT_HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
    "#;
    const LT: &str = "            lt = llvm.call_intrinsic @\"llvm.lifetime.start\" (eight, dst) : llvm.func <llvm.void (builtin.integer i64, llvm.ptr (0)) variadic = false>;\n";
    const LT_TAIL: &str = "            r = llvm.load dst : builtin.integer i64;\n            llvm.return\n        }\n    }\n";

    /// Forward the copy in `body`, with its two-operand intrinsic
    /// registered as `dst`'s `llvm.lifetime.start`; returns how many of
    /// `dst`'s markers are left.
    fn dst_markers_after_forward(body: &str) -> usize {
        let (mut ctx, mut st, f, m) = lt_fixture(body);
        assert!(try_forward(&mut ctx, &mut st, f, &mut None, &mut None, &[], m));
        st.intrinsics
            .iter()
            .filter(|(_, n)| n.as_str() == "llvm.lifetime.start")
            .count()
    }

    /// `LT_HEAD` + `body` + `LT_TAIL` parsed, allocas and intrinsics
    /// registered; returns the function and the copy.
    fn lt_fixture(body: &str) -> (Context, State, Ptr<Operation>, Ptr<Operation>) {
        let input = format!("{LT_HEAD}{body}{LT_TAIL}");
        let mut ctx = Context::new();
        let mut st = State::default();
        let op = parse_from_str(spaced(Operation::top_level_parser()), &mut ctx, &input).expect("parse");
        let mop = Operation::get_op::<ModuleOp>(op, &ctx).unwrap();
        let f = blocks(&ctx, mop.get_operation())
            .into_iter()
            .flat_map(|b| ops(&ctx, b))
            .find(|&o| Operation::is_op::<FuncOp>(o, &ctx))
            .unwrap();
        let mut m = None;
        for b in blocks(&ctx, f) {
            for o in ops(&ctx, b) {
                if Operation::is_op::<AllocaOp>(o, &ctx) {
                    st.allocas.insert(o.deref(&ctx).get_result(&ctx, 0), (8, 8));
                }
                if Operation::is_op::<CallIntrinsicOp>(o, &ctx) {
                    if o.deref(&ctx).get_num_operands() == 2 {
                        st.intrinsics.insert(o, "llvm.lifetime.start".to_string());
                    } else {
                        st.intrinsics.insert(o, "llvm.memcpy".to_string());
                        st.consts.insert(
                            o.deref(&ctx).get_operand(&ctx, 2),
                            crate::state::ConstVal::Bits(8),
                        );
                        m = Some(o);
                    }
                }
            }
        }
        (ctx, st, f, m.unwrap())
    }

    /// `store tmp; lifetime.start(dst); memcpy(dst, tmp)`: the forwarded
    /// store lands in `dst` ahead of `dst`'s start marker, which would
    /// then call the stored bytes dead (slotmerge reused them: mimas
    /// `install_assoc`). The marker has to go with the copy.
    #[test]
    fn dst_marker_between_writer_and_copy_dropped() {
        let n = dst_markers_after_forward(&format!("            llvm.store *tmp <- v;\n{LT}{MC}"));
        assert_eq!(n, 0, "dst's lifetime.start still sits after the forwarded write");
    }

    /// The same with the writer in an earlier block and the marker at
    /// the head of the copy's block: the shape of the two dsa corpus
    /// functions recorded before the marker scan existed
    /// (`DragSession::drop`: 12 bytes stored to `dst` in one block,
    /// `lifetime.start(dst)` heading the block that reads them back;
    /// bincode's `next_element_seed`: three field stores and a 240-byte
    /// copy ahead of the tuple's marker). slotmerge gave the bytes to
    /// another slot there and the CLIF checker reported it.
    #[test]
    fn dst_marker_heading_the_copy_block_dropped() {
        let n = dst_markers_after_forward(&format!(
            "            llvm.store *tmp <- v;\n            llvm.br ^tail()\n          ^tail():\n{LT}{MC}"
        ));
        assert_eq!(n, 0, "dst's lifetime.start still sits between the forwarded write and its read");
    }

    /// `PLIRON_MEMCPYOPT_MOVEMARK`: in one block with nothing touching
    /// `dst` on the way, the marker moves ahead of the writer instead: it
    /// is kept, and it now precedes the forwarded store.
    #[test]
    fn dst_marker_moves_ahead_of_the_writer() {
        TEST_MOVEMARK.with(|c| c.set(true));
        let (mut ctx, mut st, f, m) = lt_fixture(&format!("            llvm.store *tmp <- v;\n{LT}{MC}"));
        let ok = try_forward(&mut ctx, &mut st, f, &mut None, &mut None, &[], m);
        TEST_MOVEMARK.with(|c| c.set(false));
        assert!(ok);
        let all: Vec<Ptr<Operation>> = blocks(&ctx, f).into_iter().flat_map(|b| ops(&ctx, b)).collect();
        let mk = all.iter().position(|o| st.intrinsics.get(*o).is_some_and(|n| n == "llvm.lifetime.start"));
        let w = all.iter().position(|&o| Operation::is_op::<StoreOp>(o, &ctx)).unwrap();
        assert!(mk.is_some_and(|k| k < w), "start marker dropped or still behind the forwarded store");
    }

    /// A read of `dst` between the writer and the marker sees bytes the
    /// marker has not killed yet; so does a second marker. Neither may
    /// end up behind the moved marker.
    #[test]
    fn dst_marker_stays_behind_a_dst_touch() {
        let lt2 = LT.replace("lt = ", "lt2 = ");
        for mid in ["            old = llvm.load dst : builtin.integer i64;\n", lt2.as_str()] {
            let (mut ctx, st, f, _m) = lt_fixture(&format!("            llvm.store *tmp <- v;\n{mid}{LT}{MC}"));
            let all: Vec<Ptr<Operation>> = blocks(&ctx, f).into_iter().flat_map(|b| ops(&ctx, b)).collect();
            let w = *all.iter().find(|&&o| Operation::is_op::<StoreOp>(o, &ctx)).unwrap();
            let marks: Vec<Ptr<Operation>> =
                all.iter().copied().filter(|o| st.intrinsics.get(*o).is_some_and(|n| n == "llvm.lifetime.start")).collect();
            let mk = *marks.last().unwrap();
            let dbase = mk.deref(&ctx).get_operand(&ctx, 1);
            let wset: FxHashSet<Ptr<Operation>> = [w].into_iter().collect();
            let dmarks: FxHashSet<Ptr<Operation>> = marks.iter().copied().collect();
            assert!(!move_start_marker(&mut ctx, &st, dbase, 8, mk, &wset, &dmarks), "moved over {mid}");
        }
    }

    /// The marker ahead of the writer still bounds `dst` correctly.
    #[test]
    fn dst_marker_before_writer_kept() {
        let n = dst_markers_after_forward(&format!("{LT}            llvm.store *tmp <- v;\n{MC}"));
        assert_eq!(n, 1);
    }

    // ---- `PLIRON_MEMCPYOPT_NOALIASDST`: the copy goes to `*p`, `p` a
    // `noalias` (`&mut [u8; 8]`) entry parameter.

    const NA_HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void (llvm.ptr (0)) variadic = false> [] {
          ^entry(p: llvm.ptr (0)):
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
            flag = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;
    "#;
    const NA_TAIL: &str = "        };\n          llvm.func @g: llvm.func <llvm.void () variadic = false> []\n    }\n";
    const NA_MC: &str = "            m = llvm.call_intrinsic @\"llvm.memcpy\" (p, tmp, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n";

    /// Parse a `NA_HEAD` body, mark `p` `noalias` (`deref` bytes,
    /// `writable`) and try the forward with the knob on or off.
    fn na_forward(body: &str, on: bool, deref: u64, writable: bool) -> bool {
        na_forward_in(body, on, deref, writable, false)
    }

    /// `na_forward` with `State::panic_abort` set as given.
    fn na_forward_in(body: &str, on: bool, deref: u64, writable: bool, panic_abort: bool) -> bool {
        let input = format!("{NA_HEAD}{body}{NA_TAIL}");
        let ctx = &mut Context::new();
        let st = &mut State { panic_abort, ..State::default() };
        let (f, m) = fixture(&input, st, ctx);
        let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head().unwrap();
        let p = entry.deref(ctx).get_argument(ctx, 0);
        st.noalias.insert(p, (deref, writable));
        TEST_NOALIASDST.with(|c| c.set(on));
        let ok = try_forward(ctx, st, f, &mut None, &mut None, &[], m);
        TEST_NOALIASDST.with(|c| c.set(false));
        ok
    }

    /// `*p = { tmp.x = 7; tmp }`: forwarded into the parameter's pointee
    /// with the knob, refused without it, and refused when `p` is
    /// `readonly` or not known dereferenceable over the copy.
    #[test]
    fn noalias_param_dst_forwarded() {
        let body = format!("            llvm.store *tmp <- v;\n{NA_MC}            llvm.return\n");
        assert!(na_forward(&body, true, 8, true));
        assert!(!na_forward(&body, false, 8, true));
        assert!(!na_forward(&body, true, 8, false));
        assert!(!na_forward(&body, true, 4, true));
    }

    /// A call that may unwind between the redirected writer and the copy:
    /// the caller's landing pad would see `*p` already written. Refuse.
    #[test]
    fn noalias_param_dst_unwind_between_refused() {
        let body = format!(
            "            llvm.store *tmp <- v;\n            c = llvm.call @g () : llvm.func <llvm.void () variadic = false>;\n{NA_MC}            llvm.return\n"
        );
        assert!(!na_forward(&body, true, 8, true));
        // Under `panic=abort` the call cannot unwind: forwarded.
        assert!(na_forward_in(&body, true, 8, true, true));
    }

    /// A path from the writer that returns without reaching the copy: the
    /// caller would see `*p` written where it was not. Refuse.
    #[test]
    fn noalias_param_dst_return_between_refused() {
        let body = format!(
            "            llvm.store *tmp <- v;\n            llvm.cond_br if flag ^out() else ^c()\n          ^out():\n            llvm.return\n          ^c():\n{NA_MC}            llvm.return\n"
        );
        assert!(!na_forward(&body, true, 8, true));
    }

    /// `*p` read between the writer and the copy sees the old bytes;
    /// redirected, it would see the new ones. Refuse.
    #[test]
    fn noalias_param_dst_read_between_refused() {
        let body = format!(
            "            llvm.store *tmp <- v;\n            old = llvm.load p : builtin.integer i64;\n{NA_MC}            llvm.return\n"
        );
        assert!(!na_forward(&body, true, 8, true));
    }

    /// The `noalias` of an inlined callee's parameter (a clone-entry block
    /// argument, `region_noalias`) holds for the inlined body only: not a
    /// destination, even with an `st.noalias` entry claiming it.
    #[test]
    fn scoped_noalias_dst_refused() {
        let input = format!(
            "{NA_HEAD}            llvm.br ^clone(p)\n          ^clone(q: llvm.ptr (0)):\n            llvm.store *tmp <- v;\n            m = llvm.call_intrinsic @\"llvm.memcpy\" (q, tmp, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n            llvm.return\n{NA_TAIL}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        let q = m.deref(ctx).get_operand(ctx, 0);
        st.noalias.insert(q, (8, true));
        st.region_noalias.insert(q, vec![0]);
        TEST_NOALIASDST.with(|c| c.set(true));
        let ok = try_forward(ctx, st, f, &mut None, &mut None, &[], m);
        TEST_NOALIASDST.with(|c| c.set(false));
        assert!(!ok);
    }

    // ---- `PLIRON_MEMCPYOPT_SIBARMS`: a 16-byte `tmp` written in pieces
    // (`tmp` = bytes 0..8, `tmp8` = bytes 8..16) by writers that do not
    // all dominate the copy.

    const SIB_HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            sixteen = llvm.constant <builtin.integer <16: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x sixteen] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x sixteen] : llvm.ptr (0);
            tmp8 = llvm.gep <builtin.integer i8> (tmp, eight)[OperandIdx(1)] : llvm.ptr (0);
            dst8 = llvm.gep <builtin.integer i8> (dst, eight)[OperandIdx(1)] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
            w = llvm.constant <builtin.integer <9: i64>> : builtin.integer i64;
            flag = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;
    "#;
    const SIB_MC: &str = "            m = llvm.call_intrinsic @\"llvm.memcpy\" (dst, tmp, sixteen) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n";

    /// Parse a `SIB_HEAD` body (16-byte allocas, constant GEP indices) and
    /// try the forward with `PLIRON_MEMCPYOPT_SIBARMS` on or off.
    fn sib_forward(body: &str, on: bool) -> bool {
        let input = format!("{SIB_HEAD}{body}            llvm.return\n        }}\n    }}\n");
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        for b in blocks(ctx, f) {
            for o in ops(ctx, b) {
                if Operation::is_op::<AllocaOp>(o, ctx) {
                    st.allocas.insert(o.deref(ctx).get_result(ctx, 0), (16, 8));
                }
                if Operation::is_op::<GetElementPtrOp>(o, ctx) {
                    st.consts.insert(o.deref(ctx).get_operand(ctx, 1), crate::state::ConstVal::Bits(8));
                }
            }
        }
        st.consts.insert(m.deref(ctx).get_operand(ctx, 2), crate::state::ConstVal::Bits(16));
        TEST_SIBARMS.with(|c| c.set(Some(on)));
        let ok = try_forward(ctx, st, f, &mut None, &mut None, &[], m);
        TEST_SIBARMS.with(|c| c.set(None));
        ok
    }

    /// `if c { tmp.a = 7 } else { tmp.a = 9 }; tmp.b = 7; dst = tmp`:
    /// every route into the copy covers both halves. Forwarded with the
    /// knob, refused without it (no single writer dominates). An arm that
    /// leaves `tmp.a` unwritten forwards too: on that route the copied
    /// bytes are the alloca's undef, and `dst`'s own bytes refine them.
    #[test]
    fn sibarms_partial_arms_forwarded() {
        let body = format!(
            "            llvm.cond_br if flag ^a() else ^b()\n          ^a():\n            llvm.store *tmp <- v;\n            llvm.br ^join()\n          ^b():\n            llvm.store *tmp <- w;\n            llvm.br ^join()\n          ^join():\n            llvm.store *tmp8 <- v;\n{SIB_MC}"
        );
        assert!(sib_forward(&body, true));
        assert!(!sib_forward(&body, false));
        let undef_arm = format!(
            "            llvm.cond_br if flag ^a() else ^b()\n          ^a():\n            llvm.store *tmp <- v;\n            llvm.br ^join()\n          ^b():\n            llvm.br ^join()\n          ^join():\n            llvm.store *tmp8 <- v;\n{SIB_MC}"
        );
        assert!(sib_forward(&undef_arm, true));
    }

    /// `tmp.a = 7` before the branch; the `else` arm writes `dst.a` and
    /// then only `tmp.b`. On that route `dst.a` must end up 7 (the entry
    /// writer, copied by `m`); rebased, the entry writer's 7 would land in
    /// `dst` before the arm's `dst` store overwrote it. The scan reaches
    /// that store with `tmp.a`'s bytes still uncovered: refuse. (Cutting
    /// the route at the first writer, as for full-range writers, would
    /// accept it.) With the arm writing `tmp.a` after its `dst` store the
    /// route is covered first, and the forward stands.
    #[test]
    fn sibarms_partial_arm_refused() {
        let arms = |b: &str| {
            format!(
                "            llvm.store *tmp <- v;\n            llvm.cond_br if flag ^a() else ^b()\n          ^a():\n            llvm.store *tmp <- w;\n            llvm.store *tmp8 <- w;\n            llvm.br ^join()\n          ^b():\n{b}            llvm.br ^join()\n          ^join():\n{SIB_MC}"
            )
        };
        let part = arms("            llvm.store *dst <- w;\n            llvm.store *tmp8 <- v;\n");
        assert!(!sib_forward(&part, true));
        let whole = arms("            llvm.store *dst <- w;\n            llvm.store *tmp <- v;\n            llvm.store *tmp8 <- v;\n");
        assert!(sib_forward(&whole, true));
    }

    /// A route that reads `dst` between a writer and the copy would see
    /// the writer's bytes early once they land in `dst`. Refuse.
    #[test]
    fn sibarms_dst_read_between_refused() {
        let body = format!(
            "            llvm.cond_br if flag ^a() else ^b()\n          ^a():\n            llvm.store *tmp <- v;\n            llvm.br ^join()\n          ^b():\n            llvm.store *tmp <- w;\n            r = llvm.load dst : builtin.integer i64;\n            llvm.br ^join()\n          ^join():\n            llvm.store *tmp8 <- v;\n{SIB_MC}"
        );
        assert!(!sib_forward(&body, true));
    }

    /// The read in the window is of the half no writer on its route has
    /// written yet (`dst.b`, before `tmp.b`'s writer), but it is still
    /// between a writer (`tmp.a`) and the copy on the same route. Refuse.
    #[test]
    fn sibarms_dst_other_half_read_refused() {
        let body = format!(
            "            llvm.cond_br if flag ^a() else ^b()\n          ^a():\n            llvm.store *tmp <- v;\n            llvm.br ^join()\n          ^b():\n            llvm.store *tmp <- w;\n            llvm.br ^join()\n          ^join():\n            r = llvm.load dst8 : builtin.integer i64;\n            llvm.store *tmp8 <- v;\n{SIB_MC}"
        );
        assert!(!sib_forward(&body, true));
    }
}

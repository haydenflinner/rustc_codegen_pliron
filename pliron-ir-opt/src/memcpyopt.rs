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
    /// `Some(on)` overrides `PLIRON_MEMCPYOPT_NOPCOPY` (tests).
    static TEST_NOPCOPY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

fn movemark() -> bool {
    #[cfg(test)]
    if TEST_MOVEMARK.with(|c| c.get()) {
        return true;
    }
    crate::env_is("PLIRON_MEMCPYOPT_MOVEMARK", "1")
}

/// `PLIRON_MEMCPYOPT_NOPCOPY=1` (opt-in): erase `memcpy`/`memmove` ops
/// whose dst and src resolve to the same `(base, offset)` — a copy onto
/// itself is a no-op. MIR identity moves and block-arg forwarding both
/// leave them (`phisimp` binding `memcpy(p <- a)`'s `p` to `a`), and one
/// surviving copy keeps its alloca out of `sroa` (`accesses` refuses a
/// self-copy outright, so the whole temp — e.g. an `Option<enum>` the
/// caller only discriminates — round-trips through the stack).
fn nopcopy() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_NOPCOPY.with(|c| c.get()) {
        return on;
    }
    crate::env_is("PLIRON_MEMCPYOPT_NOPCOPY", "1")
}

/// `v` as `(base, byte-offset)` with constant-offset GEPs peeled, `None`
/// past 16 links. Base may be any value — an alloca, an entry param, a
/// block argument; only the pair equality matters to the callers.
fn ptr_base(ctx: &Context, st: &State, mut v: Value) -> Option<(Value, i64)> {
    let mut off = 0i64;
    for _ in 0..16 {
        match v.defining_op() {
            Some(g) if Operation::is_op::<GetElementPtrOp>(g, ctx) => {
                off = off.checked_add(gep_offset(ctx, st, g)?)?;
                v = g.deref(ctx).get_operand(ctx, 0);
            }
            _ => return Some((v, off)),
        }
    }
    None
}

/// Erase `m`, a `memcpy`/`memmove` with identical ends. `llvm.memcpy`
/// forbids any other overlap, so equal pointers are the only sound
/// aliasing case; `memmove` self-copies are no-ops the same way.
fn kill_nopcopy(ctx: &mut Context, st: &mut State, m: Ptr<Operation>) -> bool {
    let dst = m.deref(ctx).get_operand(ctx, 0);
    let src = m.deref(ctx).get_operand(ctx, 1);
    // `None` pairs must not compare equal: two unresolvable pointers are
    // not known-identical.
    match (ptr_base(ctx, st, dst), ptr_base(ctx, st, src)) {
        (Some(d), Some(s)) if d == s => {}
        _ => return false,
    }
    st.intrinsics.remove(m);
    Operation::erase(m, ctx);
    true
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
    if nopcopy() && kill_nopcopy(ctx, st, m) {
        return true;
    }
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
        // `PLIRON_MEMCPYOPT_SOLECOPY=1` (opt-in): a `tmp` whose only reader
        // is this copy is not protected (promotion would leave its scalars
        // to be stored into `dst` by the copy anyway). Exact counts: html
        // -0.16%, blind-sig -0.12%, but pulldown-cmark +0.15% (`parse_block`:
        // the changed frame moves which values spill). Requiring writers in
        // the copy's block or writing disjoint bytes kept pulldown's loss
        // and lost html's win.
        // lol_html's selector VM built each 144-byte `StackItem` in a temp,
        // copied it into a second temp and pushed that: three `memcpy`
        // calls per element where LLVM has none.
        let solecopy = readers.is_empty() && pliron_cfg::env_is("PLIRON_MEMCPYOPT_SOLECOPY", "1");
        let tmp_esc = solecopy
            || writers
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
    drop_writes: bool,
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
                let Some((n, _)) = info.arg_extent.as_ref().and_then(|e| e.get(ai)).copied() else {
                    nope!("home: call operand has no extent ({:?} #{ai})", info.callee);
                };
                if !nocap || n == 0 {
                    nope!("home: call operand may retain (nocap {nocap}) or has no extent ({n}) ({:?} #{ai})", info.callee);
                }
                let rw = !drop || drop_writes;
                out.entry(op).or_default().push(touch(rw, rw, base, n));
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
    let Some(users) = home_users(ctx, st, a, asize, false) else {
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

    /// `PLIRON_SRCFWD_ALLOCA`: `tmp` filled by `memcpy(tmp, src, 8)` —
    /// the srcfwd temp/source direction, not `try_forward`'s. `body`
    /// goes inside `^loop` (HEAD's `dst` alloca is renamed `src`); runs
    /// `srcfwd_one` on `tmp` with the knob forced `on`.
    const SFMC: &str = "            m = llvm.call_intrinsic @\"llvm.memcpy\" (tmp, src, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n";
    fn srcfwd_alloca_try(input: &str, on: bool) -> bool {
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, _m) = fixture(input, st, ctx);
        let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head().unwrap();
        // First alloca is `tmp` (entry order), the temp to forward.
        let tmp = blocks(ctx, f)
            .into_iter()
            .flat_map(|b| ops(ctx, b))
            .find(|&o| Operation::is_op::<AllocaOp>(o, ctx))
            .map(|o| o.deref(ctx).get_result(ctx, 0))
            .unwrap();
        TEST_ALLOCA.with(|c| c.set(Some(on)));
        let ok = srcfwd_one(ctx, st, entry, f, tmp, &mut None, &mut None, None, "");
        TEST_ALLOCA.with(|c| c.set(None));
        ok
    }

    /// `src`'s store sits before the fill-copy inside the loop: it is a
    /// write of the source reachable after the copy (the next
    /// iteration's), but every path from it to the read crosses a fill —
    /// the copy barrier clears it and `tmp`'s reads rebase onto `src`.
    /// Off by default: refused without `PLIRON_SRCFWD_ALLOCA`.
    #[test]
    fn srcfwd_alloca_loop_fill_forwarded() {
        let input = format!(
            "{HEAD}            llvm.store *src <- v;\n{SFMC}            r = llvm.load tmp : builtin.integer i64;\n{TAIL}"
        )
        .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(srcfwd_alloca_try(&input, true));
        assert!(!srcfwd_alloca_try(&input, false));
    }

    /// The same shape with the source written *between* the copy and the
    /// read on the same iteration: no fill stands between them, the
    /// forwarded read would see the new bytes where the temp kept the
    /// old. Must refuse.
    #[test]
    fn srcfwd_alloca_write_between_refused() {
        let input = format!(
            "{HEAD}{SFMC}            llvm.store *src <- v;\n            r = llvm.load tmp : builtin.integer i64;\n{TAIL}"
        )
        .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(!srcfwd_alloca_try(&input, true));
    }

    /// `src` allocated inside the loop with `tmp` read in `entry`: the
    /// rebased read would use `src` on a path where it is not defined.
    /// Must refuse.
    #[test]
    fn srcfwd_alloca_non_dominating_refused() {
        // Drop `dst` from entry; `src`'s alloca sits inside `^loop`
        // instead, and `tmp` is read in `entry` — `src` cannot dominate
        // that use.
        let input = HEAD.replace("            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);\n", "            r = llvm.load tmp : builtin.integer i64;\n");
        assert_ne!(input, HEAD);
        let input = format!(
            "{input}            src = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);\n            llvm.store *src <- v;\n{SFMC}{TAIL}"
        );
        assert!(!srcfwd_alloca_try(&input, true));
    }

    /// `PLIRON_SRCFWD_BPSRC` fixture: `sv` is `^loop`'s param — `dst` on
    /// the `^a` route and on the backedge, `dst2` on `^b` — so `sv`
    /// names a different alloca's window per traversal. `BPM` is the
    /// `memcpy(tmp, sv, eight)` fill; `BPT` the loop's tail.
    const BPH: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst2 = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            v = llvm.constant <builtin.integer <7: i64>> : builtin.integer i64;
            flag = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;
            llvm.cond_br if flag ^a() else ^b()
          ^a():
            llvm.br ^loop(dst)
          ^b():
            llvm.br ^loop(dst2)
          ^loop(sv: llvm.ptr (0)):
    "#;
    const BPM: &str = "            m = llvm.call_intrinsic @\"llvm.memcpy\" (tmp, sv, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n";
    const BPT: &str = "        llvm.cond_br if flag ^loop(dst) else ^exit()\n          ^exit():\n            llvm.return\n        }\n    }\n";

    fn srcfwd_bpsrc_try(input: &str, on: bool) -> bool {
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, _m) = fixture(input, st, ctx);
        let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head().unwrap();
        // First alloca is `tmp` (entry order), the temp to forward.
        let tmp = blocks(ctx, f)
            .into_iter()
            .flat_map(|b| ops(ctx, b))
            .find(|&o| Operation::is_op::<AllocaOp>(o, ctx))
            .map(|o| o.deref(ctx).get_result(ctx, 0))
            .unwrap();
        TEST_BPSRC.with(|c| c.set(Some(on)));
        let ok = srcfwd_one(ctx, st, entry, f, tmp, &mut None, &mut None, None, "");
        TEST_BPSRC.with(|c| c.set(None));
        ok
    }

    /// `sv` carries `dst` or `dst2` by route; the fill is `^loop`'s head
    /// op, so every surviving use sees this traversal's window. The
    /// temp's uses rebase onto `sv` itself. Off by default.
    #[test]
    fn srcfwd_bpsrc_loop_carried_forwarded() {
        let input = format!("{BPH}{BPM}            r = llvm.load tmp : builtin.integer i64;\n{BPT}");
        assert!(srcfwd_bpsrc_try(&input, true));
        assert!(!srcfwd_bpsrc_try(&input, false));
    }

    /// The same loop with `tmp` read *before* the fill: that read holds
    /// the previous traversal's window (uninitialized on the first), and
    /// the rebase would hand it `sv`'s new one. Must refuse.
    #[test]
    fn srcfwd_bpsrc_unguarded_use_refused() {
        let input = format!("{BPH}            r = llvm.load tmp : builtin.integer i64;\n{BPM}{BPT}");
        assert!(!srcfwd_bpsrc_try(&input, true));
    }

    /// `sv` is still the loop param but the fill sits on one arm only:
    /// the `^use` route reaches the load having crossed no fill since
    /// `sv` moved. Must refuse.
    #[test]
    fn srcfwd_bpsrc_arm_unguarded_refused() {
        let input = format!(
            "{BPH}            llvm.cond_br if flag ^fill() else ^use()\n          ^fill():\n{BPM}            llvm.br ^join()\n          ^use():\n            r = llvm.load tmp : builtin.integer i64;\n            llvm.br ^join()\n          ^join():\n{BPT}"
        );
        assert!(!srcfwd_bpsrc_try(&input, true));
    }

    /// `tmp` read in `^a`, which `sv`'s block does not dominate: the
    /// rebase pointer isn't defined there. Must refuse.
    #[test]
    fn srcfwd_bpsrc_non_dominating_refused() {
        let input = format!("{BPH}{BPM}{BPT}").replace(
            "llvm.br ^loop(dst)",
            "r = llvm.load tmp : builtin.integer i64;\n            llvm.br ^loop(dst)",
        );
        assert!(!srcfwd_bpsrc_try(&input, true));
    }

    /// A write of `dst` — one of `sv`'s windows — between the fill and
    /// the read: the rebase would show the new bytes where the temp kept
    /// the old. Must refuse.
    #[test]
    fn srcfwd_bpsrc_write_between_refused() {
        let input = format!(
            "{BPH}{BPM}            llvm.store *dst <- v;\n            r = llvm.load tmp : builtin.integer i64;\n{BPT}"
        );
        assert!(!srcfwd_bpsrc_try(&input, true));
    }

    // ---- `PLIRON_SRCFWD_WSTORE`: `store`s *into* the temp rebase onto
    // the source, write-side. `WEXIT` is a `^loop`-exit edge whose block
    // carries the store; `WRET` closes it and the function.

    const WEXIT: &str = "        llvm.cond_br if flag ^loop() else ^exit()\n          ^exit():\n";
    const WRET: &str = "            llvm.return\n        }\n    }\n";

    /// The `srcfwd_alloca` shape (`src` a local alloca) with the ALLOCA
    /// and WSTORE knobs forced.
    fn srcfwd_wstore_try(input: &str, on: bool) -> bool {
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, _m) = fixture(input, st, ctx);
        let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head().unwrap();
        let tmp = blocks(ctx, f)
            .into_iter()
            .flat_map(|b| ops(ctx, b))
            .find(|&o| Operation::is_op::<AllocaOp>(o, ctx))
            .map(|o| o.deref(ctx).get_result(ctx, 0))
            .unwrap();
        TEST_ALLOCA.with(|c| c.set(Some(true)));
        TEST_WSTORE.with(|c| c.set(Some(on)));
        let ok = srcfwd_one(ctx, st, entry, f, tmp, &mut None, &mut None, None, "");
        TEST_WSTORE.with(|c| c.set(None));
        TEST_ALLOCA.with(|c| c.set(None));
        ok
    }

    /// `tmp` filled in `^loop`; the store into `tmp` is in the exit
    /// block, so no fill can run after it and nothing reads `src` again —
    /// rebased it writes `src` directly. Off by default.
    #[test]
    fn srcfwd_wstore_after_loop_forwarded() {
        let input = format!(
            "{HEAD}{SFMC}{WEXIT}            llvm.store *tmp <- v;\n            r = llvm.load tmp : builtin.integer i64;\n{WRET}"
        )
        .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(srcfwd_wstore_try(&input, true));
        assert!(!srcfwd_wstore_try(&input, false));
    }

    /// `src` read after the store: the rebased store landed its bytes on
    /// `src` early — the read would see them where the program kept them
    /// in the private temp. Must refuse.
    #[test]
    fn srcfwd_wstore_src_read_after_refused() {
        let input = format!(
            "{HEAD}{SFMC}{WEXIT}            llvm.store *tmp <- v;\n            r = llvm.load src : builtin.integer i64;\n{WRET}"
        )
        .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(!srcfwd_wstore_try(&input, true));
    }

    /// ... but a write covering the stored range between the store and
    /// the read is what the read saw either way: forwarded.
    #[test]
    fn srcfwd_wstore_covering_write_forwarded() {
        let input = format!(
            "{HEAD}{SFMC}{WEXIT}            llvm.store *tmp <- v;\n            llvm.store *src <- v;\n            r = llvm.load src : builtin.integer i64;\n{WRET}"
        )
        .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(srcfwd_wstore_try(&input, true));
    }

    /// The store into `tmp` sits inside the loop before the wrap edge:
    /// the next iteration's fill would restore `src`'s bytes — the
    /// erased copy cannot, so the rebased reads would keep the stored
    /// value. Must refuse.
    #[test]
    fn srcfwd_wstore_refill_refused() {
        let input = format!("{HEAD}{SFMC}            llvm.store *tmp <- v;\n{TAIL}")
            .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(!srcfwd_wstore_try(&input, true));
    }

    /// `src` written in the loop before the exit-edge store into `tmp`:
    /// the write reaches the store without crossing a fill — the rebased
    /// store would clobber it early. Must refuse.
    #[test]
    fn srcfwd_wstore_src_write_between_refused() {
        let input = format!(
            "{HEAD}{SFMC}            llvm.store *src <- v;\n{WEXIT}            llvm.store *tmp <- v;\n            r = llvm.load tmp : builtin.integer i64;\n{WRET}"
        )
        .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(!srcfwd_wstore_try(&input, true));
    }

    /// `tmp` as the *value* of a store: the pointer escapes — out of the
    /// transform's reach whatever the knob.
    #[test]
    fn srcfwd_wstore_stored_value_refused() {
        let input = format!(
            "{HEAD}{SFMC}{WEXIT}            llvm.store *src <- tmp;\n            r = llvm.load tmp : builtin.integer i64;\n{WRET}"
        )
        .replace("dst = llvm.alloca", "src = llvm.alloca");
        assert!(!srcfwd_wstore_try(&input, true));
    }

    /// `tmp` filled from a `noalias` entry param — caller-visible
    /// memory: a store into the temp would write it where the program
    /// touched only its private copy. Refuse even on a writable param.
    #[test]
    fn srcfwd_wstore_param_source_refused() {
        let input = format!(
            "{NA_HEAD}            llvm.br ^loop()\n          ^loop():\n            m = llvm.call_intrinsic @\"llvm.memcpy\" (tmp, p, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n{WEXIT}            llvm.store *tmp <- v;\n            r = llvm.load tmp : builtin.integer i64;\n{WRET}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, _m) = fixture(&input, st, ctx);
        let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head().unwrap();
        let p = entry.deref(ctx).get_argument(ctx, 0);
        st.noalias.insert(p, (8, true));
        let tmp = blocks(ctx, f)
            .into_iter()
            .flat_map(|b| ops(ctx, b))
            .find(|&o| Operation::is_op::<AllocaOp>(o, ctx))
            .map(|o| o.deref(ctx).get_result(ctx, 0))
            .unwrap();
        TEST_WSTORE.with(|c| c.set(Some(true)));
        let ok = srcfwd_one(ctx, st, entry, f, tmp, &mut None, &mut None, None, "");
        TEST_WSTORE.with(|c| c.set(None));
        assert!(!ok);
    }

    /// `BPM` fill with the WSTORE knob on too — `srcfwd_bpsrc_try` plus
    /// the store branch.
    fn srcfwd_wbparam_try(input: &str, on: bool) -> bool {
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, _m) = fixture(input, st, ctx);
        let entry = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head().unwrap();
        let tmp = blocks(ctx, f)
            .into_iter()
            .flat_map(|b| ops(ctx, b))
            .find(|&o| Operation::is_op::<AllocaOp>(o, ctx))
            .map(|o| o.deref(ctx).get_result(ctx, 0))
            .unwrap();
        TEST_BPSRC.with(|c| c.set(Some(true)));
        TEST_WSTORE.with(|c| c.set(Some(on)));
        let ok = srcfwd_one(ctx, st, entry, f, tmp, &mut None, &mut None, None, "");
        TEST_WSTORE.with(|c| c.set(None));
        TEST_BPSRC.with(|c| c.set(None));
        ok
    }

    const WBPT: &str = "        llvm.cond_br if flag ^loop(dst) else ^exit()\n          ^exit():\n";

    /// The param-carried fill (`sv` names `dst`/`dst2` by route) with the
    /// store in the exit block: `sv` dominates it, the fill guards it,
    /// and no window is read after. Forwarded — the store lands on
    /// whichever window `sv` names that traversal.
    #[test]
    fn srcfwd_wbparam_store_forwarded() {
        let input = format!(
            "{BPH}{BPM}{WBPT}            llvm.store *tmp <- v;\n            r = llvm.load tmp : builtin.integer i64;\n{WRET}"
        );
        assert!(srcfwd_wbparam_try(&input, true));
        assert!(!srcfwd_wbparam_try(&input, false));
    }

    /// The same shape with `dst` — one of `sv`'s windows — read after
    /// the store: the rebase would show the stored bytes where the
    /// program kept the window's own. Must refuse.
    #[test]
    fn srcfwd_wbparam_window_read_after_refused() {
        let input = format!(
            "{BPH}{BPM}{WBPT}            llvm.store *tmp <- v;\n            r = llvm.load dst : builtin.integer i64;\n{WRET}"
        );
        assert!(!srcfwd_wbparam_try(&input, true));
    }

    const NOP_HEAD: &str = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @t: llvm.func <llvm.void () variadic = false> [] {
          ^entry():
            eight = llvm.constant <builtin.integer <8: i64>> : builtin.integer i64;
            zero = llvm.constant <builtin.integer <0: i64>> : builtin.integer i64;
            tmp = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
            dst = llvm.alloca [builtin.integer i8 x eight] : llvm.ptr (0);
    "#;
    const NOP_TAIL: &str = "            llvm.return\n        }\n    }\n";

    /// `memcpy(tmp, tmp)` is a no-op: erased under
    /// `PLIRON_MEMCPYOPT_NOPCOPY`; off, the old "self-copy through the
    /// temp" refusal keeps it (returning `false`).
    #[test]
    fn selfcopy_erased() {
        let input = format!(
            "{NOP_HEAD}            m = llvm.call_intrinsic @\"llvm.memcpy\" (tmp, tmp, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n{NOP_TAIL}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        TEST_NOPCOPY.with(|c| c.set(Some(true)));
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
        TEST_NOPCOPY.with(|c| c.set(None));
        assert!(st.intrinsics.get(m).is_none());

        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        TEST_NOPCOPY.with(|c| c.set(Some(false)));
        assert!(!try_forward(ctx, st, f, &mut None, &mut None, &[], m));
        TEST_NOPCOPY.with(|c| c.set(None));
        assert!(st.intrinsics.get(m).is_some());
    }

    /// The same through a GEP'd end: `memcpy(tmp+0, tmp)`.
    #[test]
    fn selfcopy_gep_erased() {
        let input = format!(
            "{NOP_HEAD}            tmp0 = llvm.gep <builtin.integer i8> (tmp, zero)[OperandIdx(1)] : llvm.ptr (0);\n            m = llvm.call_intrinsic @\"llvm.memcpy\" (tmp0, tmp, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n{NOP_TAIL}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (f, m) = fixture(&input, st, ctx);
        let g = m.deref(ctx).get_operand(ctx, 0).defining_op().unwrap();
        st.consts
            .insert(g.deref(ctx).get_operand(ctx, 1), crate::state::ConstVal::Bits(0));
        TEST_NOPCOPY.with(|c| c.set(Some(true)));
        assert!(try_forward(ctx, st, f, &mut None, &mut None, &[], m));
        TEST_NOPCOPY.with(|c| c.set(None));
        assert!(st.intrinsics.get(m).is_none());
    }

    /// `memcpy(dst, tmp)` with distinct ends is untouched by the gate.
    #[test]
    fn nonselfcopy_kept() {
        let input = format!(
            "{NOP_HEAD}            m = llvm.call_intrinsic @\"llvm.memcpy\" (dst, tmp, eight) : llvm.func <llvm.void (llvm.ptr (0), llvm.ptr (0), builtin.integer i64) variadic = false>;\n{NOP_TAIL}"
        );
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let (_f, m) = fixture(&input, st, ctx);
        TEST_NOPCOPY.with(|c| c.set(Some(true)));
        assert!(!kill_nopcopy(ctx, st, m));
        TEST_NOPCOPY.with(|c| c.set(None));
        assert!(st.intrinsics.get(m).is_some());
    }
}

/// `PLIRON_MEMCPYOPT_SRCFWD=1`: a temporary that only a copy of an
/// unaliased parameter's bytes writes reads the parameter instead.
///
/// ```text
/// t = alloca n;  memcpy(t, s + off, n);  ... load (t + k) ...
/// ```
///
/// becomes `load (s + off + k)` and the copy and `t` go. `s` is an entry
/// param rustc marks `noalias` (a by-value aggregate passed by pointer, a
/// `&mut`, a frozen `&T`) with at least `off + n` dereferenceable bytes,
/// whose address never leaves the function's view ([`home_users`], drop
/// glue counted as a write): only the function's own ops can change its
/// bytes. `t` is an alloca of exactly `n` bytes whose every use, through
/// GEPs, is a load, the source of a `memcpy`/`memmove`, a lifetime marker,
/// or the destination of such a copy from the same `(s, off)` — plus,
/// under `PLIRON_SRCFWD_WSTORE` (opt-in), a `store` *into* `t`, whose write
/// the rebase lands on `s` directly (`srcfwd_wstore` spells out the
/// write-side checks). Sound when
/// no op that may write `s`'s `[off, off + n)` lies on a path from a copy
/// to a read of `t` (one forward walk from the copies, one from the writes
/// they reach); a read no copy reaches read uninitialized bytes, and the
/// parameter's are a refinement of them.
///
/// lol_html's `SelectorMatchingVm::exec_without_attrs(ctx)` moved its
/// 152-byte `ExecutionCtx` into a local on every path and the 144-byte
/// `stack_item` out of that local again (`into_owned`): two `memcpy` calls
/// per element, plus one in each bailout path.
pub fn run_srcfwd(ctx: &mut Context, st: &mut State) {
    if !pliron_cfg::env_is("PLIRON_MEMCPYOPT_SRCFWD", "1") {
        return;
    }
    let mut n = 0;
    let trace = pliron_cfg::tracing_on();
    let funcs: Vec<_> = st.funcs.iter().map(|(name, f)| (f.op, trace.then(|| name.clone()))).collect();
    for (f, name) in funcs {
        if !opt_body(ctx, st, f) {
            continue;
        }
        let Some(entry) = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head() else {
            continue;
        };
        let bname = crate::bisect_name(st, f);
        let mut inc_slot: Option<IncMap> = None;
        let mut dom_slot: Option<Dom> = None;
        // Rounds: a temp copied from a rewritten temp is a candidate next.
        for _ in 0..4 {
            let mut cands: Vec<Value> = Vec::new();
            let mut seen: FxHashSet<Value> = FxHashSet::default();
            for b in crate::inline::blocks(ctx, f) {
                for op in crate::inline::ops(ctx, b) {
                    if !matches!(st.intrinsics.get(op).map(|s| s.as_str()), Some("llvm.memcpy" | "llvm.memmove")) {
                        continue;
                    }
                    // `phisimp` binding a copy's block-param end to its
                    // other end leaves `memcpy(a, a)` — dead, and a
                    // self-copy keeps the alloca out of sroa.
                    if nopcopy() && kill_nopcopy(ctx, st, op) {
                        n += 1;
                        continue;
                    }
                    let t = op.deref(ctx).get_operand(ctx, 0);
                    if !t.defining_op().is_some_and(|a| Operation::is_op::<AllocaOp>(a, ctx)) {
                        continue;
                    }
                    let sv = op.deref(ctx).get_operand(ctx, 1);
                    let resolves = src_base(ctx, st, entry, inc_of(ctx, f, &mut inc_slot), sv).is_some()
                        || (srcfwd_bpsrc()
                            && base_offsets(ctx, st, inc_of(ctx, f, &mut inc_slot), sv).is_some());
                    if !resolves {
                        if trace {
                            let kind = match alloca_bases(ctx, st, inc_of(ctx, f, &mut inc_slot), sv) {
                                Some(bs) => format!("src merges {} allocas", bs.len()),
                                None if sv.defining_op().is_none() => "src block-param/other value".to_string(),
                                None => format!("src is {}", sv.defining_op().map(|d| Operation::get_opid(d, ctx).to_string()).unwrap_or_default()),
                            };
                            tracing::debug!(target: "pliron::memcpyopt", "srcfwd: {kind} ({})", name.as_deref().unwrap_or(""));
                        }
                        continue;
                    }
                    if seen.insert(t) {
                        cands.push(t);
                    }
                }
            }
            let mut did = 0;
            for t in cands {
                if srcfwd_one(ctx, st, entry, f, t, &mut inc_slot, &mut dom_slot, name.as_deref(), &bname) {
                    did += 1;
                }
            }
            n += did;
            if did == 0 {
                break;
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO) {
        tracing::info!(target: "pliron::stats", "srcfwd {}: {n} temporaries read from their source", st.cgu)
    }
}

/// `v` as `(param, offset)`: constant GEPs down to an entry param of
/// `entry` that rustc marks `noalias`.
fn param_base(ctx: &Context, st: &State, entry: Ptr<BasicBlock>, v: Value) -> Option<(Value, u64)> {
    let mut v = v;
    let mut off = 0u64;
    for _ in 0..16 {
        match v.defining_op() {
            Some(g) if Operation::is_op::<GetElementPtrOp>(g, ctx) => {
                let o = gep_offset(ctx, st, g)?;
                off = u64::try_from(off as i64 + o).ok()?;
                v = g.deref(ctx).get_operand(ctx, 0);
            }
            Some(_) => return None,
            None => {
                if v.defining_block() != Some(entry) {
                    return None;
                }
                st.noalias.get(v)?;
                return Some((v, off));
            }
        }
    }
    None
}

/// `PLIRON_SRCFWD_ALLOCA=1`: a temp's fill-copy may also read out of a
/// local alloca — `memcpy(t, slot+k, n)` forwards `t`'s bounded reads
/// (`load`s, copy sources, drop glue) onto `slot+k`. rustc's MIR
/// temp→temp chains (`next()`'s out slot copied to the `match` scrutinee
/// copied to a closure argument, each a full memcpy) fold to reads of the
/// first slot. The source alloca's def must dominate the temp's so the
/// rebased pointers stay defined on every use, and the reachability
/// conflict check treats `copies` as barriers — a refill of the source on
/// a later loop iteration hides behind that iteration's copy, only a
/// write that reaches a read without crossing a fill is a conflict.
fn srcfwd_alloca() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| crate::env_is("PLIRON_SRCFWD_ALLOCA", "1"));
    TEST_ALLOCA.with(|c| c.get()).unwrap_or(*ON)
}

thread_local! {
    /// `Some(on)` overrides the environment, `None` reads it (tests).
    static TEST_ALLOCA: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `PLIRON_SRCFWD_BPSRC=1` (opt-in): a temp's fill-copy may also read
/// through a block parameter — `memcpy(t, sv, n)` where `sv` is a param
/// (or a GEP of one) whose incoming edges resolve to more than one
/// constant `(alloca, offset)` window, or to a window `alloca_base`
/// could not pin to a single base+offset (`alloca_bases`'s union: the
/// html census's "src merges 1 allocas", ~1550 candidate copies vs ~60
/// accepted temps, 2026-11-19). `sv` itself is the rebase pointer — the
/// param already names the window the copy read — but it is an SSA name
/// for a *different* window on each traversal of its block, so besides
/// `alloca_base`'s per-edge bounds the transform needs two extra checks
/// `srcfwd_one` spells out: `sv`'s def must dominate every surviving
/// temp use, and every such use must be reached from that def only by
/// crossing a fill (an unfilled temp holds an older window's bytes where
/// the rebase would hand it the newer one).
fn srcfwd_bpsrc() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| crate::env_is("PLIRON_SRCFWD_BPSRC", "1"));
    TEST_BPSRC.with(|c| c.get()).unwrap_or(*ON)
}

thread_local! {
    /// `Some(on)` overrides the environment, `None` reads it (tests).
    static TEST_BPSRC: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `PLIRON_SRCFWD_WSTORE=1` (opt-in): a temp's uses may also include
/// `store`s *into* the temp — the store's address operand is a
/// temp-rooted pointer (`store`'s operands are `[value, ptr]`). The
/// rebase lands the write on the source window directly, so the temp's
/// private copy of those bytes never exists: sound only when no fill
/// can run after the store (the erased copy's "restore the source
/// bytes" would be lost) and no read of the source's stored range can
/// observe the new bytes early — `srcfwd_one`'s write-side checks. A
/// param source is caller-visible memory, so a store into its temp is
/// refused outright; a store use where the temp is the *value* operand
/// (`store dst <- t`) is the pointer escaping and stays a refusal.
fn srcfwd_wstore() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| crate::env_is("PLIRON_SRCFWD_WSTORE", "1"));
    TEST_WSTORE.with(|c| c.get()).unwrap_or(*ON)
}

thread_local! {
    /// `Some(on)` overrides the environment, `None` reads it (tests).
    static TEST_WSTORE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `v` as the set of constant `(alloca, byte-offset)` windows it may
/// point at: `alloca_bases`'s union walk with the offsets kept — constant
/// GEPs down to an `st.allocas` alloca, block parameters resolved per
/// incoming edge. `None` when any edge is opaque (a non-constant GEP, a
/// load, a callee operand, an entry param — `src_base`'s param case owns
/// those), a cycle bites, or more than 16 windows meet.
fn base_offsets(
    ctx: &Context,
    st: &State,
    inc: Option<&IncMap>,
    v: Value,
) -> Option<FxHashSet<(Value, u64)>> {
    let mut memo: FxHashMap<Value, Option<FxHashSet<(Value, u64)>>> = FxHashMap::default();
    let mut on = FxHashSet::default();
    base_offsets_d(ctx, st, inc, v, &mut memo, &mut on).filter(|s| s.len() <= 16)
}

fn base_offsets_d(
    ctx: &Context,
    st: &State,
    inc: Option<&IncMap>,
    mut v: Value,
    memo: &mut FxHashMap<Value, Option<FxHashSet<(Value, u64)>>>,
    on: &mut FxHashSet<Value>,
) -> Option<FxHashSet<(Value, u64)>> {
    let mut off: i64 = 0;
    for _ in 0..16 {
        if let Some(d) = v.defining_op() {
            if Operation::is_op::<AllocaOp>(d, ctx) {
                return (st.allocas.contains(v) && off >= 0)
                    .then(|| [(v, off as u64)].into_iter().collect());
            }
            if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
                return None;
            }
            off = off.checked_add(gep_offset(ctx, st, d)?)?;
            v = d.deref(ctx).get_operand(ctx, 0);
            continue;
        }
        let res = if let Some(r) = memo.get(&v) {
            r.clone()
        } else if on.insert(v) {
            let mut acc: Option<FxHashSet<(Value, u64)>> = Some(FxHashSet::default());
            if let Some(edges) = inc.and_then(|m| m.get(&v)).filter(|e| !e.is_empty()) {
                for &(t, si, k) in edges {
                    let Some(iv) = inc_value(ctx, t, si, k)
                        .and_then(|iv| base_offsets_d(ctx, st, inc, iv, memo, on))
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
            acc
        } else {
            None
        };
        // The GEPs above the param shift every edge's window.
        return res.and_then(|rs| {
            rs.iter()
                .map(|&(b, o)| {
                    (off >= 0)
                        .then(|| o.checked_add(off as u64))
                        .flatten()
                        .map(|o| (b, o))
                })
                .collect()
        });
    }
    None
}

/// `v` as `(base, byte offset, is_alloca)`: constant GEPs (and, for the
/// alloca case, block parameters) down to an entry `noalias` param — the
/// original `srcfwd_one` source — or, under `PLIRON_SRCFWD_ALLOCA`, a
/// local alloca.
fn src_base(
    ctx: &Context,
    st: &State,
    entry: Ptr<BasicBlock>,
    inc: Option<&IncMap>,
    v: Value,
) -> Option<(Value, u64, bool)> {
    if let Some((p, off)) = param_base(ctx, st, entry, v) {
        return Some((p, off, false));
    }
    if srcfwd_alloca() {
        if let Some((a, off)) = alloca_base(ctx, st, inc, v) {
            return Some((a, off, true));
        }
    }
    None
}

macro_rules! sfno {
    ($name:expr, $($t:tt)*) => {{
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::memcpyopt", "srcfwd: {} ({})", format!($($t)*), $name.unwrap_or(""));
        }
        return false;
    }};
}

/// Ops of `targets` that run after one of `starts` on some path (invoke
/// unwind edges included); `None` when the walk runs out of budget. An op
/// in `barriers` ends the walk along its path: nothing past it counts —
/// `srcfwd_one` passes the temp's fill-copies so a source write separated
/// from a read by a re-fill (the next iteration's copy, say) is not a
/// conflict.
fn reached_after(
    ctx: &Context,
    st: &State,
    starts: &[Ptr<Operation>],
    targets: &FxHashSet<Ptr<Operation>>,
    barriers: Option<&FxHashSet<Ptr<Operation>>>,
) -> Option<FxHashSet<Ptr<Operation>>> {
    const MAX_OPS: usize = 1 << 16;
    let mut out = FxHashSet::default();
    let mut seen = 0usize;
    let mut visited: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
    let mut work: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> = Vec::new();
    for &s in starts {
        let b = s.deref(ctx).get_parent_block()?;
        work.push((b, s.get_next(ctx)));
        if let Some(&(pad, _)) = st.invokes.get(s) {
            work.push((pad, None));
        }
    }
    while let Some((b, from)) = work.pop() {
        if from.is_none() && !visited.insert(b) {
            continue;
        }
        let mut cur = match from {
            Some(op) => Some(op),
            None => b.deref(ctx).get_head(),
        };
        let mut last = None;
        let mut stopped = false;
        while let Some(op) = cur {
            seen += 1;
            if seen > MAX_OPS {
                return None;
            }
            last = Some(op);
            if targets.contains(&op) {
                out.insert(op);
            }
            if barriers.is_some_and(|bs| bs.contains(&op)) {
                stopped = true;
                break;
            }
            if let Some(&(pad, _)) = st.invokes.get(op) {
                work.push((pad, None));
            }
            cur = op.get_next(ctx);
        }
        if !stopped {
            if let Some(t) = last {
                for s in t.deref(ctx).successors(ctx) {
                    work.push((s, None));
                }
            }
        }
    }
    Some(out)
}

/// A fill-copy's resolved source: what the temp's uses rebase onto.
/// Param and Alloca name a *fixed* base — the rebase pointer is freshly
/// built (`gep(base, off)`), so only the base's bytes can drift between a
/// fill and a use. `BParam` names the copy's own source operand — a
/// pointer that is itself a different window on each traversal of its
/// block, which carries the two extra checks below.
enum Src {
    /// An entry `noalias` param at a byte offset — the original source.
    Param(Value, u64),
    /// A local alloca at a byte offset (`PLIRON_SRCFWD_ALLOCA`).
    Alloca(Value, u64),
    /// The fill-copy's source operand and the `(alloca, offset)` windows
    /// its incoming edges resolve to (`PLIRON_SRCFWD_BPSRC`).
    BParam(Value, FxHashSet<(Value, u64)>),
}

/// Two fill-copies read the same source bytes: the same base window for
/// the fixed bases, the same operand value for a param-carried one —
/// different operands may carry different windows even with equal edge
/// sets.
fn same_src(a: &Src, b: &Src) -> bool {
    match (a, b) {
        (Src::Param(x, o), Src::Param(y, p)) | (Src::Alloca(x, o), Src::Alloca(y, p)) => {
            x == y && o == p
        }
        (Src::BParam(x, _), Src::BParam(y, _)) => x == y,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn srcfwd_one(
    ctx: &mut Context,
    st: &mut State,
    entry: Ptr<BasicBlock>,
    f: Ptr<Operation>,
    t: Value,
    inc_slot: &mut Option<IncMap>,
    dom_slot: &mut Option<Dom>,
    name: Option<&str>,
    bname: &str,
) -> bool {
    let Some(t_op) = t.defining_op() else { return false };
    let Some(&(n, _)) = st.allocas.get(t) else { return false };
    if n == 0 {
        return false;
    }
    // `t`'s cone: copies into it, reads of it, markers.
    let mut copies: Vec<Ptr<Operation>> = Vec::new();
    let mut readers: Vec<Ptr<Operation>> = Vec::new();
    let mut markers: Vec<Ptr<Operation>> = Vec::new();
    let mut drops: Vec<Ptr<Operation>> = Vec::new();
    // `PLIRON_SRCFWD_WSTORE`: stores into the temp — (op, byte offset in
    // the temp, stored size).
    let mut wstores: Vec<(Ptr<Operation>, u64, u64)> = Vec::new();
    let mut src: Option<Src> = None;
    let mut work = vec![t];
    let mut cone: Vec<Ptr<Operation>> = Vec::new();
    let mut budget = 4096usize;
    while let Some(p) = work.pop() {
        for u in p.uses_iter(ctx) {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                sfno!(name, "budget");
            }
            let op = u.user_op();
            cone.push(op);
            let i = u.find_index(ctx);
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if i != 0 {
                    sfno!(name, "temp as a gep index");
                }
                work.push(op.deref(ctx).get_result(ctx, 0));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                if st.volatile.contains(op) {
                    sfno!(name, "volatile read");
                }
                readers.push(op);
            } else if let Some(nm) = st.intrinsics.get(op).map(|s| s.as_str()) {
                match nm {
                    "llvm.lifetime.start" | "llvm.lifetime.end" => markers.push(op),
                    "llvm.memcpy" | "llvm.memmove" if i == 1 && !st.volatile.contains(op) => readers.push(op),
                    "llvm.memcpy" | "llvm.memmove" if i == 0 && p == t && !st.volatile.contains(op) && !st.invokes.contains(op) => {
                        let len = const_int(ctx, st, op.deref(ctx).get_operand(ctx, 2)).and_then(|x| u64::try_from(x).ok());
                        if len != Some(n) {
                            sfno!(name, "partial copy into the temp");
                        }
                        let sv = op.deref(ctx).get_operand(ctx, 1);
                        let b = src_base(ctx, st, entry, inc_of(ctx, f, inc_slot), sv)
                            .map(|(s, o, isa)| if isa { Src::Alloca(s, o) } else { Src::Param(s, o) })
                            .or_else(|| {
                                // `PLIRON_SRCFWD_BPSRC`: a param-carried
                                // source — the operand value is the rebase
                                // pointer, its edges give the windows.
                                srcfwd_bpsrc()
                                    .then(|| base_offsets(ctx, st, inc_of(ctx, f, inc_slot), sv))
                                    .flatten()
                                    .map(|edges| Src::BParam(sv, edges))
                            });
                        let Some(b) = b else {
                            sfno!(name, "copy from something else");
                        };
                        if src.as_ref().is_some_and(|s| !same_src(s, &b)) {
                            sfno!(name, "copies from two sources");
                        }
                        src = Some(b);
                        copies.push(op);
                    }
                    _ => sfno!(name, "temp in {nm}"),
                }
            } else if Operation::get_op::<CallOp>(op, ctx).is_some_and(|c| c.get_attr_llvm_call_callee(ctx).is_some())
                && st
                    .calls
                    .get(op)
                    .and_then(|c| c.callee.as_deref())
                    .is_some_and(|c| c.contains("4core3ptr9drop_glue") || c.contains("13drop_in_place"))
            {
                // Drop glue on the temp or a field of it (an unwind path
                // dropping a value moved into it): it reads the bytes and,
                // rebased, may write the param's.
                drops.push(op);
            } else if Operation::is_op::<StoreOp>(op, ctx) {
                // `store`'s operands are `[value, ptr]`: `i == 1` is a
                // write into the temp's slot — `PLIRON_SRCFWD_WSTORE`
                // rebases it onto the source (`srcfwd_wstore`). `i == 0`
                // stores the temp pointer itself: escape.
                if i != 1 {
                    sfno!(name, "temp in llvm.store as value");
                }
                if st.volatile.contains(op) {
                    sfno!(name, "temp in llvm.store as address (volatile)");
                }
                let Some(k) = temp_off(ctx, st, t, p) else {
                    sfno!(name, "temp in llvm.store as address (non-const off)");
                };
                if !srcfwd_wstore() {
                    sfno!(name, "temp in llvm.store as address");
                }
                let sz = size_align(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx)).0;
                if k < 0 || (k as u64).checked_add(sz).is_none_or(|e| e > n) {
                    sfno!(name, "store into the temp past the copied window");
                }
                wstores.push((op, k as u64, sz));
            } else {
                // Census (temporary): for a call-arg use, log the facts a
                // `PLIRON_SRCFWD_CALLARG` rule would need — arg index, the
                // call-site `nocap`/`arg_ro`/`arg_extent`, and the callee's
                // `nocapture`/`nowrite` summaries.
                if pliron_cfg::tracing_on()
                    && Operation::get_op::<CallOp>(op, ctx).is_some()
                    && let Some(info) = st.calls.get(op)
                {
                    let direct = Operation::get_op::<CallOp>(op, ctx).unwrap().get_attr_llvm_call_callee(ctx).is_some();
                    let ai = i.wrapping_sub(usize::from(!direct));
                    let sym = info.callee.as_deref();
                    tracing::debug!(target: "pliron::memcpyopt",
                        "srcfwd: temp in {} {:?} ai={ai} nocap={:?} ro={:?} ext={:?} sum_nc={} nowrite={:?} ({})",
                        Operation::get_opid(op, ctx), sym,
                        info.nocap.as_ref().and_then(|n| n.get(ai)),
                        info.arg_ro.as_ref().and_then(|r| r.get(ai)),
                        info.arg_extent.as_ref().and_then(|e| e.get(ai)),
                        sym.and_then(|s| st.nocapture.get(s)).map(|s| s.contains(&(ai as u32))).unwrap_or(false),
                        sym.and_then(|s| st.nowrite.get(s)),
                        name.unwrap_or(""));
                }
                sfno!(name, "temp in {} {:?}", Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.as_deref()));
            }
        }
    }
    let Some(src) = src else { return false };
    readers.extend_from_slice(&drops);
    if !wstores.is_empty() && matches!(src, Src::Param(..)) {
        // A store into the temp rebased onto an entry parameter writes
        // the caller's memory where the program only touched its local
        // copy — the difference survives the return. (The local sources
        // get the reachability checks below instead.)
        sfno!(name, "store into a temp copied from caller-visible memory");
    }
    // Where the rebased pointer's `gep(s, off)` is built: at the temp's
    // alloca for a parameter (its def dominates everything), right after
    // the source alloca's own def otherwise — `a_op` dominating `t_op`
    // would be enough but is needlessly narrow (same-block order), so
    // the check below is per cone op instead. A `BParam` source rebases
    // onto the operand itself (`ptr_plus` at offset 0 returns it), so
    // `at` stays unused there.
    let mut at = t_op;
    let (deref, writable) = match &src {
        Src::Alloca(s, _) => {
            // Alloca source (`PLIRON_SRCFWD_ALLOCA`): the slot's own size
            // is the dereferenceable bound and it is always writable.
            // `s`'s def must dominate every op that used a `t`-rooted
            // pointer — the reads rebase onto `s + off`, so `s` has to be
            // defined wherever they were — and the source can't be the
            // temp itself (a copy out of `t` into `t`).
            if *s == t {
                sfno!(name, "copy out of the temp itself");
            }
            let Some(&(sz, _)) = st.allocas.get(*s) else { return false };
            let Some(a_op) = s.defining_op() else { return false };
            if !cone.iter().all(|&o| op_dominates(ctx, st, dom_slot, f, a_op, o)) {
                sfno!(name, "source alloca does not dominate a temp use");
            }
            let Some(next) = a_op.get_next(ctx) else { return false };
            at = next;
            (sz, true)
        }
        Src::Param(s, _) => {
            let Some(&(deref, writable)) = st.noalias.get(*s) else { return false };
            (deref, writable)
        }
        Src::BParam(..) => (0, true),
    };
    if !drops.is_empty() && !writable {
        sfno!(name, "drop of a temp copied from read-only memory");
    }
    match &src {
        Src::Param(_, off) | Src::Alloca(_, off) => {
            if off.checked_add(n).is_none_or(|e| e > deref) {
                sfno!(name, "source range past dereferenceable {deref}");
            }
        }
        Src::BParam(sv, edges) => {
            // Every edge's `[off, off + n)` must sit inside its alloca
            // (a `gep` of the temp's own slot is a copy out of it).
            for &(b, o) in edges {
                if b == t {
                    sfno!(name, "copy out of the temp itself");
                }
                let Some(&(sz, _)) = st.allocas.get(b) else { return false };
                if o.checked_add(n).is_none_or(|e| e > sz) {
                    sfno!(name, "source window past its base's {sz} bytes");
                }
            }
            // `sv` is the rebase pointer: its definition must dominate
            // every surviving temp use (the copies and markers are
            // erased, so they are exempt — their operands die with them).
            let cset: FxHashSet<Ptr<Operation>> = copies.iter().copied().collect();
            let surv: FxHashSet<Ptr<Operation>> = cone
                .iter()
                .copied()
                .filter(|o| !cset.contains(o) && !markers.contains(o))
                .collect();
            let ok = match sv.defining_op() {
                Some(d) => cone
                    .iter()
                    .all(|&o| !surv.contains(&o) || op_dominates(ctx, st, dom_slot, f, d, o)),
                None => {
                    let Some(db) = sv.defining_block() else { return false };
                    match dom_of(ctx, st, dom_slot, f) {
                        Some(d) => cone.iter().all(|&o| {
                            !surv.contains(&o)
                                || o.deref(ctx)
                                    .get_parent_block()
                                    .is_some_and(|ob| d.dominates(db, ob))
                        }),
                        None => false,
                    }
                }
            };
            if !ok {
                sfno!(name, "source value does not dominate a temp use");
            }
            // `sv` names a *different* window on every traversal of its
            // block: a temp use reached from `sv`'s def without crossing
            // a fill holds an older window's bytes, which the rebase
            // would swap for the newer pointer's. The fill copies are
            // the barriers — a use every path covers with a fill sees the
            // window this traversal's `sv` already names.
            let unguarded = match sv.defining_op() {
                Some(d) => reached_after(ctx, st, &[d], &surv, Some(&cset)),
                // A param is defined at its block's head; `sv`'s lifetime
                // starts before the head op, so the walk must cover the
                // whole block — a seed of the terminator has no next op,
                // which makes `reached_after` scan the block from its head
                // (the usual case has the fill AS the head op, and uses
                // past it are exactly the guarded ones).
                None => sv.defining_block().and_then(|db| {
                    let term = db.deref(ctx).get_terminator(ctx)?;
                    reached_after(ctx, st, &[term], &surv, Some(&cset))
                }),
            };
            match unguarded {
                Some(u) if u.is_empty() => {}
                Some(_) => sfno!(name, "a temp use is reached without a fill after the source moved"),
                None => sfno!(name, "walk budget"),
            }
        }
    }
    let frozen = match &src {
        Src::Param(s, off) => {
            drops.is_empty() && st.frozen.get(*s).is_some_and(|&d| d >= off + n)
        }
        _ => false,
    };
    if !frozen {
        let (lo, hi) = match &src {
            Src::Param(_, off) | Src::Alloca(_, off) => (*off, *off + n),
            Src::BParam(..) => (0, n),
        };
        let (s, off) = match &src {
            Src::Param(s, off) | Src::Alloca(s, off) => (*s, *off),
            Src::BParam(..) => (t, 0), // unused on that path
        };
        let Some((users, captured)) = (match &src {
            Src::BParam(_, edges) => {
                // The union of every edge's touches of its base's window,
                // walked through the block params that carry them
                // (`base_touches` follows `succ_param`; `src_touches`
                // would call the branch operand itself an escape).
                let mut out: Touches = FxHashMap::default();
                for &(b, o) in edges {
                    let sz = st.allocas.get(b).map(|x| x.0).unwrap_or(0);
                    let Some(t2) = base_touches(ctx, st, b, sz, o, n) else {
                        sfno!(name, "source walk budget");
                    };
                    for (op, k) in t2 {
                        let e = out.entry(op).or_insert(k);
                        if *e != k {
                            *e = TouchKind::Other;
                        }
                    }
                }
                Some((
                    out.into_iter()
                        .map(|(op, k)| {
                            (
                                op,
                                vec![Touch {
                                    read: k != TouchKind::Write,
                                    write: k != TouchKind::Read,
                                    lo: 0,
                                    hi: u64::MAX,
                                    kill: false,
                                }],
                            )
                        })
                        .collect::<FxHashMap<Ptr<Operation>, Vec<Touch>>>(),
                    false,
                ))
            }
            _ => src_touches(ctx, st, s, deref),
        }) else {
            sfno!(name, "source walk budget");
        };
        if readers.iter().any(|r| users.contains_key(r)) {
            sfno!(name, "a read of the temp also touches the source");
        }
        let mut writes: FxHashSet<Ptr<Operation>> = users
            .iter()
            .filter(|(_, ts)| ts.iter().any(|x| x.write && x.lo < hi && lo < x.hi))
            .map(|(&op, _)| op)
            .chain(drops.iter().copied())
            .collect();
        // A call may have kept the param's address: from then on any op
        // that may write memory other than an alloca's may write it.
        if captured {
            // A copy out of the temp into memory the param may share
            // would become an overlapping copy.
            if readers.iter().any(|&r| !drops.contains(&r) && may_write_nonlocal(ctx, st, r)) {
                sfno!(name, "a read of the temp copies into memory the source may share");
            }
            let f = entry.deref(ctx).get_parent_region().unwrap().deref(ctx).get_parent_op();
            for b in crate::inline::blocks(ctx, f) {
                for op in crate::inline::ops(ctx, b) {
                    if may_write_nonlocal(ctx, st, op) {
                        writes.insert(op);
                    }
                }
            }
        }
        if !writes.is_empty() {
            let Some(hit) = reached_after(ctx, st, &copies, &writes, None) else {
                sfno!(name, "walk budget");
            };
            if !hit.is_empty() {
                let hit: Vec<_> = hit.into_iter().collect();
                // `PLIRON_SRCFWD_WSTORE`: a store into the temp is a
                // protected use too — a source write that reaches it
                // without crossing a fill has its bytes clobbered early
                // (the write-direction mirror of a stale read).
                let rs: FxHashSet<Ptr<Operation>> = readers
                    .iter()
                    .copied()
                    .chain(wstores.iter().map(|w| w.0))
                    .collect();
                // A fill-copy is a barrier for the write→read walk under
                // `PLIRON_SRCFWD_ALLOCA`: a write of the source that only
                // reaches a read through another fill (a refill on a later
                // loop iteration) can't change what the read sees — that
                // read's temp was last written by a copy that already saw
                // the write. Off by default: the barriers also widen the
                // param-source transform, so it goes under the same knob.
                // `PLIRON_SRCFWD_BPSRC` needs them outright — a param
                // source only exists to be refilled.
                let cset = (srcfwd_alloca() || matches!(src, Src::BParam(..)))
                    .then(|| copies.iter().copied().collect::<FxHashSet<Ptr<Operation>>>());
                // `PLIRON_SRCFWD_RANGES=1`: a write conflicts only with a read
                // after it whose bytes overlap (temp coordinates; unknown
                // overlaps everything). Drop glue on a field writes only that
                // field (`drop_in_place::<T>` touches the `T` at its pointer:
                // rustc's pointee size, `arg_extent`), so an unwind chain
                // dropping a moved value's fields one by one is no conflict.
                // A `BParam` walk has no per-op ranges to be precise with.
                let precise = srcfwd_ranges() && hit.len() <= 32 && !matches!(src, Src::BParam(..));
                let late = if precise {
                    let wr = |w: Ptr<Operation>| -> Option<(i64, i64)> {
                        if drops.contains(&w) {
                            return temp_access(ctx, st, t, w);
                        }
                        let ts = users.get(&w)?;
                        let (a, b) = ts.iter().filter(|x| x.write).fold((u64::MAX, 0u64), |(a, b), x| (a.min(x.lo), b.max(x.hi)));
                        (a < b).then(|| (a as i64 - off as i64, b as i64 - off as i64))
                    };
                    let mut bad: FxHashSet<Ptr<Operation>> = FxHashSet::default();
                    for &w in &hit {
                        let w_r = wr(w);
                        let Some(after) = reached_after(ctx, st, &[w], &rs, cset.as_ref()) else {
                            sfno!(name, "walk budget");
                        };
                        for r in after {
                            let r_r = temp_access(ctx, st, t, r);
                            let disjoint = matches!((w_r, r_r), (Some((a, b)), Some((c, d))) if b <= c || d <= a);
                            if !disjoint {
                                bad.insert(r);
                            }
                        }
                    }
                    bad
                } else {
                    let Some(late) = reached_after(ctx, st, &hit, &rs, cset.as_ref()) else {
                        sfno!(name, "walk budget");
                    };
                    late
                };
                if !late.is_empty() {
                    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::memcpyopt", tracing::Level::TRACE) {
                        for &w in hit.iter().take(6) {
                            tracing::trace!(target: "pliron::memcpyopt", "srcfwd conflict write: {}", pliron::operation::OpDbg { op: w, ctx }.to_string().chars().take(200).collect::<String>());
                        }
                        for &r in late.iter().take(6) {
                            tracing::trace!(target: "pliron::memcpyopt", "srcfwd conflict read:  {}", pliron::operation::OpDbg { op: r, ctx }.to_string().chars().take(200).collect::<String>());
                        }
                    }
                    let what: Vec<String> = hit
                        .iter()
                        .take(4)
                        .map(|&w| format!("{} {:?}", Operation::get_opid(w, ctx), st.calls.get(w).and_then(|c| c.callee.as_deref()).map(|c| &c[..c.len().min(90)])))
                        .collect();
                    let why = CAPTURE_WHY.with(|w| w.borrow().clone());
                    sfno!(name, "source may be written between a copy and a read (captured {captured} by {why:?}; {} writes: {what:?})", hit.len());
                }
            }
        }
        // `PLIRON_SRCFWD_WSTORE` — a store into the temp lands on the
        // source directly, so the temp's private copy of those bytes
        // never exists. Two hazards the read side does not have:
        //   - a fill after the store would restore the source's bytes
        //     into the temp — the erased copy can't, so the rebased
        //     reads would see the stored value where the program meant
        //     the older source bytes;
        //   - a read of the source past the store sees the new bytes
        //     where the program's own source value was meant to live —
        //     unless a write covering the stored range ran between, which
        //     is what the program read either way.
        if !wstores.is_empty() {
            if captured {
                // The source's address escaped: reads of it through
                // pointers the walk never saw could observe the stored
                // bytes, and `users` names none of them.
                sfno!(name, "store into a temp whose source's address escaped");
            }
            if wstores.len() > 32 {
                sfno!(name, "too many stores into the temp");
            }
            let cset: FxHashSet<Ptr<Operation>> = copies.iter().copied().collect();
            let wops: Vec<Ptr<Operation>> = wstores.iter().map(|w| w.0).collect();
            match reached_after(ctx, st, &wops, &cset, None) {
                Some(h) if h.is_empty() => {}
                Some(_) => sfno!(name, "a fill after a store into the temp drops the stored bytes"),
                None => sfno!(name, "walk budget"),
            }
            let bparam = matches!(src, Src::BParam(..));
            let s_off = match &src {
                Src::Param(..) => 0, // refused above
                Src::Alloca(_, o) => *o,
                Src::BParam(..) => 0,
            };
            for &(w, k, sz) in &wstores {
                let (wlo, whi) = (s_off + k, s_off + k + sz);
                let rtargets: FxHashSet<Ptr<Operation>> = users
                    .iter()
                    .filter(|(_, ts)| ts.iter().any(|x| x.read && x.lo < whi && wlo < x.hi))
                    .map(|(&o, _)| o)
                    .collect();
                if rtargets.is_empty() {
                    continue;
                }
                // For a param-carried source the store's window is
                // dynamic: a write of one possible window cannot prove
                // it covered the one the store hit, so it is no barrier.
                let wbarriers: FxHashSet<Ptr<Operation>> = if bparam {
                    FxHashSet::default()
                } else {
                    users
                        .iter()
                        .filter(|(_, ts)| ts.iter().any(|x| x.write && x.lo <= wlo && whi <= x.hi))
                        .map(|(&o, _)| o)
                        .collect()
                };
                match reached_after(ctx, st, &[w], &rtargets, Some(&wbarriers)) {
                    Some(h) if h.is_empty() => {}
                    Some(h) => sfno!(name, "the source is read past a store into the temp ({} reads)", h.len()),
                    None => sfno!(name, "walk budget"),
                }
            }
        }
    }
    if !crate::bisect_in("memcpyopt-srcfwd", bname) {
        return false;
    }
    // `(rebase pointer, offset)`: the resolved base for the fixed-base
    // sources, the copy's own operand for a param-carried one.
    let (rb, roff, rwhat) = match &src {
        Src::Param(s, off) | Src::Alloca(s, off) => (*s, *off, "base"),
        Src::BParam(sv, _) => (*sv, 0, "param"),
    };
    if pliron_cfg::tracing_on() {
        tracing::debug!(target: "pliron::memcpyopt", "srcfwd: {n}-byte temp reads {rwhat} at +{roff}: {} copies, {} reads, {} stores ({})",
            copies.len(), readers.len(), wstores.len(), name.unwrap_or(""));
    }
    for op in copies.into_iter().chain(markers) {
        st.intrinsics.remove(op);
        Operation::erase(op, ctx);
    }
    let base = crate::sroa::ptr_plus(ctx, st, rb, roff, at);
    t.replace_all_uses_with(ctx, &base);
    st.allocas.remove(t);
    st.promoted.remove(t);
    Operation::erase(t_op, ctx);
    true
}

fn srcfwd_ranges() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| crate::env_is("PLIRON_SRCFWD_RANGES", "1"));
    *ON
}

/// `p`'s byte offset in temp `t`, through constant GEPs.
fn temp_off(ctx: &Context, st: &State, t: Value, mut p: Value) -> Option<i64> {
    let mut off = 0i64;
    for _ in 0..16 {
        if p == t {
            return Some(off);
        }
        let d = p.defining_op()?;
        Operation::get_op::<GetElementPtrOp>(d, ctx)?;
        off = off.checked_add(gep_offset(ctx, st, d)?)?;
        p = d.deref(ctx).get_operand(ctx, 0);
    }
    None
}

/// The bytes of temp `t` that `op` reads or (drop glue) writes: a load, a
/// mem* copy out of it, or a drop of a value in it (`arg_extent`, rustc's
/// pointee size for `drop_in_place::<T>`). `None` when unknown.
fn temp_access(ctx: &Context, st: &State, t: Value, op: Ptr<Operation>) -> Option<(i64, i64)> {
    let d = op.deref(ctx);
    if Operation::is_op::<LoadOp>(op, ctx) {
        let o = temp_off(ctx, st, t, d.get_operand(ctx, 0))?;
        let n = crate::types::size_align(ctx, d.get_result(ctx, 0).get_type(ctx)).0 as i64;
        return Some((o, o + n));
    }
    // A `PLIRON_SRCFWD_WSTORE` store into the temp: the write side's
    // temp range (`store`'s operands are `[value, ptr]`).
    if Operation::is_op::<StoreOp>(op, ctx) {
        let o = temp_off(ctx, st, t, d.get_operand(ctx, 1))?;
        let n = crate::types::size_align(ctx, d.get_operand(ctx, 0).get_type(ctx)).0 as i64;
        return Some((o, o + n));
    }
    if let Some(nm) = st.intrinsics.get(op).map(|s| s.as_str())
        && matches!(nm, "llvm.memcpy" | "llvm.memmove")
    {
        let o = temp_off(ctx, st, t, d.get_operand(ctx, 1))?;
        let n = const_int(ctx, st, d.get_operand(ctx, 2)).and_then(|x| i64::try_from(x).ok())?;
        return Some((o, o + n));
    }
    if Operation::is_op::<CallOp>(op, ctx) {
        let info = st.calls.get(op)?;
        for (k, a) in d.operands(ctx).enumerate() {
            if let Some(o) = temp_off(ctx, st, t, a) {
                let n = info.arg_extent.as_ref()?.get(k)?.0 as i64;
                return (n > 0).then_some((o, o + n));
            }
        }
    }
    None
}

fn srcfwd_ro() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| crate::env_is("PLIRON_SRCFWD_RO", "1"));
    *ON
}

thread_local! {
    /// What made the last `src_touches` walk call its param captured (for the `srcfwd` trace).
    static CAPTURE_WHY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Every op that touches param `s` (`size` dereferenceable bytes) through
/// GEPs, with the byte ranges it may read or write, and whether its
/// address may outlive an op (a call that may keep it, a store of it as a
/// value, a block argument, an integer). Drop glue reads and writes.
/// `None` when the walk runs out of budget.
fn src_touches(
    ctx: &Context,
    st: &State,
    s: Value,
    size: u64,
) -> Option<(FxHashMap<Ptr<Operation>, Vec<Touch>>, bool)> {
    let mut out: FxHashMap<Ptr<Operation>, Vec<Touch>> = FxHashMap::default();
    let mut captured = false;
    let mut why: Option<String> = None;
    // (pointer, its offset when constant)
    let mut work: Vec<(Value, Option<u64>)> = vec![(s, Some(0))];
    let mut seen = 0usize;
    let touch = |read, write, lo: Option<u64>, n: Option<u64>| match (lo, n) {
        (Some(lo), Some(n)) => Touch { read, write, lo, hi: lo.saturating_add(n), kill: false },
        _ => Touch { read, write, lo: 0, hi: size.max(1), kill: false },
    };
    while let Some((p, base)) = work.pop() {
        for u in p.uses_iter(ctx) {
            seen += 1;
            if seen > 4096 {
                return None;
            }
            let op = u.user_op();
            let i = u.find_index(ctx);
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if i != 0 {
                    captured = true;
                    if why.is_none() { why = Some(format!("gep index {}{}", Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.as_deref()).map(|c| format!(" {}", &c[..c.len().min(100)])).unwrap_or_default())); }
                    continue;
                }
                let nb = match (base, gep_offset(ctx, st, op)) {
                    (Some(b), Some(o)) => u64::try_from(b as i64 + o).ok().filter(|&x| x <= size),
                    _ => None,
                };
                work.push((op.deref(ctx).get_result(ctx, 0), nb));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                let w = size_align(ctx, op.deref(ctx).get_result(ctx, 0).get_type(ctx)).0;
                out.entry(op).or_default().push(touch(true, false, base, Some(w)));
            } else if Operation::is_op::<StoreOp>(op, ctx) {
                if i != 1 {
                    captured = true;
                    if why.is_none() { why = Some(format!("stored as a value {}{}", Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.as_deref()).map(|c| format!(" {}", &c[..c.len().min(100)])).unwrap_or_default())); }
                    continue;
                }
                let w = size_align(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx)).0;
                out.entry(op).or_default().push(touch(false, true, base, Some(w)));
            } else if Operation::is_op::<ICmpOp>(op, ctx) {
                // Comparing an address neither reads nor keeps it.
            } else if let Some(name) = st.intrinsics.get(op).map(|s| s.as_str()) {
                let opnds: Vec<Value> = op.deref(ctx).operands(ctx).collect();
                match name {
                    "llvm.lifetime.start" | "llvm.lifetime.end" => {}
                    "llvm.memcpy" | "llvm.memmove" | "llvm.memset" if i == 0 || (i == 1 && name != "llvm.memset") => {
                        let n = const_int(ctx, st, opnds[2]).and_then(|n| u64::try_from(n).ok());
                        out.entry(op).or_default().push(touch(i == 1, i == 0, base, n));
                    }
                    _ => {
                        captured = true;
                    if why.is_none() { why = Some(format!("intrinsic {}{}", Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.as_deref()).map(|c| format!(" {}", &c[..c.len().min(100)])).unwrap_or_default())); }
                        out.entry(op).or_default().push(touch(true, true, None, None));
                    }
                }
            } else if let Some(c) = Operation::get_op::<CallOp>(op, ctx) {
                let direct = c.get_attr_llvm_call_callee(ctx).is_some();
                let ai = i.wrapping_sub(usize::from(!direct));
                let info = st.calls.get(op);
                let drop = info
                    .and_then(|i| i.callee.as_deref())
                    .is_some_and(|s| s.contains("4core3ptr9drop_glue") || s.contains("13drop_in_place"));
                // An indirect call whose callee is a constant function address
                // (a vtable slot `constload2` folded after the last `devirt`)
                // reads that function's facts.
                // (or a join of them: `bailout`'s recovery functions)
                let const_callees = (!direct)
                    .then(|| op.deref(ctx).get_operand(ctx, 0))
                    .and_then(|cv| crate::helpers::const_callees(ctx, st, cv));
                let const_callee = const_callees.as_ref().map(|cs| cs.join(","));
                let nocap = (direct || i != 0)
                    && (drop
                        || info.and_then(|i| i.nocap.as_ref()).is_some_and(|n| n.get(ai) == Some(&true))
                        || c
                            .get_attr_llvm_call_callee(ctx)
                            .and_then(|id| st.ident_to_sym.get(AsRef::<str>::as_ref(id.as_ref())))
                            .and_then(|sym| st.nocapture.get(sym))
                            .is_some_and(|s| s.contains(&(ai as u32)))
                        || const_callees.as_ref().is_some_and(|cs| {
                            cs.iter().all(|sym| st.nocapture.get(sym).is_some_and(|s| s.contains(&(ai as u32))))
                        }));
                let ext = info.and_then(|i| i.arg_extent.as_ref()).and_then(|e| e.get(ai)).map(|e| e.0).filter(|&n| n > 0);
                // `PLIRON_SRCFWD_RO=1`: a frozen `&T` (rustc's `readonly` +
                // `noalias` on the operand) is a read for the call's
                // duration, and a copy kept past it can never be used to
                // write (a write through a shared borrow is UB). lol_html's
                // compiled selector matchers get `&ctx.stack_item.local_name`
                // through a `dyn Fn` no analysis can resolve.
                let frozen = (direct || i != 0)
                    && srcfwd_ro()
                    && info.and_then(|i| i.arg_ro.as_ref()).is_some_and(|r| r.get(ai) == Some(&true))
                    && info.and_then(|i| i.arg_noalias.as_ref()).is_some_and(|r| r.get(ai) == Some(&true));
                if frozen {
                    out.entry(op).or_default().push(touch(true, false, base, ext));
                    continue;
                }
                if !nocap {
                    captured = true;
                    if why.is_none() { why = Some(format!("call {}{} arg {ai}{}", Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.as_deref()).map(|c| format!(" {}", &c[..c.len().min(100)])).unwrap_or_default(), const_callee.as_deref().map(|c| format!(" const callee {}", &c[..c.len().min(100)])).unwrap_or_else(|| if direct { String::new() } else {
                        let cv = op.deref(ctx).get_operand(ctx, 0);
                        match cv.defining_op() {
                            Some(d) => format!(" callee from {} {} in {}", Operation::get_opid(d, ctx), pliron::operation::OpDbg { op: d, ctx }.to_string().chars().take(100).collect::<String>(), pliron::operation::OpDbg { op, ctx }.to_string().chars().take(220).collect::<String>()),
                            None => format!(" callee is a block arg; const {:?}", st.consts.get(cv)),
                        }
                    }))); }
                }
                out.entry(op).or_default().push(touch(true, true, base, ext));
            } else {
                // A block argument, a select, an integer: the address goes
                // where this walk does not follow.
                captured = true;
                    if why.is_none() { why = Some(format!("other use {}{}", Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.as_deref()).map(|c| format!(" {}", &c[..c.len().min(100)])).unwrap_or_default())); }
                out.entry(op).or_default().push(touch(true, true, None, None));
            }
        }
    }
    if captured {
        CAPTURE_WHY.with(|w| *w.borrow_mut() = why);
    }
    Some((out, captured))
}

/// `op` may write memory that is not a local alloca's (conservative:
/// unknown ops may).
fn may_write_nonlocal(ctx: &Context, st: &State, op: Ptr<Operation>) -> bool {
    if Operation::is_op::<LoadOp>(op, ctx)
        || Operation::is_op::<GetElementPtrOp>(op, ctx)
        || Operation::is_op::<AllocaOp>(op, ctx)
        || Operation::is_op::<ICmpOp>(op, ctx)
    {
        return false;
    }
    // An alloca, or a fresh allocation made in this function: neither
    // can share the param's bytes.
    let local = |v: Value| alloca_base(ctx, st, None, v).is_some() || fresh_alloc(ctx, st, v);
    if Operation::is_op::<StoreOp>(op, ctx) {
        return !local(op.deref(ctx).get_operand(ctx, 1));
    }
    if let Some(name) = st.intrinsics.get(op).map(|s| s.as_str()) {
        return match name {
            "llvm.lifetime.start" | "llvm.lifetime.end" | "llvm.assume" | "llvm.expect" | "pliron.eh.exn" => false,
            "llvm.memcpy" | "llvm.memmove" | "llvm.memset" => !local(op.deref(ctx).get_operand(ctx, 0)),
            _ => true,
        };
    }
    if Operation::is_op::<CallOp>(op, ctx) {
        let callee = st.calls.get(op).and_then(|i| i.callee.as_deref());
        return !callee.is_some_and(|c| st.nowrite.get(c) == Some(&true) || alloc_shim(c));
    }
    if op.deref(ctx).num_regions() == 0 && op.deref(ctx).get_num_results() > 0 {
        let id = Operation::get_opid(op, ctx).to_string();
        if crate::irstat::is_pure(&id)
            || matches!(id.as_str(), "llvm.gep" | "llvm.extract_value" | "llvm.insert_value" | "llvm.undef" | "llvm.constant" | "llvm.zero")
        {
            return false;
        }
    }
    let id = Operation::get_opid(op, ctx).to_string();
    !matches!(id.as_str(), "llvm.br" | "llvm.cond_br" | "llvm.switch" | "llvm.return" | "llvm.unreachable")
}

/// The allocator entry points that hand out fresh memory and write none
/// the program can see (`__rust_alloc`, `__rust_alloc_zeroed`, the shim
/// marker); not `realloc`/`dealloc`.
fn alloc_shim(sym: &str) -> bool {
    sym.ends_with("___rust_alloc")
        || sym.ends_with("___rust_alloc_zeroed")
        || sym.contains("___rust_no_alloc_shim_is_unstable")
}

/// `v` is, through GEPs, the result of an [`alloc_shim`] call.
fn fresh_alloc(ctx: &Context, st: &State, v: Value) -> bool {
    let mut v = v;
    for _ in 0..16 {
        let Some(d) = v.defining_op() else { return false };
        if Operation::is_op::<GetElementPtrOp>(d, ctx) {
            v = d.deref(ctx).get_operand(ctx, 0);
            continue;
        }
        return Operation::is_op::<CallOp>(d, ctx)
            && st.calls.get(d).and_then(|i| i.callee.as_deref()).is_some_and(|c| c.ends_with("___rust_alloc") || c.ends_with("___rust_alloc_zeroed"));
    }
    false
}

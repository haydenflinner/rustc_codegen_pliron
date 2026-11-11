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
use pliron_llvm::ops::{AllocaOp, CallOp, GetElementPtrOp, LoadOp, StoreOp};
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
        other.iter().any(|&b| !(d.dominates(mb, b) || d.dominates(b, pb)))
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
    for &(w, _, _) in &writers {
        if !op_dominates(ctx, st, dom, f, w, m) {
            why!("writer does not dominate the copy");
        }
    }
    // A `memcpy(tmp, tmp)` would rebase into `memcpy(p, p)` — overlap an
    // `llvm.memcpy` forbids. Rare enough to refuse outright.
    if readers.iter().any(|r| writers.iter().any(|w| w.0 == r.0)) {
        why!("self-copy through the temp");
    }
    // Where `tmp`'s range lands in `dst`: the caller's `sret` arg at
    // offset 0, or a constant-offset field of an alloca it fits inside.
    let (dbase, doff) = if sret_args.contains(&dst) {
        (dst, 0)
    } else {
        let Some((base, off)) = alloca_base(ctx, st, inc_of(ctx, f, pin), dst) else {
            if pliron_cfg::tracing_on() {
                let kind = match dst.defining_op() {
                    None => "block-arg",
                    Some(d) if Operation::is_op::<LoadOp>(d, ctx) => "load",
                    Some(d) if st.calls.get(d).is_some() => "call",
                    Some(d) => {
                        tracing::debug!(
                            target: "pliron::memcpyopt",
                            "dst base is {}",
                            Operation::get_opid(d, ctx)
                        );
                        "other"
                    }
                };
                tracing::debug!(target: "pliron::memcpyopt", "dst unbased: {kind}");
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
                if crate::types::leaf_is_ptr(ctx, x.get_type(ctx)).contains(&true)
                    && !disjoint(ctx, st, inc_of(ctx, f, pin), sret_args, x, dst)
                {
                    why!("call writer's arg may alias dst");
                }
            }
        } else if matches!(
            st.intrinsics.get(w).map(|s| s.as_str()),
            Some("llvm.memcpy" | "llvm.memmove")
        ) {
            let x = w.deref(ctx).operands(ctx).nth(1).unwrap();
            if alloca_base(ctx, st, inc_of(ctx, f, pin), x).is_some_and(|(b, _)| b == src)
                || !disjoint(ctx, st, inc_of(ctx, f, pin), sret_args, x, dst)
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
    // Between a writer and `m`, nothing may touch the range that writer
    // commits to `dst` — it would observe the early write. Ops outside
    // the range keep reading `dst`'s old bytes, which is what they saw
    // before `m` ran anyway.
    let size = st.allocas.get(dbase).map(|x| x.0).unwrap_or(u64::MAX);
    for &(w, lo, hi) in &writers {
        let Some(mut set) = base_touches(ctx, st, dbase, size, doff + lo, hi - lo) else {
            why!("dst use-cone not fully walkable");
        };
        set.remove(&m);
        set.remove(&w);
        if any_between(ctx, st, dom, f, m, w, &set) {
            why!("dst touched in writer-to-copy window");
        }
    }
    // A reader after `m` reads `tmp`'s final bytes — `dst`'s after the
    // rebase — so the copy must dominate it (a path skipping `m` could
    // miss writers entirely) and nothing may touch its range in `dst`
    // in between (a write would substitute bytes `tmp` never held).
    for &(r, ro, rh) in &readers {
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
        let Some(mut set) = base_touches(ctx, st, dbase, size, doff + ro, rh - ro) else {
            why!("dst use-cone not fully walkable");
        };
        set.remove(&m);
        set.remove(&r);
        for &(w, _, _) in &writers {
            set.remove(&w);
        }
        if any_between(ctx, st, dom, f, r, m, &set) {
            why!("dst touched between copy and post-copy read");
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
    st.allocas.remove(src);
    st.promoted.remove(src);
    Operation::erase(a, ctx);
    true
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
        alloca_base(ctx, st, inc, x),
        alloca_base(ctx, st, inc, dst),
        sret_args.contains(&x),
        sret_args.contains(&dst),
    ) {
        (Some((a, _)), Some((b, _)), _, _) => a != b,
        (Some(_), _, _, true) | (_, Some(_), true, _) => true,
        _ => false,
    }
}

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
) -> Option<FxHashSet<Ptr<Operation>>> {
    let mut out: FxHashSet<Ptr<Operation>> = FxHashSet::default();
    let mut seen = 0usize;
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
                    out.insert(op);
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
                out.insert(op);
                continue;
            }
            // Byte range of the access, when it is statically known.
            let range = if Operation::is_op::<LoadOp>(op, ctx) {
                Some((bo, size_align(ctx, op.deref(ctx).get_result(ctx, 0).get_type(ctx)).0))
            } else if Operation::is_op::<StoreOp>(op, ctx) && i == 1 {
                Some((bo, size_align(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx)).0))
            } else if i <= 1
                && matches!(
                    st.intrinsics.get(op).map(|s| s.as_str()),
                    Some("llvm.memcpy" | "llvm.memmove" | "llvm.memset")
                )
            {
                op.deref(ctx)
                    .operands(ctx)
                    .nth(2)
                    .and_then(|v| const_int(ctx, st, v))
                    .and_then(|n| u64::try_from(n).ok())
                    .map(|n| (bo, n))
            } else {
                None
            };
            if range.is_none_or(|(lo, n)| lo < off + len && off < lo.saturating_add(n)) {
                out.insert(op);
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
    let funcs: Vec<_> = st
        .funcs
        .values()
        .map(|f| (f.op, f.exts.params.clone()))
        .collect();
    for (f, params) in funcs {
        if !opt_body(ctx, st, f) {
            continue;
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
}

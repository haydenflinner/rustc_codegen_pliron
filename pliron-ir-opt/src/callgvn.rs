//! Dedup, hoist and remove calls using the `nowrite`/`nounwind` facts —
//! LLVM's dead-call elimination, `readonly` GVN and LICM-of-readonly-calls
//! in one linear pass.
//!
//! - **block dedup**: `f(a); …; f(a)` in one block with no write between →
//!   reuse the first result. A callee in `st.nowrite` (either value) works:
//!   the earlier call already returned, so a `false` (writes only on
//!   diverging paths) callee wrote nothing either.
//! - **dominated dedup**: same shape across blocks — c1's block dominates
//!   c2's and every path c1→c2 is write-free (a backward walk from c2 over
//!   preds, pruned at c1's block — dominance guarantees it terminates).
//! - **loop hoist**: a call inside a write-free natural loop whose args are
//!   all defined outside it is moved into the unique preheader — one call
//!   instead of one per iteration (LLVM LICM; this is what makes `n` folded
//!   pure calls collapse to a single evaluation).
//! - **dead elim**: unused result from a strictly `nowrite` + `nounwind`
//!   callee → erase.
//!
//! Every consumer also requires `st.willreturn` (LLVM's `willreturn`, here
//! including determinism: volatile/atomic/asm/indirect/unknown-callee ops
//! seed its complement). That is what lets a *still-used* call be erased
//! in favour of an earlier same-args one: same args + same memory ⇒ the
//! second call comes back with the same value, on the same exit — without
//! the fact it could diverge or read differently where the first returned.
//! Speculation (hoist of a call the loop might not run) is equally safe:
//! an infinite loop with no observable interaction is UB under rustc's
//! `mustprogress`, so a willreturn callee may only diverge via UB.

use pliron::{
    basic_block::BasicBlock,
    builtin::op_interfaces::CallOpInterface,
    context::{Context, Ptr},
    operation::Operation,
    printable::Printable,
    value::Value,
};
use pliron_llvm::ops::{
    AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp, FenceOp,
    InlineAsmOp, StoreOp, VAArgOp,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::domcheck::Dom;
use crate::helpers::bodied;
use crate::inline::{blocks, direct_callee, ops};
use crate::nowrite::{PURE, frame_local};
use crate::state::State;

/// The alloca a pointer derives from (same walk as `frame_local`), as a
/// `Value` — the AllocaOp's result.
fn base_alloca(ctx: &Context, mut p: Value) -> Option<Value> {
    loop {
        match p.defining_op() {
            Some(d) if Operation::is_op::<pliron_llvm::ops::AllocaOp>(d, ctx) => return Some(p),
            Some(d) if Operation::is_op::<pliron_llvm::ops::GetElementPtrOp>(d, ctx) => {
                p = d.deref(ctx).get_operand(ctx, 0)
            }
            _ => return None,
        }
    }
}

/// `op` may change memory a dedup/hoist partner can observe: non-frame-local
/// stores, atomics/fences/asm/va_arg, volatile ops, non-pure intrinsics,
/// calls to anything not proven write-free — and frame-local stores whose
/// alloca is handed to a call as a pointer arg (`arg_allocas`): the callee
/// reads through it, so the store is observable there. Mirrors
/// `nowrite.rs`'s body scan.
fn writes(ctx: &Context, st: &State, op: Ptr<Operation>, arg_allocas: &FxHashSet<Value>) -> bool {
    writes_impl(ctx, st, op, arg_allocas)
}

/// `writes` without escaped-alloca awareness — shared with passes (xoridem)
/// that only need "does this op write at all".
pub(crate) fn writes_plain(ctx: &Context, st: &State, op: Ptr<Operation>) -> bool {
    writes_impl(ctx, st, op, &FxHashSet::default())
}

fn writes_impl(
    ctx: &Context,
    st: &State,
    op: Ptr<Operation>,
    arg_allocas: &FxHashSet<Value>,
) -> bool {
    if st.volatile.contains(op) {
        return true;
    }
    macro_rules! is {
        ($t:ty) => {
            Operation::is_op::<$t>(op, ctx)
        };
    }
    if is!(CallOp) {
        // A call itself only writes through its callee; invokes to
        // write-free callees can't write before their pad either.
        return match direct_callee(ctx, st, op) {
            Some(c) => !st.nowrite.contains_key(c),
            None => true,
        };
    }
    (is!(StoreOp) && {
        let a = op.deref(ctx).get_operand(ctx, 1);
        !frame_local(ctx, a) || base_alloca(ctx, a).is_some_and(|b| arg_allocas.contains(&b))
    }) || is!(AtomicRmwOp)
        || is!(AtomicCmpxchgOp)
        || is!(FenceOp)
        || is!(AtomicLoadOp)
        || is!(AtomicStoreOp)
        || is!(InlineAsmOp)
        || is!(VAArgOp)
        || (is!(CallIntrinsicOp)
            && !st
                .intrinsics.get(op)
                .is_some_and(|n| PURE.iter().any(|p| n.starts_with(p))))
}

/// A direct call result can be deduplicated or hoisted: callee is in
/// `nowrite` at all (`true` never writes; `false` writes only on paths
/// that never return — and a deduped-away call follows one that
/// demonstrably returned) *and* in `willreturn` — the second call provably
/// comes back with the same value and can't read nondeterministically.
/// Invokes are excluded (`inv == false`): erasing one removes its unwind
/// edge, so only moves (hoists) may carry them.
fn dedupable(
    ctx: &Context,
    st: &State,
    op: Ptr<Operation>,
    inv: bool,
) -> Option<(String, Vec<Value>, Value)> {
    if !Operation::is_op::<CallOp>(op, ctx)
        || (!inv && st.invokes.contains(op))
        || op.deref(ctx).get_num_results() != 1
    {
        return None;
    }
    if st
        .calls.get(op)
        .is_some_and(|ci| ci.bb || ci.tail || ci.foreign)
    {
        return None;
    }
    let c = direct_callee(ctx, st, op)?;
    if !st.nowrite.contains_key(c) || !st.willreturn.contains(c) {
        return None;
    }
    let args = Operation::get_op::<CallOp>(op, ctx).unwrap().args(ctx);
    Some((c.clone(), args, op.deref(ctx).get_result(ctx, 0)))
}

/// Can `v` be made available in `ph`? Values defined outside the loop body
/// must dominate the preheader; values defined inside are hoisted through
/// `cone` when their whole operand cone is movable — a write-free loop
/// makes every load loop-invariant, and GEPs/arith/other nowrite calls
/// move freely. Body block args (phis) and ops with effects stop the cone.
fn cone_args(
    ctx: &Context,
    st: &State,
    dom: &Dom,
    body: &FxHashSet<Ptr<BasicBlock>>,
    ph: Ptr<BasicBlock>,
    v: Value,
    cone: &mut Vec<Ptr<Operation>>,
    seen_v: &mut FxHashSet<Value>,
    fuel: &mut u32,
    arg_allocas: &FxHashSet<Value>,
    stored_allocas: &FxHashSet<Value>,
) -> bool {
    if *fuel == 0 {
        return false;
    }
    *fuel -= 1;
    let Some(db) = v.get_defining_block(ctx) else {
        // No defining block: an entry-block arg or module constant.
        return true;
    };
    if !body.contains(&db) {
        return db == ph || dom.dominates(db, ph);
    }
    if !seen_v.insert(v) {
        return true;
    }
    let Some(d) = v.defining_op() else {
        return false; // phi of a body block
    };
    if writes(ctx, st, d, arg_allocas) || d.deref(ctx).successors(ctx).next().is_some() {
        return false;
    }
    // A read that reaches the preheader must not look at memory the loop
    // body still stores into. `writes` skips frame-local stores, so they
    // leave a write-free loop eligible — catch the aliasing reads here:
    // a load of a stored alloca, or a movable call taking its pointer.
    let mem_reads = |a: Value| base_alloca(ctx, a).is_some_and(|b| stored_allocas.contains(&b));
    if (Operation::is_op::<pliron_llvm::ops::LoadOp>(d, ctx)
        && mem_reads(d.deref(ctx).get_operand(ctx, 0)))
        || (Operation::is_op::<CallOp>(d, ctx) && d.deref(ctx).operands(ctx).any(|a| mem_reads(a)))
    {
        return false;
    }
    for a in d.deref(ctx).operands(ctx) {
        if !cone_args(
            ctx,
            st,
            dom,
            body,
            ph,
            a,
            cone,
            seen_v,
            fuel,
            arg_allocas,
            stored_allocas,
        ) {
            return false;
        }
    }
    cone.push(d);
    true
}

/// Any write in `os[lo..hi]`? `os` is a pre-erasure snapshot — `dead`
/// holds ops already erased (deref on them is a dangling-Ptr panic).
/// `op_writes` was computed pre-erasure too, so a dead op's entry is
/// still valid to read (it is just skipped).
fn writes_between(
    os: &[Ptr<Operation>],
    lo: usize,
    hi: usize,
    op_writes: &FxHashMap<Ptr<Operation>, bool>,
    dead: &FxHashSet<Ptr<Operation>>,
) -> bool {
    os[lo..hi]
        .iter()
        .any(|&o| !dead.contains(&o) && op_writes.get(&o).copied().unwrap_or(false))
}

/// (callee, args) hash for the dedup tables: the maps key on `u64` +
/// store the full pair in the bucket, so lookups pay a hash instead of
/// a `String` + `Vec` clone per candidate op.
fn ckey(c: &str, args: &[Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    c.hash(&mut h);
    args.hash(&mut h);
    h.finish()
}

/// Every path from `src`'s block to `dst`'s (distinct, dominated-by-src)
/// block is write-free? Backward walk from dst over preds, pruned at src —
/// dominance guarantees every backward path ends there.
fn path_write_free(
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
    writes_blk: &FxHashSet<Ptr<BasicBlock>>,
    src: Ptr<BasicBlock>,
    dst: Ptr<BasicBlock>,
) -> bool {
    let mut seen = FxHashSet::default();
    seen.insert(dst);
    seen.insert(src);
    let mut wl = vec![dst];
    while let Some(b) = wl.pop() {
        if seen.len() > 4096 {
            return false;
        }
        for &p in preds.get(&b).into_iter().flatten() {
            if !seen.insert(p) || p == src {
                continue;
            }
            if writes_blk.contains(&p) {
                return false;
            }
            wl.push(p);
        }
    }
    true
}

/// Natural loops keyed by header: latch → header back edges via the
/// dominator tree, bodies collected by backward reachability.
fn natural_loops(
    dom: &Dom,
    succ: &FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
) -> Vec<(Ptr<BasicBlock>, FxHashSet<Ptr<BasicBlock>>)> {
    let mut latched: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
    for &b in &dom.order {
        for &s in succ.get(&b).into_iter().flatten() {
            if dom.dominates(s, b) {
                latched.entry(s).or_default().push(b);
            }
        }
    }
    let mut out = Vec::new();
    for (h, latches) in latched {
        let mut body = FxHashSet::default();
        body.insert(h);
        let mut ok = true;
        for l in latches {
            let mut wl = vec![l];
            while let Some(b) = wl.pop() {
                if b == h || !body.insert(b) {
                    continue;
                }
                if body.len() > 4096 {
                    ok = false;
                    break;
                }
                wl.extend(preds.get(&b).into_iter().flatten().copied());
            }
            if !ok {
                break;
            }
        }
        if ok {
            out.push((h, body));
        }
    }
    out
}

pub fn run(ctx: &mut Context, st: &State) {
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::callgvn", tracing::Level::DEBUG);
    let dumpf = crate::env_get("PLIRON_CGDUMP");
    for name in bodied(ctx, st) {
        if st.opt_skip.contains(&st.funcs[&name].op) {
            continue;
        }
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::callgvn", "callgvn fn={name}")
            };
        }
        if dumpf.as_ref().is_some_and(|s| name.contains(s)) {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::dump", "{}",
                    st.funcs[&name]
                        .op
                        .print(ctx, &pliron::printable::State::default())
                )
            };
        }
        run_fn(ctx, st, st.funcs[&name].op);
        if crate::env_get("PLIRON_CGDUMP2").is_some_and(|s| name.contains(s)) {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::dump", "==== after callgvn ====\n{}",
                    st.funcs[&name]
                        .op
                        .print(ctx, &pliron::printable::State::default())
                )
            };
        }
    }
}

fn run_fn(ctx: &mut Context, st: &State, fop: Ptr<Operation>) {
    let bs = blocks(ctx, fop);
    if bs.is_empty() {
        return;
    }
    let Some(dom) = Dom::new(ctx, st, fop) else {
        return;
    };
    let bops: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<Operation>>> =
        bs.iter().map(|&b| (b, ops(ctx, b))).collect();
    // Write-flag per block plus succ/pred edges for the path & loop walks.
    // Allocas handed to a call anywhere in the function: a callee reads
    // through pointer args, so a store into one of these is an observable
    // write for dedup/hoist purposes even though `frame_local` hides it
    // from `nowrite`'s callee classification.
    let mut arg_allocas = FxHashSet::default();
    for &b in &bs {
        for &op in &bops[&b] {
            if Operation::is_op::<CallOp>(op, ctx) {
                for a in op.deref(ctx).operands(ctx) {
                    if let Some(ba) = base_alloca(ctx, a) {
                        arg_allocas.insert(ba);
                    }
                }
            }
        }
    }
    let mut writes_blk = FxHashSet::default();
    // Per-op write flag: `writes` is consulted per op here, again in the
    // dedup scan, and per range query in `writes_between` — cache it once
    // `arg_allocas` (its only function-global input) is complete.
    let mut op_writes: FxHashMap<Ptr<Operation>, bool> = FxHashMap::default();
    let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
    let mut succ: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
    for &b in &bs {
        let mut wb = false;
        for &op in &bops[&b] {
            let w = writes(ctx, st, op, &arg_allocas);
            op_writes.insert(op, w);
            wb |= w;
        }
        if wb {
            writes_blk.insert(b);
        }
        for op in &bops[&b] {
            for s in op.deref(ctx).successors(ctx) {
                preds.entry(s).or_default().push(b);
                succ.entry(b).or_default().push(s);
            }
            if let Some(&(l, _)) = st.invokes.get(*op) {
                preds.entry(l).or_default().push(b);
                succ.entry(b).or_default().push(l);
            }
        }
    }
    let mut gone = FxHashSet::default();
    // `dedupable` runs per op in each of the three passes below; its
    // inputs (op shape, `st` fact tables) are all pass-stable, so the
    // result is memoized per (op, invoke-allowed).
    let mut dmemo: FxHashMap<(Ptr<Operation>, bool), Option<(String, Vec<Value>, Value)>> =
        FxHashMap::default();

    // 1) Block-local dedup + dead-call elim.
    let mut index: FxHashMap<
        u64,
        Vec<(String, Vec<Value>, Ptr<BasicBlock>, usize, Ptr<Operation>)>,
    > = FxHashMap::default();
    for &b in &bs {
        let mut seen: FxHashMap<u64, Vec<(String, Vec<Value>, Value)>> = FxHashMap::default();
        let os = &bops[&b];
        for (i, &op) in os.iter().enumerate() {
            if op_writes.get(&op).copied().unwrap_or(false) {
                seen.clear();
            }
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::callgvn", tracing::Level::DEBUG)
                && Operation::is_op::<CallOp>(op, ctx)
            {
                let why = if st.invokes.contains(op) {
                    "invoke"
                } else if op.deref(ctx).get_num_results() != 1 {
                    "results"
                } else if st
                    .calls.get(op)
                    .is_some_and(|ci| ci.bb || ci.tail || ci.foreign)
                {
                    "flags"
                } else if direct_callee(ctx, st, op).is_none() {
                    "indirect"
                } else {
                    "ok"
                };
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::callgvn", "callgvn   call dedupable={why}")
                };
            }
            let Some((c, args, res)) = dmemo
                .entry((op, false))
                .or_insert_with(|| dedupable(ctx, st, op, false))
                .as_ref()
            else {
                continue;
            };
            // Dead: nowrite + nounwind + unused result (willreturn comes
            // free through `dedupable` — LLVM's `isRemovable` triple).
            if res.uses(ctx).is_empty()
                && st.nowrite.get(c) == Some(&true)
                && st.funcs.get(c).is_some_and(|f| f.nounwind)
            {
                gone.insert(op);
                continue;
            }
            let h = ckey(c, args);
            let prev = seen
                .get(&h)
                .and_then(|bkt| bkt.iter().find(|(c2, a2, _)| c2 == c && a2 == args))
                .map(|e| e.2);
            if let Some(prev) = prev {
                res.replace_all_uses_with(ctx, &prev);
                gone.insert(op);
                continue;
            }
            seen.entry(h)
                .or_default()
                .push((c.clone(), args.clone(), *res));
            index
                .entry(h)
                .or_default()
                .push((c.clone(), args.clone(), b, i, op));
        }
    }
    for &op in &gone {
        Operation::erase(op, ctx);
    }

    // 2) Dominated-block dedup: c1 dominates c2, write-free c1→c2 paths.
    let mut killed = FxHashSet::default();
    for &b in &bs {
        let os = &bops[&b];
        for (i, &op) in os.iter().enumerate() {
            if gone.contains(&op) {
                continue;
            }
            let Some((c, args, res)) = dmemo
                .entry((op, false))
                .or_insert_with(|| dedupable(ctx, st, op, false))
                .as_ref()
            else {
                continue;
            };
            let Some(cands) = index.get(&ckey(c, args)) else {
                continue;
            };
            for e in cands {
                let (c2, a2, cb, ci, cop) = e;
                if c2 != c || a2 != args {
                    continue; // hash-collision guard
                }
                if gone.contains(cop) || killed.contains(cop) || cb == &b || !dom.dominates(*cb, b)
                {
                    continue;
                }
                // No writes after c1 in its block, none before c2 in ours,
                // none in any block on a c1→c2 path.
                if writes_between(&bops[cb], *ci + 1, bops[cb].len(), &op_writes, &gone)
                    || writes_between(os, 0, i, &op_writes, &gone)
                    || !path_write_free(&preds, &writes_blk, *cb, b)
                {
                    continue;
                }
                let prev = cop.deref(ctx).get_result(ctx, 0);
                res.replace_all_uses_with(ctx, &prev);
                killed.insert(op);
                break;
            }
        }
    }
    for &op in &killed {
        Operation::erase(op, ctx);
    }

    // 3) Loop hoist: write-free natural loops with a unique preheader.
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::callgvn", tracing::Level::DEBUG);
    for (h, body) in natural_loops(&dom, &succ, &preds) {
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::callgvn", "callgvn loop h={h:?} body={} w={:?}", body.len(),
                body.iter().filter(|b| writes_blk.contains(b)).count())
            };
        }
        if body.iter().any(|b| writes_blk.contains(b)) {
            continue;
        }
        let outside: Vec<Ptr<BasicBlock>> = preds
            .get(&h)
            .into_iter()
            .flatten()
            .copied()
            .filter(|p| !body.contains(p))
            .collect();
        let [ph] = outside[..] else {
            if dbg {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::callgvn", "callgvn   no unique ph: {} outside preds", outside.len())
                };
            }
            continue;
        };
        let Some(term) = ph.deref(ctx).get_terminator(ctx) else {
            continue;
        };
        // Header must dominate the whole body (reducible) for the
        // preheader to dominate it too.
        if body.iter().any(|&b| b != h && !dom.dominates(h, b)) {
            if dbg {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::callgvn", "callgvn   header doesn't dominate body")
                };
            }
            continue;
        }
        // Allocas the body still writes: a hoisted cone must not read
        // them before their last store ran.
        let mut stored_allocas = FxHashSet::default();
        for &b in &body {
            for &op in &bops[&b] {
                if gone.contains(&op) || killed.contains(&op) {
                    continue;
                }
                if Operation::is_op::<StoreOp>(op, ctx)
                    && let Some(ba) = base_alloca(ctx, op.deref(ctx).get_operand(ctx, 1))
                {
                    stored_allocas.insert(ba);
                }
            }
        }
        for &b in &body {
            for &op in &bops[&b] {
                if gone.contains(&op) || killed.contains(&op) {
                    continue;
                }
                // An earlier hoist may have moved this op into `ph` as
                // part of another call's operand cone — re-moving it
                // would place it after the call that now uses it.
                if op.deref(ctx).get_parent_block() == Some(ph) {
                    continue;
                }
                if dbg && Operation::is_op::<CallOp>(op, ctx) {
                    let why = if op.deref(ctx).get_num_results() != 1 {
                        "results"
                    } else if st
                        .calls.get(op)
                        .is_some_and(|ci| ci.bb || ci.tail || ci.foreign)
                    {
                        "flags"
                    } else if direct_callee(ctx, st, op).is_none() {
                        "indirect"
                    } else if !st.nowrite.contains_key(direct_callee(ctx, st, op).unwrap()) {
                        "writer"
                    } else if !st.willreturn.contains(direct_callee(ctx, st, op).unwrap()) {
                        "diverge"
                    } else {
                        "ok"
                    };
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::callgvn", "callgvn   call in body dedupable={why}")
                    };
                }
                // Hoists may carry invokes: the unwind edge is an
                // `st.invokes` entry keyed on the op, so it moves with it.
                let Some((c, args, _)) = dmemo
                    .entry((op, true))
                    .or_insert_with(|| dedupable(ctx, st, op, true))
                    .as_ref()
                else {
                    continue;
                };
                // Any `nowrite` entry works: a `false` callee writes only
                // on paths that never return, so speculating the call
                // behaves identically on every returning execution. An
                // unwind out of a plain call leaves this frame the same
                // way from the preheader as from the loop body. (`dedupable`
                // already required `willreturn`, the part that makes
                // speculation itself safe.)
                if !st.nowrite.contains_key(c) {
                    continue;
                }
                // An invoke whose landing pad is inside the loop must stay
                // put: hoisting it adds a `ph → pad` edge that bypasses the
                // header, so the header stops dominating the pad and every
                // header-defined value used downstream of it.
                if st.invokes.get(op).is_some_and(|&(l, _)| body.contains(&l)) {
                    continue;
                }
                // Args need to reach the preheader: values defined inside
                // the body are hoisted along with the call when their whole
                // operand cone is movable (a write-free loop makes every
                // load loop-invariant; GEPs/arith/other nowrite calls move
                // freely). Block args (phis) and anything with effects stop
                // the cone.
                let mut cone: Vec<Ptr<Operation>> = Vec::new();
                let mut seen_v = FxHashSet::default();
                let mut fuel = 128;
                // The call's own pointer args must not reach into an
                // alloca the body still stores — cone_args only screens
                // ops *inside* the cone.
                let ok = args
                    .iter()
                    .all(|&a| base_alloca(ctx, a).is_none_or(|b| !stored_allocas.contains(&b)))
                    && args.iter().all(|&a| {
                        cone_args(
                            ctx,
                            st,
                            &dom,
                            &body,
                            ph,
                            a,
                            &mut cone,
                            &mut seen_v,
                            &mut fuel,
                            &arg_allocas,
                            &stored_allocas,
                        )
                    });
                if dbg {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::callgvn", "callgvn   {c} hoistable={ok} cone={}", cone.len())
                    };
                }
                if !ok {
                    continue;
                }
                for d in cone {
                    d.unlink(ctx);
                    d.insert_before(ctx, term);
                }
                op.unlink(ctx);
                op.insert_before(ctx, term);
            }
        }
    }
}

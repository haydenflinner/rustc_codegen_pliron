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
    printable::Printable,
    builtin::op_interfaces::CallOpInterface,
    context::{Context, Ptr},
    operation::Operation,
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

/// `op` may change memory a caller can observe: non-frame-local stores,
/// atomics/fences/asm/va_arg, volatile ops, non-pure intrinsics, and calls
/// to anything not proven write-free. Mirrors `nowrite.rs`'s body scan.
pub(crate) fn writes(ctx: &Context, st: &State, op: Ptr<Operation>) -> bool {
    if st.volatile.contains(&op) {
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
    (is!(StoreOp) && !frame_local(ctx, op.deref(ctx).get_operand(1)))
        || is!(AtomicRmwOp)
        || is!(AtomicCmpxchgOp)
        || is!(FenceOp)
        || is!(AtomicLoadOp)
        || is!(AtomicStoreOp)
        || is!(InlineAsmOp)
        || is!(VAArgOp)
        || (is!(CallIntrinsicOp)
            && !st
                .intrinsics
                .get(&op)
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
        || (!inv && st.invokes.contains_key(&op))
        || op.deref(ctx).get_num_results() != 1
    {
        return None;
    }
    if st
        .calls
        .get(&op)
        .is_some_and(|ci| ci.bb || ci.tail || ci.foreign)
    {
        return None;
    }
    let c = direct_callee(ctx, st, op)?;
    if !st.nowrite.contains_key(c) || !st.willreturn.contains(c) {
        return None;
    }
    let args = Operation::get_op::<CallOp>(op, ctx).unwrap().args(ctx);
    Some((c.clone(), args, op.deref(ctx).get_result(0)))
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
    if writes(ctx, st, d) || d.deref(ctx).successors().next().is_some() {
        return false;
    }
    for a in d.deref(ctx).operands() {
        if !cone_args(ctx, st, dom, body, ph, a, cone, seen_v, fuel) {
            return false;
        }
    }
    cone.push(d);
    true
}

/// Any write in `os[lo..hi]`?
fn writes_between(ctx: &Context, st: &State, os: &[Ptr<Operation>], lo: usize, hi: usize) -> bool {
    os[lo..hi].iter().any(|&o| writes(ctx, st, o))
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
    let dbg = crate::env_set("PLIRON_CGDBG");
    for name in bodied(ctx, st) {
        if dbg {
            eprintln!("callgvn fn={name}");
        }
        if crate::env_get("PLIRON_CGDUMP").is_some_and(|s| name.contains(s)) {
            eprintln!(
                "{}",
                st.funcs[&name]
                    .op
                    .print(ctx, &pliron::printable::State::default())
            );
        }
        run_fn(ctx, st, st.funcs[&name].op);
        if crate::env_get("PLIRON_CGDUMP2").is_some_and(|s| name.contains(s)) {
            eprintln!(
                "==== after callgvn ====\n{}",
                st.funcs[&name]
                    .op
                    .print(ctx, &pliron::printable::State::default())
            );
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
    let mut writes_blk = FxHashSet::default();
    let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
    let mut succ: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
    for &b in &bs {
        if bops[&b].iter().any(|&o| writes(ctx, st, o)) {
            writes_blk.insert(b);
        }
        for op in &bops[&b] {
            for s in op.deref(ctx).successors() {
                preds.entry(s).or_default().push(b);
                succ.entry(b).or_default().push(s);
            }
            if let Some(&(l, _)) = st.invokes.get(op) {
                preds.entry(l).or_default().push(b);
                succ.entry(b).or_default().push(l);
            }
        }
    }
    let mut gone = FxHashSet::default();

    // 1) Block-local dedup + dead-call elim.
    let mut index: FxHashMap<(String, Vec<Value>), Vec<(Ptr<BasicBlock>, usize, Ptr<Operation>)>> =
        FxHashMap::default();
    for &b in &bs {
        let mut seen: FxHashMap<(String, Vec<Value>), Value> = FxHashMap::default();
        let os = &bops[&b];
        for (i, &op) in os.iter().enumerate() {
            if writes(ctx, st, op) {
                seen.clear();
            }
            if crate::env_set("PLIRON_CGDBG") && Operation::is_op::<CallOp>(op, ctx) {
                let why = if st.invokes.contains_key(&op) {
                    "invoke"
                } else if op.deref(ctx).get_num_results() != 1 {
                    "results"
                } else if st
                    .calls
                    .get(&op)
                    .is_some_and(|ci| ci.bb || ci.tail || ci.foreign)
                {
                    "flags"
                } else if direct_callee(ctx, st, op).is_none() {
                    "indirect"
                } else {
                    "ok"
                };
                eprintln!("callgvn   call dedupable={why}");
            }
            let Some((c, args, res)) = dedupable(ctx, st, op, false) else {
                continue;
            };
            // Dead: nowrite + nounwind + unused result (willreturn comes
            // free through `dedupable` — LLVM's `isRemovable` triple).
            if res.uses(ctx).is_empty()
                && st.nowrite.get(&c) == Some(&true)
                && st.funcs.get(&c).is_some_and(|f| f.nounwind)
            {
                gone.insert(op);
                continue;
            }
            match seen.get(&(c.clone(), args.clone())) {
                Some(&prev) => {
                    res.replace_all_uses_with(ctx, &prev);
                    gone.insert(op);
                    continue;
                }
                None => {
                    seen.insert((c.clone(), args.clone()), res);
                }
            }
            index.entry((c, args)).or_default().push((b, i, op));
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
            let Some((c, args, res)) = dedupable(ctx, st, op, false) else {
                continue;
            };
            let Some(cands) = index.get(&(c, args)) else {
                continue;
            };
            for &(cb, ci, cop) in cands {
                if gone.contains(&cop) || killed.contains(&cop) || cb == b || !dom.dominates(cb, b) {
                    continue;
                }
                // No writes after c1 in its block, none before c2 in ours,
                // none in any block on a c1→c2 path.
                if writes_between(ctx, st, &bops[&cb], ci + 1, bops[&cb].len())
                    || writes_between(ctx, st, os, 0, i)
                    || !path_write_free(&preds, &writes_blk, cb, b)
                {
                    continue;
                }
                let prev = cop.deref(ctx).get_result(0);
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
    let dbg = crate::env_set("PLIRON_CGDBG");
    for (h, body) in natural_loops(&dom, &succ, &preds) {
        if dbg {
            eprintln!("callgvn loop h={h:?} body={} w={:?}", body.len(),
                body.iter().filter(|b| writes_blk.contains(b)).count());
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
                eprintln!("callgvn   no unique ph: {} outside preds", outside.len());
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
                eprintln!("callgvn   header doesn't dominate body");
            }
            continue;
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
                        .calls
                        .get(&op)
                        .is_some_and(|ci| ci.bb || ci.tail || ci.foreign)
                    {
                        "flags"
                    } else if direct_callee(ctx, st, op).is_none() {
                        "indirect"
                    } else if !st
                        .nowrite
                        .contains_key(direct_callee(ctx, st, op).unwrap())
                    {
                        "writer"
                    } else if !st
                        .willreturn
                        .contains(direct_callee(ctx, st, op).unwrap())
                    {
                        "diverge"
                    } else {
                        "ok"
                    };
                    eprintln!("callgvn   call in body dedupable={why}");
                }
                // Hoists may carry invokes: the unwind edge is an
                // `st.invokes` entry keyed on the op, so it moves with it.
                let Some((c, args, _)) = dedupable(ctx, st, op, true) else {
                    continue;
                };
                // Any `nowrite` entry works: a `false` callee writes only
                // on paths that never return, so speculating the call
                // behaves identically on every returning execution. An
                // unwind out of a plain call leaves this frame the same
                // way from the preheader as from the loop body. (`dedupable`
                // already required `willreturn`, the part that makes
                // speculation itself safe.)
                if !st.nowrite.contains_key(&c) {
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
                let ok = args
                    .iter()
                    .all(|&a| cone_args(ctx, st, &dom, &body, ph, a, &mut cone, &mut seen_v, &mut fuel));
                if dbg {
                    eprintln!("callgvn   {c} hoistable={ok} cone={}", cone.len());
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

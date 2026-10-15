//! `acc ^= inv` loop-idiom elimination. LLVM reaches this fold through
//! full unrolling (a constant-trip loop expands, then `x ^ x -> 0` cancels
//! pairs); the exit value falls out of the recurrence directly: a
//! loop-carried `acc ^= v` with loop-invariant `v` nets `init` over an
//! even trip and `init ^ v` over an odd one. When the counter is dead
//! outside — the `for i in 0..N { s ^= call(...) }` shape — the whole
//! loop is a no-op and erases, which is what lets a hoisted pure call
//! (already carried to the preheader by callgvn) die in turn.
//!
//! The rewrite never moves an op: iteration 0 executes the body exactly
//! as written, then every dominated back-edge is redirected to the
//! exit. Calls to nowrite+willreturn callees are allowed in the body —
//! the dropped iterations could only have returned values identical to
//! iteration 0's, and unwind/diverge behavior is preserved by the kept
//! copy. On even trip counts the accumulator ends at `s0` regardless of
//! the body, so its stores are erased when nothing else reads the slot
//! — which is what frees the (now unused) call result for callgvn's
//! dead-call elim; a still-read slot falls back to a preheader snapshot
//! restored in the latch.
//!
//! The match is deliberately narrow: a single-entry single-exit loop
//! whose header tests `load c <ULT> K`, whose only in-loop stores are the
//! counter increment `store c <- add(load c, step)`, xor-recurrences
//! `store p <- xor(load p, inv)` and stores of loop-invariant values
//! (one per pointer — replayed once in the preheader when n > 0), where
//! every participating pointer is a bare alloca (whole-slot accesses, so
//! no two pointers can partially alias), and whose interior blocks chain
//! unconditionally to the latch. No calls, no volatile ops, nothing
//! defined inside may be used outside, and the counter needs exactly one
//! store outside the loop — a constant init — with no loads of it outside
//! either (its post-loop value is `init + n*step`, not `init`).

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_llvm::attributes::ICmpPredicateAttr as P;
use pliron_llvm::ops::{
    AddOp, AllocaOp, BrOp, CallIntrinsicOp, CallOp, CondBrOp, GetElementPtrOp, ICmpOp,
    LoadOp, StoreOp, XorOp,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::guardelim::slot_base;
use crate::helpers::{bodied, erase_unreachable};
use crate::inline::{blocks, ops};
use crate::state::State;
use crate::types::{TyK, classify};

/// Blocks a single fold may erase (bounds the backward natural-loop walk
/// and the per-block validation scan).
const MAX_LOOP_BLOCKS: usize = 64;

/// Cap on `arg_inv` cone walks and `loop_private` cone walks.
const MAX_ACC_WALK: usize = 4096;

/// Is `v` loop-invariant: defined by an op in a block outside `lset`, or
/// a block argument of such a block.
fn invariant(ctx: &Context, lset: &FxHashSet<Ptr<BasicBlock>>, v: Value) -> bool {
    if let Some(d) = v.defining_op() {
        return d
            .deref(ctx)
            .get_parent_block()
            .is_some_and(|b| !lset.contains(&b));
    }
    if let Some(b) = v.defining_block() {
        return !lset.contains(&b);
    }
    true
}

fn parent(ctx: &Context, op: Ptr<Operation>) -> Option<Ptr<BasicBlock>> {
    op.deref(ctx).get_parent_block()
}

/// The block that produces `v`: its defining op's block, or the block it
/// is an argument of. None for unattached values (undefs, free results).
fn def_block(ctx: &Context, v: Value) -> Option<Ptr<BasicBlock>> {
    if let Some(d) = v.defining_op() {
        return parent(ctx, d);
    }
    v.defining_block()
}

/// `v` is iteration-invariant over `lset`: defined outside it, produced
/// by an eligible call on invariant args (iteration 0 runs it for real),
/// a non-volatile load of an unstored-in-loop alloca on an invariant
/// pointer, or a pure op over invariant operands. `memo` is a tri-state
/// (absent/0 = visiting, 1 = yes, 2 = no); `fuel` bounds the cone.
fn arg_inv(
    ctx: &Context,
    st: &State,
    lset: &FxHashSet<Ptr<BasicBlock>>,
    sbases: &FxHashSet<Value>,
    memo: &mut FxHashMap<Value, u8>,
    fuel: &mut usize,
    v: Value,
) -> bool {
    match memo.get(&v) {
        Some(&1) => return true,
        Some(_) => return false,
        None => {}
    }
    *fuel = fuel.saturating_sub(1);
    if *fuel == 0 {
        return false;
    }
    memo.insert(v, 0);
    let r = if invariant(ctx, lset, v) {
        true
    } else if let Some(d) = v.defining_op() {
        if Operation::is_op::<LoadOp>(d, ctx) {
            !st.volatile.contains(&d)
                && slot_base(ctx, d.deref(ctx).get_operand(0))
                    .is_some_and(|a| !sbases.contains(&a))
                && arg_inv(
                    ctx,
                    st,
                    lset,
                    sbases,
                    memo,
                    fuel,
                    d.deref(ctx).get_operand(0),
                )
        } else {
            !crate::callgvn::writes_plain(ctx, st, d)
                && d.deref(ctx).successors().next().is_none()
                && (0..d.deref(ctx).get_num_operands()).all(|i| {
                    arg_inv(ctx, st, lset, sbases, memo, fuel, d.deref(ctx).get_operand(i))
                })
        }
    } else {
        v.defining_block().is_none_or(|b| !lset.contains(&b))
    };
    memo.insert(v, if r { 1 } else { 2 });
    r
}

/// Every pointer derived from `a` through gep chains is used only inside
/// `lset`, and only as a memory address (load/store dest), a gep base,
/// or a lifetime intrinsic arg — never as a stored value, call arg, or
/// anything that could carry it out. Then the slot is loop-private and
/// writes to it die with the fold.
fn loop_private(ctx: &Context, st: &State, lset: &FxHashSet<Ptr<BasicBlock>>, a: Value) -> bool {
    let mut seen: FxHashSet<Value> = [a].into_iter().collect();
    let mut wl = vec![a];
    let mut fuel = MAX_ACC_WALK;
    while let Some(v) = wl.pop() {
        for u in v.uses(ctx) {
            let op = u.user_op();
            if !parent(ctx, op).is_some_and(|b| lset.contains(&b)) {
                return false;
            }
            fuel -= 1;
            if fuel == 0 {
                return false;
            }
            let o = op.deref(ctx);
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if u.find_index(ctx) != 0 {
                    return false;
                }
                let r = o.get_result(0);
                if seen.insert(r) {
                    wl.push(r);
                }
            } else if Operation::is_op::<LoadOp>(op, ctx) || Operation::is_op::<StoreOp>(op, ctx) {
                // Fine as the addressed slot; as the stored value or a
                // load offset the pointer escapes.
                let is_addr = if Operation::is_op::<LoadOp>(op, ctx) {
                    u.find_index(ctx) == 0
                } else {
                    u.find_index(ctx) == 1
                };
                if !is_addr {
                    return false;
                }
            } else if Operation::is_op::<CallIntrinsicOp>(op, ctx) {
                if !st
                    .intrinsics
                    .get(&op)
                    .is_some_and(|n| n == "llvm.lifetime.start" || n == "llvm.lifetime.end")
                {
                    return false;
                }
            } else {
                return false;
            }
        }
    }
    true
}

/// `v` is a bare alloca result — loads/stores through it cover the whole
/// slot, so two such pointers can never partially alias.
fn is_alloca(ctx: &Context, v: Value) -> bool {
    v.defining_op()
        .is_some_and(|d| Operation::is_op::<AllocaOp>(d, ctx))
}

/// Erase `v`'s def while it is a single-use `xor`/`load` whose last
/// consumer just died — frees the values the chain itself consumed
/// (the pure call results `invs` resolve to). Bounded by `fuel`.
fn peel_dead(ctx: &mut Context, st: &State, v: Value, fuel: usize) {
    if fuel == 0 || !v.uses(ctx).is_empty() {
        return;
    }
    let Some(d) = v.defining_op() else {
        return;
    };
    let dead = Operation::is_op::<XorOp>(d, ctx)
        || (Operation::is_op::<LoadOp>(d, ctx) && !st.volatile.contains(&d));
    if !dead {
        return;
    }
    let operands: Vec<Value> = (0..d.deref(ctx).get_num_operands())
        .map(|i| d.deref(ctx).get_operand(i))
        .collect();
    Operation::erase(d, ctx);
    for o in operands {
        peel_dead(ctx, st, o, fuel - 1);
    }
}

/// `v` is a non-volatile `load` of pointer `p`.
fn is_load_of(ctx: &Context, st: &State, v: Value, p: Value) -> bool {
    v.defining_op().is_some_and(|d| {
        Operation::is_op::<LoadOp>(d, ctx)
            && !st.volatile.contains(&d)
            && d.deref(ctx).get_operand(0) == p
    })
}

fn run_fn(ctx: &mut Context, st: &State, fop: Ptr<Operation>) -> usize {
    let bs = blocks(ctx, fop);
    let bops: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<Operation>>> =
        bs.iter().map(|&b| (b, ops(ctx, b))).collect();
    let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
    for &b in &bs {
        for op in &bops[&b] {
            for s in op.deref(ctx).successors() {
                preds.entry(s).or_default().push(b);
            }
        }
    }

    let dom = crate::domcheck::Dom::new(ctx, st, fop);
    let mut folds = 0;
    let mut claimed: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
    for &h in &bs {
        if claimed.contains(&h) {
            continue;
        }
        let Some(&term) = bops[&h].last() else {
            continue;
        };
        if !Operation::is_op::<CondBrOp>(term, ctx) {
            continue;
        }
        let cond = term.deref(ctx).get_operand(0);
        let Some(ci) = cond.defining_op() else {
            continue;
        };
        let Some(icmp) = Operation::get_op::<ICmpOp>(ci, ctx) else {
            continue;
        };
        if icmp.predicate(ctx) != P::ULT || parent(ctx, ci) != Some(h) {
            continue;
        }
        let (x, y) = (ci.deref(ctx).get_operand(0), ci.deref(ctx).get_operand(1));
        let Some(bound) = crate::sroa::const_int(ctx, st, y) else {
            continue;
        };
        let Some(cl) = x.defining_op() else {
            continue;
        };
        // The counter test must be a plain alloca load in the header.
        if !Operation::is_op::<LoadOp>(cl, ctx)
            || st.volatile.contains(&cl)
            || parent(ctx, cl) != Some(h)
        {
            continue;
        }
        let c_ptr = cl.deref(ctx).get_operand(0);
        if !is_alloca(ctx, c_ptr) {
            continue;
        }
        let c_slot = c_ptr; // alloca-direct
        if h.deref(ctx).get_num_arguments() > 0 {
            continue;
        }
        // succ0 = body (cond true), succ1 = exit.
        let (body0, e) = (
            term.deref(ctx).get_successor(0),
            term.deref(ctx).get_successor(1),
        );
        if e == h || body0 == e || e.deref(ctx).get_num_arguments() > 0 {
            continue;
        }
        // Natural loop: latches are h's preds dominated by h; the body is
        // their backward cone without crossing h.
        let hpreds: Vec<Ptr<BasicBlock>> =
            preds.get(&h).into_iter().flatten().copied().collect();
        let latches: Vec<Ptr<BasicBlock>> = hpreds
            .iter()
            .copied()
            .filter(|&p| {
                p != h && dom.as_ref().is_some_and(|d| d.dominates(h, p))
            })
            .collect();
        if latches.is_empty() {
            continue;
        }
        let mut lset: FxHashSet<Ptr<BasicBlock>> = [h].into_iter().collect();
        let mut wl = latches.clone();
        while let Some(b) = wl.pop() {
            if !lset.insert(b) || lset.len() > MAX_LOOP_BLOCKS {
                continue;
            }
            wl.extend(
                preds
                    .get(&b)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|p| *p != h),
            );
        }
        if lset.len() > MAX_LOOP_BLOCKS
            || lset.contains(&e)
            || !lset.contains(&body0)
            || lset.iter().any(|b| claimed.contains(b))
        {
            continue;
        }
        // Unique outside pred = preheader, ending in `br h`.
        let outside: Vec<Ptr<BasicBlock>> = hpreds
            .iter()
            .copied()
            .filter(|p| !lset.contains(p))
            .collect();
        let [ph] = outside.as_slice() else {
            continue;
        };
        let Some(&pht) = bops[&ph].last() else {
            continue;
        };
        if !Operation::is_op::<BrOp>(pht, ctx) {
            continue;
        }
        // Interior: unconditional brs only, exits only through h.
        let ok = lset.iter().all(|&b| {
            b == h || bops[&b].last().is_some_and(|&t| Operation::is_op::<BrOp>(t, ctx))
        }) && lset.iter().all(|&b| {
            bops[&b]
                .last()
                .into_iter()
                .all(|&t| {
                    t.deref(ctx)
                        .successors()
                        .all(|s| lset.contains(&s) || (b == h && s == e))
                })
        });
        if !ok {
            continue;
        }
        // Op gate + raw collections. Stores are classified once
        // `stored_bases` and the eligible-call set are known; every other
        // op must have no memory effects.
        let mut ok = true;
        let mut step: Option<u128> = None;
        let mut calls: FxHashSet<Ptr<Operation>> = FxHashSet::default();
        let mut stores: Vec<(Ptr<Operation>, Ptr<BasicBlock>, Value, Value)> = Vec::new();
        let mut loads: Vec<Ptr<Operation>> = Vec::new();
        'scan: for &b in &lset {
            let nb = bops[&b].len();
            for (i, &op) in bops[&b].iter().enumerate() {
                let o = op.deref(ctx);
                if Operation::is_op::<BrOp>(op, ctx) || (b == h && i + 1 == nb) {
                    continue;
                }
                if Operation::is_op::<StoreOp>(op, ctx) {
                    let (w, p) = (o.get_operand(0), o.get_operand(1));
                    if p == c_ptr {
                        // Counter increment: `add (load c_ptr), K`.
                        if step.is_some() {
                            ok = false;
                            break 'scan;
                        }
                        let Some(ad) = w.defining_op() else {
                            ok = false;
                            break 'scan;
                        };
                        if !Operation::is_op::<AddOp>(ad, ctx) {
                            ok = false;
                            break 'scan;
                        }
                        let (a0, a1) =
                            (ad.deref(ctx).get_operand(0), ad.deref(ctx).get_operand(1));
                        let kv = if is_load_of(ctx, st, a0, c_ptr) {
                            a1
                        } else if is_load_of(ctx, st, a1, c_ptr) {
                            a0
                        } else {
                            ok = false;
                            break 'scan;
                        };
                        match crate::sroa::const_int(ctx, st, kv).filter(|k| *k > 0) {
                            Some(k) => step = Some(k as u128),
                            None => {
                                ok = false;
                                break 'scan;
                            }
                        }
                        continue;
                    }
                    stores.push((op, b, w, p));
                    continue;
                }
                if Operation::is_op::<LoadOp>(op, ctx) {
                    loads.push(op);
                    continue;
                }
                if Operation::is_op::<CallOp>(op, ctx) {
                    // Iterations 1..n drop this call: sound when the
                    // callee can't write on a returning path (nowrite
                    // member) and can't diverge there (willreturn). The
                    // iteration-0 copy still runs, so unwind and any
                    // non-returning write happen exactly as they would
                    // have.
                    if !st.volatile.contains(&op)
                        && crate::inline::direct_callee(ctx, st, op).is_some_and(|c| {
                            st.nowrite.contains_key(c) && st.willreturn.contains(c)
                        })
                    {
                        calls.insert(op);
                        continue;
                    }
                    ok = false;
                    break 'scan;
                }
                if crate::callgvn::writes_plain(ctx, st, op) {
                    ok = false;
                    break 'scan;
                }
            }
        }
        if !ok || step.is_none() {
            continue;
        }
        // Nothing defined inside may be used outside: once the latch is
        // rewired to the exit an in-loop def is only a valid fixup input.
        for &b in &lset {
            for &op in &bops[&b] {
                if !ok {
                    break;
                }
                let o = op.deref(ctx);
                for r in 0..o.get_num_results() {
                    if !o.get_result(r).uses(ctx).iter().all(|u| {
                        parent(ctx, u.user_op()).is_some_and(|pb| lset.contains(&pb))
                    }) {
                        ok = false;
                    }
                }
            }
        }
        if !ok {
            continue;
        }
        // Every stored pointer must resolve to a bare alloca base; a
        // store anywhere else writes memory the fold cannot model.
        let mut sbases: FxHashSet<Value> = FxHashSet::default();
        for &(_, _, _, p) in &stores {
            let Some(a) = slot_base(ctx, p) else {
                ok = false;
                break;
            };
            if a == c_slot {
                ok = false;
                break;
            }
            sbases.insert(a);
        }
        // The counter slot is stored too — `load c` is iteration-variant.
        if ok {
            sbases.insert(c_slot);
        }
        if !ok {
            continue;
        }
        // Group non-dead stores by their exact pointer. A slot whose
        // base's pointer cone never leaves the loop is dead — iteration
        // 0's write is already unobservable, whatever it stored.
        let mut memo: FxHashMap<Value, u8> = FxHashMap::default();
        let mut fuel = MAX_ACC_WALK;
        let mut seen_dead: FxHashSet<Value> = FxHashSet::default();
        // Two distinct SSA pointers resolving to the same slot would each
        // classify independently while really aliasing — refuse that.
        let mut pbase: FxHashMap<Value, Value> = FxHashMap::default();
        let mut byp: FxHashMap<Value, Vec<(Ptr<Operation>, Ptr<BasicBlock>, Value)>> =
            FxHashMap::default();
        for &(sop, sb, w, p) in &stores {
            let a = slot_base(ctx, p).unwrap();
            if seen_dead.contains(&a) || loop_private(ctx, st, &lset, a) {
                seen_dead.insert(a);
                continue;
            }
            // The store address must be the same every trip — a
            // `a[i] = f(i)` scatter can't be collapsed to one store.
            if !arg_inv(ctx, st, &lset, &sbases, &mut memo, &mut fuel, p) {
                ok = false;
                break;
            }
            match pbase.insert(a, p) {
                Some(q) if q != p => {
                    ok = false;
                    break;
                }
                _ => {}
            }
            byp.entry(p).or_default().push((sop, sb, w));
        }
        // Per pointer, every store must be the same kind: xor-recurrence
        // `store p <- xor(load p, inv)` (accumulator — final value needs a
        // fixup on even trips) or `arg_inv` (self-consistent — iteration
        // 0 already wrote the final value). Mixing kinds would make the
        // final value order-dependent.
        // Per acc pointer: (store op, block, stored value, invariant).
        let mut accs: FxHashMap<Value, (Vec<(Ptr<Operation>, Ptr<BasicBlock>, Value, Value)>, TypeHandle)> =
            FxHashMap::default();
        let mut visible: Vec<(Ptr<BasicBlock>, Value)> = Vec::new();
        for (p, ws) in byp {
            let mut mask = 0u8;
            let mut ast = Vec::new();
            let ty = ws[0].2.get_type(ctx);
            for (sop, sb, w) in ws {
                if w.get_type(ctx) != ty {
                    ok = false;
                    break;
                }
                let mut iv = None;
                if let Some(xo) =
                    w.defining_op().filter(|&d| Operation::is_op::<XorOp>(d, ctx))
                {
                    let (x0, x1) =
                        (xo.deref(ctx).get_operand(0), xo.deref(ctx).get_operand(1));
                    iv = if is_load_of(ctx, st, x0, p)
                        && arg_inv(ctx, st, &lset, &sbases, &mut memo, &mut fuel, x1)
                    {
                        Some(x1)
                    } else if is_load_of(ctx, st, x1, p)
                        && arg_inv(ctx, st, &lset, &sbases, &mut memo, &mut fuel, x0)
                    {
                        Some(x0)
                    } else {
                        None
                    };
                }
                if let Some(v) = iv {
                    mask |= 1;
                    ast.push((sop, sb, w, v));
                } else if arg_inv(ctx, st, &lset, &sbases, &mut memo, &mut fuel, w) {
                    mask |= 2;
                    visible.push((sb, p));
                } else {
                    mask |= 4;
                }
            }
            if !ok {
                break;
            }
            match mask {
                1 => {
                    accs.insert(p, (ast, ty));
                }
                2 => {}
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || latches.len() != 1 {
            continue;
        }
        let latch = latches[0];
        // The executed iteration must actually run every live store and
        // produce every fixup input: their defs have to dominate the
        // latch. Outside-loop defs reach it through the preheader.
        let dom_ok = visible.iter().all(|&(sb, _)| {
            dom.as_ref().is_some_and(|d| d.dominates(sb, latch))
        }) && accs.values().all(|(ast, _)| {
            ast.iter().all(|&(_, _, _, iv)| {
                def_block(ctx, iv)
                    .is_none_or(|db| dom.as_ref().is_some_and(|d| d.dominates(db, latch)))
            })
        });
        if !dom_ok {
            continue;
        }
        // Counter discipline across the whole function: exactly one
        // store outside the loop (the constant init) and no loads of its
        // slot at all outside it.
        let mut init = None;
        for &b in &bs {
            for &op in &bops[&b] {
                if lset.contains(&b) {
                    continue;
                }
                let o = op.deref(ctx);
                if Operation::is_op::<LoadOp>(op, ctx)
                    && slot_base(ctx, o.get_operand(0)) == Some(c_slot)
                {
                    ok = false;
                    break;
                }
                if Operation::is_op::<StoreOp>(op, ctx)
                    && slot_base(ctx, o.get_operand(1)) == Some(c_slot)
                {
                    if init.is_some() {
                        ok = false;
                        break;
                    }
                    init = crate::sroa::const_int(ctx, st, o.get_operand(0));
                    if init.is_none() {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                break;
            }
        }
        let Some(init) = init.filter(|_| ok) else {
            continue;
        };
        // Trips until `c >= bound` (unsigned); bail if the counter could
        // wrap its type width.
        let w = match classify(ctx, x.get_type(ctx)) {
            TyK::Int(w) => w as u32,
            _ => continue,
        };
        let (init, bound, step) = (init as u128, bound as u128, step.unwrap());
        let n = bound.saturating_sub(init).div_ceil(step);
        if n == 0 || init.saturating_add(n.saturating_mul(step)) >= 1u128 << w.min(127) {
            continue;
        }
        let odd = n % 2 == 1;
        if crate::env_set("PLIRON_XIDBG") {
            eprintln!("xoridem   fold h={h:?} n={n} accs={}", accs.len());
        }
        claimed.extend(lset.iter().copied());
        // Iteration 0 runs as written; the latch's back-edge is rewired
        // to the exit so it is also the last. On odd trips `s0 ^ D` is
        // already the answer. On even trips `s0` is — and every in-loop
        // accumulator store is dead, so when nothing else reads the slot
        // they are erased outright: the xor chains die, freeing the call
        // results they consumed (callgvn then dead-eliminates the call
        // itself). When other reads exist the store stays honest and the
        // slot is restored from a preheader snapshot at the latch.
        let lt = *bops[&latch].last().unwrap();
        if !odd {
            for (&p, (ast, ty)) in &accs {
                let a = slot_base(ctx, p).unwrap();
                let roots: FxHashSet<Ptr<Operation>> = ast
                    .iter()
                    .filter_map(|&(_, _, w, _)| {
                        let xo = w.defining_op()?;
                        (0..xo.deref(ctx).get_num_operands()).find_map(|i| {
                            let v = xo.deref(ctx).get_operand(i);
                            is_load_of(ctx, st, v, p).then(|| v.defining_op().unwrap())
                        })
                    })
                    .collect();
                let clean = loads.iter().all(|&l| {
                    slot_base(ctx, l.deref(ctx).get_operand(0)) != Some(a)
                        || roots.contains(&l)
                });
                if clean {
                    for &(sop, _, w, _) in ast {
                        Operation::erase(sop, ctx);
                        peel_dead(ctx, st, w, MAX_ACC_WALK);
                    }
                } else {
                    let l = LoadOp::new(ctx, p, *ty).get_operation();
                    l.insert_before(ctx, pht);
                    let lv = l.deref(ctx).get_result(0);
                    let s = StoreOp::new(ctx, lv, p).get_operation();
                    s.insert_before(ctx, lt);
                }
            }
        }
        let hsuccs: Vec<usize> = lt
            .deref(ctx)
            .successors()
            .enumerate()
            .filter(|&(_, s)| s == h)
            .map(|(i, _)| i)
            .collect();
        for i in hsuccs {
            Operation::replace_successor(lt, ctx, i, e);
        }
        folds += 1;
    }
    folds
}

pub fn run(ctx: &mut Context, st: &mut State) {
    let mut n = 0usize;
    for s in bodied(ctx, st) {
        let fop = st.funcs[&s].op;
        let f = run_fn(ctx, st, fop);
        if f > 0 {
            n += f;
            let bs = blocks(ctx, fop);
            let om: FxHashMap<_, _> = bs.iter().map(|&b| (b, ops(ctx, b))).collect();
            erase_unreachable(ctx, st, &bs, &|b| om[&b].clone());
        }
    }
    if crate::env_set("PLIRON_STATS") || crate::env_set("PLIRON_XIDBG") {
        eprintln!("xoridem   {n} folds");
    }
}

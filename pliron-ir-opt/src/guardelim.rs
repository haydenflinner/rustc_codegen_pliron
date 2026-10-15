//! Implied-guard elimination — the panic-path half of LLVM's
//! CorrelatedValuePropagation. A `cond_br (icmp ult i, len) -> ok else
//! panic` whose condition is already decided by a dominating branch folds
//! to a plain `br`: the bounds check inside `for i in 0..min(a,b)` is
//! dominated by the loop guard `i < min`, and `min <= each bound` falls out
//! of the edge facts on the min-diamond's own branches. Only fires when the
//! abandoned successor is `unreachable`-terminated — the pass exists to
//! delete provably-dead panic edges so `nounwind`/`nowrite` can upgrade;
//! folded blocks (and any invoke edges leaving them) are erased by
//! `erase_unreachable`.
//!
//! Facts live on atoms, not raw values: two loads of the same frame-local
//! slot that see the same reaching stores carry the same value (on any
//! execution trace the last store before both is the same — a store
//! strictly between them would appear in only the later load's set), and a
//! slot loaded behind several reaching stores is <= any bound every stored
//! value provably satisfies under the facts at its own store's block —
//! that's the `x < umin(a,b) -> x < a` case, with no min recognizer: the
//! diamond's else-edge fact `a >= b` does the work.

use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::BranchOpInterface;
use pliron::context::{Context, Ptr};
use pliron::op::{Op, op_cast};
use pliron::operation::Operation;
use pliron::value::Value;
use pliron_llvm::attributes::ICmpPredicateAttr as P;
use pliron_llvm::ops::{
    AllocaOp, BrOp, CallIntrinsicOp, CondBrOp, GetElementPtrOp, ICmpOp, LoadOp, StoreOp,
    UnreachableOp,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::domcheck::Dom;
use crate::helpers::{bodied, erase_unreachable};
use crate::inline::{blocks, ops};
use crate::nowrite::PURE;
use crate::state::State;

/// Reaching-store set cap: a slot written in more places gets no alias
/// reasoning (atoms for its loads stay opaque).
const MAX_STORES: usize = 16;
/// DFS fuel for the implication engine.
const MAX_PROOF: usize = 64;
/// Dominator-chain depth for fact collection.
const MAX_DOM: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rel {
    Lt,
    Le,
    Eq,
    Ne,
}

/// A comparable value: an SSA value, or "whatever a load of `slot` reads
/// given exactly these reaching stores" (same slot + same set ⇒ same value).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Atom {
    V(Value),
    Slot(Value, Vec<Ptr<Operation>>),
    /// A block argument: (block, argument index). Its value is whichever
    /// operand a predecessor branch forwarded — multi-pred = multi-source.
    Arg(Ptr<BasicBlock>, usize),
}

/// Per-function scratch.
struct FnInfo {
    bops: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<Operation>>>,
    preds: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
    /// slot -> stores reaching the block head (None = overflow/unknown).
    inn: FxHashMap<Ptr<BasicBlock>, FxHashMap<Value, Option<Vec<Ptr<Operation>>>>>,
    escaped: FxHashSet<Value>,
}

/// The frame slot a pointer addresses (alloca through GEP chains), if any.
pub(crate) fn slot_base(ctx: &Context, mut p: Value) -> Option<Value> {
    loop {
        match p.defining_op() {
            Some(d) if Operation::is_op::<AllocaOp>(d, ctx) => return Some(p),
            Some(d) if Operation::is_op::<GetElementPtrOp>(d, ctx) => {
                p = d.deref(ctx).get_operand(0)
            }
            _ => return None,
        }
    }
}

/// Stores reaching `slot` at op position `i` inside `b` (kill semantics:
/// the last in-block store supersedes everything before it).
fn stores_of(
    ctx: &Context,
    info: &FnInfo,
    b: Ptr<BasicBlock>,
    i: usize,
    slot: Value,
) -> Option<Vec<Ptr<Operation>>> {
    let last = info.bops.get(&b)?.iter().take(i).rev().find(|&&op| {
        Operation::is_op::<StoreOp>(op, ctx)
            && slot_base(ctx, op.deref(ctx).get_operand(1)) == Some(slot)
    });
    if let Some(&s) = last {
        return Some(vec![s]);
    }
    info.inn.get(&b)?.get(&slot).cloned()?
}

/// Canonicalize `v`: non-volatile loads of an unescaped frame slot become
/// `Slot` atoms keyed by the reaching stores at the load's position; block
/// args become `Arg` atoms so predecessor-forwarded bounds participate.
fn atom(ctx: &Context, st: &State, info: &FnInfo, v: Value) -> Atom {
    if let Some(b) = v.defining_block() {
        return b
            .deref(ctx)
            .arguments()
            .position(|a| a == v)
            .map(|i| Atom::Arg(b, i))
            .unwrap_or(Atom::V(v));
    }
    let Some(d) = v.defining_op() else {
        return Atom::V(v);
    };
    if !Operation::is_op::<LoadOp>(d, ctx) || st.volatile.contains(&d) {
        return Atom::V(v);
    }
    let Some(slot) = slot_base(ctx, d.deref(ctx).get_operand(0)) else {
        return Atom::V(v);
    };
    if info.escaped.contains(&slot) {
        return Atom::V(v);
    }
    let Some(b) = d.deref(ctx).get_parent_block() else {
        return Atom::V(v);
    };
    let Some(i) = info
        .bops
        .get(&b)
        .and_then(|os| os.iter().position(|&o| o == d))
    else {
        return Atom::V(v);
    };
    let Some(mut set) = stores_of(ctx, info, b, i, slot) else {
        return Atom::V(v);
    };
    // Canonical order for atom equality; Ptr has no Ord — sort by its
    // stable-in-run debug index.
    set.sort_by_cached_key(|p| format!("{p:?}"));
    Atom::Slot(slot, set)
}

/// Facts true on entry to `b`: walk the idom chain; for each dominator `d`
/// ending in `cond_br` on an icmp, the successor that dominates `b`
/// contributes its (possibly negated) predicate. Unsigned preds + eq/ne.
fn facts(
    ctx: &Context,
    st: &State,
    info: &FnInfo,
    dom: &Dom,
    memo: &mut FxHashMap<Ptr<BasicBlock>, Vec<(Rel, Atom, Atom)>>,
    b: Ptr<BasicBlock>,
) -> Vec<(Rel, Atom, Atom)> {
    if let Some(f) = memo.get(&b) {
        return f.clone();
    }
    let mut out = Vec::new();
    let mut cur = b;
    let mut hops = MAX_DOM;
    while hops > 0 {
        hops -= 1;
        let Some(d) = dom.idom(cur) else { break };
        cur = d;
        let Some(&term) = info.bops.get(&d).and_then(|os| os.last()) else {
            continue;
        };
        if !Operation::is_op::<CondBrOp>(term, ctx) {
            continue;
        }
        let tdst = term.deref(ctx).get_successor(0);
        let fdst = term.deref(ctx).get_successor(1);
        let neg = if dom.dominates(tdst, b) {
            false
        } else if dom.dominates(fdst, b) {
            true
        } else {
            continue;
        };
        let cond = term.deref(ctx).get_operand(0);
        let Some(ci) = cond.defining_op() else {
            continue;
        };
        let Some(icmp) = Operation::get_op::<ICmpOp>(ci, ctx) else {
            continue;
        };
        let (x, y) = (ci.deref(ctx).get_operand(0), ci.deref(ctx).get_operand(1));
        let (rel, swap) = match (icmp.predicate(ctx), neg) {
            // Edge fact `x P y` (or its negation) normalized to `a R b`.
            (P::ULT, false) => (Rel::Lt, false),
            (P::ULT, true) => (Rel::Le, true), // !(x<y) => y<=x
            (P::ULE, false) => (Rel::Le, false),
            (P::ULE, true) => (Rel::Lt, true),
            (P::UGT, false) => (Rel::Lt, true),
            (P::UGT, true) => (Rel::Le, false),
            (P::UGE, false) => (Rel::Le, true),
            (P::UGE, true) => (Rel::Lt, false),
            (P::EQ, false) => (Rel::Eq, false),
            (P::EQ, true) => (Rel::Ne, false),
            (P::NE, false) => (Rel::Ne, false),
            (P::NE, true) => (Rel::Eq, false),
            _ => continue,
        };
        let (a, b) = if swap { (y, x) } else { (x, y) };
        out.push((rel, atom(ctx, st, info, a), atom(ctx, st, info, b)));
    }
    memo.insert(b, out.clone());
    out
}

/// Atom form of a store's stored value.
fn stored_atom(ctx: &Context, st: &State, info: &FnInfo, store: Ptr<Operation>) -> Atom {
    atom(ctx, st, info, store.deref(ctx).get_operand(0))
}

/// The values a predecessor branch forwards to an `Arg` atom:
/// (pred block, operand value). Empty for entry blocks.
fn arg_sources(
    ctx: &Context,
    info: &FnInfo,
    b: Ptr<BasicBlock>,
    arg: usize,
) -> Vec<(Ptr<BasicBlock>, Value)> {
    let na = b.deref(ctx).get_num_arguments();
    let mut out = Vec::new();
    for &p in info.preds.get(&b).into_iter().flatten() {
        let Some(&t) = info.bops.get(&p).and_then(|os| os.last()) else {
            continue;
        };
        let o = Operation::get_op_dyn(t, ctx);
        let Some(br) = op_cast::<dyn BranchOpInterface>(&*o) else {
            continue;
        };
        for (si, s) in t.deref(ctx).successors().enumerate() {
            if s != b {
                continue;
            }
            let v = br.successor_operands(ctx, si);
            if v.len() == na {
                out.push((p, v[arg]));
            }
        }
    }
    out
}

/// Sources of a multi-source atom: for a Slot its reaching stores, for an
/// Arg its pred-forwarded values — each paired with the block whose facts
/// bound the produced value (the store's block / the forwarding pred).
fn sources(
    ctx: &Context,
    st: &State,
    info: &FnInfo,
    a: &Atom,
) -> Vec<(Atom, Ptr<BasicBlock>)> {
    match a {
        Atom::Slot(_, set) => set
            .iter()
            .filter_map(|&s| {
                let w = stored_atom(ctx, st, info, s);
                s.deref(ctx)
                    .get_parent_block()
                    .map(|b| (w, b))
            })
            .collect(),
        Atom::Arg(b, i) => arg_sources(ctx, info, *b, *i)
            .into_iter()
            .map(|(p, v)| (atom(ctx, st, info, v), p))
            .collect(),
        Atom::V(_) => Vec::new(),
    }
}

struct Prover<'a> {
    ctx: &'a Context,
    st: &'a State,
    info: &'a FnInfo,
    dom: &'a Dom,
    memo: FxHashMap<Ptr<BasicBlock>, Vec<(Rel, Atom, Atom)>>,
    fuel: usize,
}

impl<'a> Prover<'a> {
    fn facts_of(&mut self, b: Ptr<BasicBlock>) -> Vec<(Rel, Atom, Atom)> {
        facts(self.ctx, self.st, self.info, self.dom, &mut self.memo, b)
    }

    /// Prove `a <= b` (strict=false) or `a < b` under facts `fs`.
    fn prove(&mut self, strict: bool, fs: &[(Rel, Atom, Atom)], a: &Atom, b: &Atom) -> bool {
        if self.fuel == 0 {
            return false;
        }
        if a == b {
            return !strict;
        }
        self.fuel -= 1;
        // Direct fact.
        for f in fs {
            if f.1 == *a && f.2 == *b {
                let hit = match (strict, f.0) {
                    (true, Rel::Lt) | (false, Rel::Le | Rel::Lt | Rel::Eq) => true,
                    _ => false,
                };
                if hit {
                    return true;
                }
            }
        }
        // One-hop chains: a R m, m R' b.
        for f in fs {
            if f.1 != *a {
                continue;
            }
            let need_strict = match f.0 {
                Rel::Lt => false,
                Rel::Le | Rel::Eq => strict,
                _ => continue,
            };
            let m = f.2.clone();
            if self.prove(need_strict, fs, &m, b) {
                return true;
            }
        }
        // Single-source equality (a sole reaching store / sole forwarded
        // operand makes the atom that value), else universal: whichever
        // source produced the value, the bound holds — checked under each
        // source's own block's facts.
        for side in 0..2 {
            let av = if side == 0 { a } else { b };
            let srcs = sources(self.ctx, self.st, self.info, av);
            if srcs.len() == 1 {
                let w = srcs[0].0.clone();
                let (x, y) = if side == 0 { (w, b.clone()) } else { (a.clone(), w) };
                if self.prove(strict, fs, &x, &y) {
                    return true;
                }
            } else if srcs.len() > 1 {
                return srcs.iter().all(|(w, sb)| {
                    let fs2 = self.facts_of(*sb);
                    let (x, y) = if side == 0 {
                        (w.clone(), b.clone())
                    } else {
                        (a.clone(), w.clone())
                    };
                    self.prove(strict, &fs2, &x, &y)
                });
            }
        }
        false
    }
}

/// The last op of `b` is `llvm.unreachable`: a panic/abort path.
fn unreachable_terminated(ctx: &Context, info: &FnInfo, b: Ptr<BasicBlock>) -> bool {
    info.bops
        .get(&b)
        .and_then(|os| os.last())
        .is_some_and(|&t| Operation::is_op::<UnreachableOp>(t, ctx))
}

fn run_fn(ctx: &mut Context, st: &mut State, fop: Ptr<Operation>) -> bool {
    let bs = blocks(ctx, fop);
    if bs.len() < 2 {
        return false;
    }
    let Some(dom) = Dom::new(ctx, st, fop) else {
        return false;
    };
    let bops: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<Operation>>> =
        bs.iter().map(|&b| (b, ops(ctx, b))).collect();
    // Slots whose address escapes (operand of a non-load/store/gep/pure op)
    // can't carry load-equality reasoning.
    let mut escaped = FxHashSet::default();
    for &b in &bs {
        for &op in &bops[&b] {
            let pure_intr = Operation::is_op::<CallIntrinsicOp>(op, ctx)
                && st
                    .intrinsics
                    .get(&op)
                    .is_some_and(|n| PURE.iter().any(|p| n.starts_with(p)));
            if Operation::is_op::<LoadOp>(op, ctx)
                || Operation::is_op::<StoreOp>(op, ctx)
                || Operation::is_op::<GetElementPtrOp>(op, ctx)
                || pure_intr
            {
                continue;
            }
            for i in 0..op.deref(ctx).get_num_operands() {
                if let Some(s) = slot_base(ctx, op.deref(ctx).get_operand(i)) {
                    escaped.insert(s);
                }
            }
        }
    }
    // Reaching stores per (block, slot): forward fixpoint over reachable
    // blocks only (unreachable preds would pollute the may-sets).
    let mut outt: FxHashMap<Ptr<BasicBlock>, FxHashMap<Value, Option<Vec<Ptr<Operation>>>>> =
        FxHashMap::default();
    let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
    let mut reach: Vec<Ptr<BasicBlock>> = Vec::new();
    for &b in &bs {
        if !dom.idx.contains_key(&b) {
            continue;
        }
        reach.push(b);
        for op in &bops[&b] {
            for s in op.deref(ctx).successors() {
                if dom.idx.contains_key(&s) {
                    preds.entry(s).or_default().push(b);
                }
            }
        }
    }
    let mut wl: Vec<Ptr<BasicBlock>> = reach.clone();
    let mut inq: FxHashSet<Ptr<BasicBlock>> = wl.iter().copied().collect();
    while let Some(b) = wl.pop() {
        inq.remove(&b);
        let mut m: FxHashMap<Value, Option<Vec<Ptr<Operation>>>> = FxHashMap::default();
        for &p in preds.get(&b).into_iter().flatten() {
            let Some(pm) = outt.get(&p) else { continue };
            for (&s, set) in pm {
                let e = m.entry(s).or_insert_with(|| Some(Vec::new()));
                match (e.as_mut(), set) {
                    (Some(v), Some(ps)) => {
                        for &op in ps {
                            if !v.contains(&op) {
                                v.push(op);
                            }
                        }
                        if v.len() > MAX_STORES {
                            *e = None;
                        }
                    }
                    _ => *e = None,
                }
            }
        }
        // out = in, killed per-slot by the block's own (last) stores.
        for &op in &bops[&b] {
            if Operation::is_op::<StoreOp>(op, ctx)
                && let Some(s) = slot_base(ctx, op.deref(ctx).get_operand(1))
            {
                m.insert(s, Some(vec![op]));
            }
        }
        if outt.get(&b) != Some(&m) {
            outt.insert(b, m);
            for op in &bops[&b] {
                for s in op.deref(ctx).successors() {
                    if dom.idx.contains_key(&s) && inq.insert(s) {
                        wl.push(s);
                    }
                }
            }
        }
    }
    // inn[b][slot] = union of preds' out — the reaching set at block head.
    let mut inn: FxHashMap<Ptr<BasicBlock>, FxHashMap<Value, Option<Vec<Ptr<Operation>>>>> =
        FxHashMap::default();
    for &b in &reach {
        let mut m: FxHashMap<Value, Option<Vec<Ptr<Operation>>>> = FxHashMap::default();
        for &p in preds.get(&b).into_iter().flatten() {
            let Some(pm) = outt.get(&p) else { continue };
            for (&s, set) in pm {
                let e = m.entry(s).or_insert_with(|| Some(Vec::new()));
                match (e.as_mut(), set) {
                    (Some(v), Some(ps)) => {
                        for &op in ps {
                            if !v.contains(&op) {
                                v.push(op);
                            }
                        }
                        if v.len() > MAX_STORES {
                            *e = None;
                        }
                    }
                    _ => *e = None,
                }
            }
        }
        inn.insert(b, m);
    }
    let info = FnInfo {
        bops,
        preds,
        inn,
        escaped,
    };

    let mut folds: Vec<(Ptr<Operation>, Ptr<BasicBlock>, Vec<Value>)> = Vec::new();
    {
        let mut pr = Prover {
            ctx,
            st,
            info: &info,
            dom: &dom,
            memo: FxHashMap::default(),
            fuel: MAX_PROOF,
        };
        for &b in &bs {
        let Some(&term) = info.bops[&b].last() else {
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
        let (x, y) = (ci.deref(ctx).get_operand(0), ci.deref(ctx).get_operand(1));
        let (ax, ay) = (atom(ctx, st, &info, x), atom(ctx, st, &info, y));
        pr.fuel = MAX_PROOF;
        let fs = pr.facts_of(b);
        if crate::env_set("PLIRON_GEDBG") {
            eprintln!(
                "guardelim   br {b:?} pred={:?} x={ax:?} y={ay:?} facts={fs:?}",
                icmp.predicate(ctx)
            );
            for atom_v in [&ax, &ay] {
                for (w, sb) in sources(ctx, st, &info, atom_v) {
                    eprintln!("guardelim       src {w:?} @{sb:?}");
                }
            }
        }
        // cond provably true / false? (unsigned + eq/ne; signed skipped)
        let (t, f) = match icmp.predicate(ctx) {
            P::ULT => (
                pr.prove(true, &fs, &ax, &ay),
                pr.prove(false, &fs, &ay, &ax),
            ),
            P::ULE => (
                pr.prove(false, &fs, &ax, &ay),
                pr.prove(true, &fs, &ay, &ax),
            ),
            P::UGT => (
                pr.prove(true, &fs, &ay, &ax),
                pr.prove(false, &fs, &ax, &ay),
            ),
            P::UGE => (
                pr.prove(false, &fs, &ay, &ax),
                pr.prove(true, &fs, &ax, &ay),
            ),
            P::EQ => (
                ax == ay || fs.iter().any(|f| {
                    f.0 == Rel::Eq && ((f.1 == ax && f.2 == ay) || (f.1 == ay && f.2 == ax))
                }),
                fs.iter().any(|f| {
                    f.0 == Rel::Ne && ((f.1 == ax && f.2 == ay) || (f.1 == ay && f.2 == ax))
                }) || pr.prove(true, &fs, &ax, &ay)
                    || pr.prove(true, &fs, &ay, &ax),
            ),
            P::NE => (
                fs.iter().any(|f| {
                    f.0 == Rel::Ne && ((f.1 == ax && f.2 == ay) || (f.1 == ay && f.2 == ax))
                }) || pr.prove(true, &fs, &ax, &ay)
                    || pr.prove(true, &fs, &ay, &ax),
                ax == ay || fs.iter().any(|f| {
                    f.0 == Rel::Eq && ((f.1 == ax && f.2 == ay) || (f.1 == ay && f.2 == ax))
                }),
            ),
            _ => (false, false),
        };
            let cb = Operation::get_op::<CondBrOp>(term, ctx).unwrap();
            let (tdst, fdst) = (
                term.deref(ctx).get_successor(0),
                term.deref(ctx).get_successor(1),
            );
            let (dst, ods) = if t && unreachable_terminated(ctx, &info, fdst) {
                (tdst, cb.get_true_dest_operands(ctx))
            } else if f && unreachable_terminated(ctx, &info, tdst) {
                (fdst, cb.get_false_dest_operands(ctx))
            } else {
                continue;
            };
            folds.push((term, dst, ods));
        }
    }
    if crate::env_set("PLIRON_GEDBG") && !folds.is_empty() {
        eprintln!("guardelim   {} folds", folds.len());
    }
    for (term, dst, ods) in &folds {
        let br = BrOp::new(ctx, *dst, ods.clone()).get_operation();
        br.insert_before(ctx, *term);
        Operation::erase(*term, ctx);
    }
    !folds.is_empty()
}

pub fn run(ctx: &mut Context, st: &mut State) {
    let mut n = 0usize;
    for s in bodied(ctx, st) {
        let fop = st.funcs[&s].op;
        if run_fn(ctx, st, fop) {
            n += 1;
            let bs = blocks(ctx, fop);
            let om: FxHashMap<_, _> = bs.iter().map(|&b| (b, ops(ctx, b))).collect();
            erase_unreachable(ctx, st, &bs, &|b| om[&b].clone());
        }
    }
    if crate::env_set("PLIRON_STATS") {
        eprintln!("guardelim {}: {} fns folded", st.cgu, n);
    }
}

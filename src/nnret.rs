//! Bottom-up nonnull-return inference (LLVM FunctionAttrs' `nonnull` on the
//! return value): a pointer-returning body whose every `ret` produces a value
//! provably non-zero — a `!nonnull` load, a `nonnull` param, an alloca or a
//! defined symbol's address, an inbounds offset of one of those, a select of
//! two, a block param whose every incoming arg qualifies, or a call to an
//! nnret callee — hands the caller a non-null pointer. `lower_call` seeds
//! such call results into `FnLower::nonnull`, where jumpthread's lattice
//! folds the `Option<ptr>` niche null tests.
//!
//! `st.nnargs`/`st.nonnull`/`st.inbounds` are populated before inlining, so
//! the sources they name are read here; recursion is handled by the same
//! optimistic-start, shrink-to-fixpoint loop as nounwind.rs. Foreign and
//! preemptible functions are never inferred.

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::BranchOpInterface;
use pliron::context::{Context, Ptr};
use pliron::op::op_cast;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::attributes::GepNoWrapFlags;
use pliron_llvm::ops::{
    AllocaOp, BitcastOp, CallOp, FreezeOp, GetElementPtrOp, ReturnOp, SelectOp,
};
use pliron_llvm::types::PointerType;
use rustc_hash::FxHashSet;

use crate::context::{ConstVal, State};
use crate::inline::{blocks, direct_callee, ops};
use crate::lower::has_body;
use crate::types::{TyK, classify};

/// Def-chain walk depth for one nonnull query.
const MAX_DEPTH: u32 = 8;

/// `sym` resolves to an object/symbol defined in this CGU: its address can't
/// be null. Declared-only and weakly-undefined symbols can resolve to 0.
fn defined(ctx: &Context, st: &State<'_>, sym: &str) -> bool {
    st.funcs.get(sym).is_some_and(|f| has_body(ctx, f.op))
        || st.globals.get(sym).is_some_and(|g| g.init.is_some())
}

/// Whether `v` is provably non-null under nnret table `t`; `known` holds the
/// function's `nonnull` pointer params plus block params that have (so far)
/// survived the all-incoming-args fixpoint.
fn nn(
    ctx: &Context,
    st: &State<'_>,
    known: &FxHashSet<Value>,
    v: Value,
    t: &FxHashSet<String>,
    depth: u32,
) -> bool {
    if known.contains(&v) {
        return true;
    }
    match st.consts.get(&v) {
        Some(ConstVal::Sym { sym, .. }) => return defined(ctx, st, sym),
        Some(ConstVal::Bits(b)) => return *b != 0,
        Some(_) => return false,
        None => {}
    }
    let Some(depth) = depth.checked_sub(1) else {
        return false;
    };
    let Some(op) = v.defining_op() else {
        return false;
    };
    if st.nonnull.contains(&op) {
        return true;
    }
    macro_rules! is {
        ($t:ty) => {
            Operation::is_op::<$t>(op, ctx)
        };
    }
    if is!(AllocaOp) {
        true
    } else if is!(BitcastOp) || is!(FreezeOp) {
        nn(ctx, st, known, op.deref(ctx).get_operand(0), t, depth)
    } else if is!(GetElementPtrOp) {
        let inbounds = st.inbounds.contains(&op)
            || Operation::get_op::<GetElementPtrOp>(op, ctx)
                .unwrap()
                .no_wrap_flags(ctx)
                .contains(GepNoWrapFlags::INBOUNDS);
        inbounds && nn(ctx, st, known, op.deref(ctx).get_operand(0), t, depth)
    } else if is!(SelectOp) {
        let d = op.deref(ctx);
        nn(ctx, st, known, d.get_operand(1), t, depth)
            && nn(ctx, st, known, d.get_operand(2), t, depth)
    } else if is!(CallOp) {
        direct_callee(ctx, st, op).is_some_and(|c| t.contains(c))
    } else {
        false
    }
}

/// Every `ret` in `f` returns a provably non-null value under table `t`.
/// A body with no `ret`s (diverging) qualifies vacuously: a call to it never
/// sees a result.
fn returns_nn(ctx: &Context, st: &State<'_>, f: Ptr<Operation>, t: &FxHashSet<String>) -> bool {
    let bs = blocks(ctx, f);
    let ptr = |v: Value| v.get_type(ctx).deref(ctx).is::<PointerType>();
    // Optimistic `known`: `nonnull` entry params plus every non-entry pointer
    // block param — except a landing pad's, whose payload comes from the
    // callee and is never provable. Params with no recorded incoming arg
    // stay optimistically marked, which is only possible in unreachable
    // blocks (their `ret`s can't run).
    let pads: FxHashSet<Ptr<BasicBlock>> = st.invokes.values().map(|&(l, _)| l).collect();
    let mut known: FxHashSet<Value> = bs[0]
        .deref(ctx)
        .arguments()
        .filter(|a| ptr(*a) && st.nnargs.get(a).is_some_and(|m| m & 1 != 0))
        .collect();
    for &b in &bs[1..] {
        if !pads.contains(&b) {
            known.extend(b.deref(ctx).arguments().filter(|&a| ptr(a)));
        }
    }
    // (param, incoming arg) for every edge, then shrink `known` until every
    // surviving param is justified on every edge (greatest fixpoint, as in
    // jumpthread's NonNull).
    let mut inc: Vec<(Value, Option<Value>)> = Vec::new();
    for &b in &bs {
        for op in ops(ctx, b) {
            let d = op.deref(ctx);
            if d.successors().next().is_none() {
                continue;
            }
            let o = Operation::get_op_dyn(op, ctx);
            let Some(br) = op_cast::<dyn BranchOpInterface>(&*o) else {
                continue;
            };
            for (si, s) in d.successors().enumerate() {
                let args = br.successor_operands(ctx, si);
                for (k, p) in s.deref(ctx).arguments().enumerate() {
                    inc.push((p, args.get(k).copied()));
                }
            }
        }
    }
    loop {
        let mut changed = false;
        for &(p, a) in &inc {
            if known.contains(&p)
                && !a.is_some_and(|a| known.contains(&a) || nn(ctx, st, &known, a, t, MAX_DEPTH))
            {
                known.remove(&p);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    bs.into_iter().all(|b| {
        ops(ctx, b).into_iter().all(|op| {
            !Operation::is_op::<ReturnOp>(op, ctx)
                || Operation::get_op::<ReturnOp>(op, ctx)
                    .unwrap()
                    .retval(ctx)
                    .is_some_and(|v| nn(ctx, st, &known, v, t, MAX_DEPTH))
        })
    })
}

pub fn run(ctx: &Context, st: &mut State<'_>) {
    let cand: Vec<String> = st
        .funcs
        .iter()
        .filter(|(_, f)| {
            f.linkage != Linkage::Preemptible
                && has_body(ctx, f.op)
                && matches!(classify(ctx, f.ty), TyK::Func(r, ..) if matches!(classify(ctx, r), TyK::Ptr))
        })
        .map(|(s, _)| s.clone())
        .collect();
    // Optimistic start, then drop functions until a fixpoint: handles
    // recursion (a vacuous SCC of `return other(...)`s can only diverge, so
    // keeping it is sound).
    let mut t: FxHashSet<String> = cand.iter().cloned().collect();
    loop {
        let mut changed = false;
        for s in &cand {
            if t.contains(s) && !returns_nn(ctx, st, st.funcs[s].op, &t) {
                t.remove(s);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!(
            "nnret {}: {} of {} pointer-returning bodies nonnull",
            st.cgu,
            t.len(),
            cand.len()
        );
    }
    st.nnret = t;
}

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
//! the sources they name are read here. The rules derive the complement,
//! "may be null", as a least fixpoint, which handles recursion and loops
//! through block params. Foreign and preemptible functions are never
//! inferred.

use pliron::builtin::op_interfaces::BranchOpInterface;
use pliron::context::Context;
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

use crate::inline::{blocks, direct_callee};
use crate::state::{ConstVal, State};
use crate::types::{TyK, classify};
use crate::{Syms, bodied, has_body};

/// `sym` resolves to an object/symbol defined in this CGU: its address can't
/// be null. Declared-only and weakly-undefined symbols can resolve to 0.
fn defined(ctx: &Context, st: &State, sym: &str) -> bool {
    st.funcs.get(sym).is_some_and(|f| has_body(ctx, f.op))
        || st.globals.get(sym).is_some_and(|g| g.init.is_some())
}

/// A constant that can't be null: a defined symbol's address or non-zero
/// bits.
fn const_nonnull(ctx: &Context, st: &State, v: Value) -> bool {
    match st.consts.get(v) {
        Some(ConstVal::Sym { sym, .. }) => defined(ctx, st, sym),
        Some(ConstVal::Bits(b)) => *b != 0,
        _ => false,
    }
}

ascent::ascent! {
    struct MayNull;
    /// `v` may be null outright.
    relation src(Value);
    /// `d` may be null when `s` may be (bitcast, freeze, inbounds GEP,
    /// either select arm, a branch arg flowing into its block param).
    relation copy(Value, Value);
    /// `d` is the result of a direct call to `c`.
    relation call(u32, Value);
    /// `f` returns `v`.
    relation ret(u32, Value);
    /// `f` has a `ret` with no value.
    relation ret_void(u32);
    /// Pointer-returning bodies the table can hold.
    relation cand(u32);

    relation null(Value);
    null(v) <-- src(v);
    null(d) <-- copy(s, d), null(s);
    null(d) <-- call(c, d), !cand(c);
    null(d) <-- call(c, d), ret_null(c);

    relation ret_null(u32);
    ret_null(f) <-- ret(f, v), null(v);
    ret_null(f) <-- ret_void(f);
}

pub fn run(ctx: &Context, st: &mut State) {
    let cand: Vec<String> = bodied(ctx, st)
        .into_iter()
        .filter(|s| {
            matches!(classify(ctx, st.funcs[s].ty), TyK::Func(r, ..) if matches!(classify(ctx, r), TyK::Ptr))
        })
        .collect();
    let pads: FxHashSet<_> = st.invokes.values().map(|&(l, _)| l).collect();
    let ptr = |v: Value| v.get_type(ctx).deref(ctx).is::<PointerType>();
    let mut ids = Syms::default();
    let mut p = MayNull::default();
    // Every value the bodies define.
    let mut local: FxHashSet<Value> = FxHashSet::default();
    let mut scratch = Vec::new();
    for s in &cand {
        let f = ids.id(s);
        p.cand.push((f,));
        let bs = blocks(ctx, st.funcs[s].op);
        for &b in &bs {
            local.extend(b.deref(ctx).arguments());
            crate::inline::ops_into(ctx, b, &mut scratch);
            for &op in &scratch {
                local.extend(op.deref(ctx).results());
            }
        }
        // Entry params: only `nonnull` pointers. A landing pad's payload
        // comes from the callee and is never provable. Other block params
        // are non-null unless an incoming edge says otherwise (a param with
        // no incoming edge is in an unreachable block, whose `ret`s can't
        // run).
        for a in bs[0].deref(ctx).arguments() {
            if !(ptr(a) && st.nnargs.get(a).is_some_and(|m| m & 1 != 0)) {
                p.src.push((a,));
            }
        }
        for &b in &bs[1..] {
            let pad = pads.contains(&b);
            p.src.extend(
                b.deref(ctx)
                    .arguments()
                    .filter(|&a| pad || !ptr(a))
                    .map(|a| (a,)),
            );
        }
        for &b in &bs {
            crate::inline::ops_into(ctx, b, &mut scratch);
            for &op in &scratch {
                let d = op.deref(ctx);
                if d.successors().next().is_some()
                    && let Some(br) =
                        op_cast::<dyn BranchOpInterface>(&*Operation::get_op_dyn(op, ctx))
                {
                    for (si, s) in d.successors().enumerate() {
                        let args = br.successor_operands(ctx, si);
                        for (k, prm) in s.deref(ctx).arguments().enumerate() {
                            match args.get(k) {
                                Some(&a) => p.copy.push((a, prm)),
                                None => p.src.push((prm,)),
                            }
                        }
                    }
                }
                if let Some(r) = Operation::get_op::<ReturnOp>(op, ctx) {
                    match r.retval(ctx) {
                        Some(v) => p.ret.push((f, v)),
                        None => p.ret_void.push((f,)),
                    }
                    continue;
                }
                let mut rs = d.results().peekable();
                if rs.peek().is_none() {
                    continue;
                }
                macro_rules! is {
                    ($t:ty) => {
                        Operation::is_op::<$t>(op, ctx)
                    };
                }
                for r in rs {
                    if st.consts.contains(r) {
                        if !const_nonnull(ctx, st, r) {
                            p.src.push((r,));
                        }
                    } else if st.nonnull.contains(op) || is!(AllocaOp) {
                    } else if is!(BitcastOp) || is!(FreezeOp) {
                        p.copy.push((d.get_operand(0), r));
                    } else if is!(GetElementPtrOp)
                        && (st.inbounds.contains(op)
                            || Operation::get_op::<GetElementPtrOp>(op, ctx)
                                .unwrap()
                                .no_wrap_flags(ctx)
                                .contains(GepNoWrapFlags::INBOUNDS))
                    {
                        p.copy.push((d.get_operand(0), r));
                    } else if is!(SelectOp) {
                        p.copy.push((d.get_operand(1), r));
                        p.copy.push((d.get_operand(2), r));
                    } else if is!(CallOp)
                        && let Some(c) = direct_callee(ctx, st, op)
                    {
                        p.call.push((ids.id(c), r));
                    } else {
                        p.src.push((r,));
                    }
                }
            }
        }
    }
    // Values the bodies use but don't define (module-level constants).
    let used: Vec<Value> = p
        .copy
        .iter()
        .map(|&(s, _)| s)
        .chain(p.ret.iter().map(|&(_, v)| v))
        .filter(|v| !local.contains(v))
        .collect();
    for v in used {
        if !const_nonnull(ctx, st, v) {
            p.src.push((v,));
        }
    }
    p.run();
    let null: FxHashSet<u32> = p.ret_null.iter().map(|&(f,)| f).collect();
    // A body with no `ret`s (diverging) qualifies vacuously: a call to it
    // never sees a result.
    let t: FxHashSet<String> = cand
        .iter()
        .filter(|s| !null.contains(&ids.id(s)))
        .cloned()
        .collect();
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats",
                "nnret {}: {} of {} pointer-returning bodies nonnull",
                st.cgu,
                t.len(),
                cand.len()
            )
        };
    }
    for s in &cand {
        crate::dump_fact("nnret", s, &t.contains(s));
    }
    st.nnret = t.into_iter().collect();
}

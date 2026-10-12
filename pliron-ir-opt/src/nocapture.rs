//! Bottom-up pointer-capture inference (LLVM FunctionAttrs' `captures(none)`):
//! a param is captured when a value derived from it is stored into memory,
//! returned, or passed to a callee that might retain it. Uses of the pointer
//! — loads, stores *through* it, GEPs, compares, math — are not captures.
//!
//! Derivation is tracked in two bands per value: `exact` for pointer-typed
//! results (the value may literally be a derived pointer) and `fuzzy` for
//! integer results, which can still encode one (`ptrtoint`); comparisons and
//! other predicates produce neither, so `p == q` doesn't capture `p`. A
//! store of a derived value anywhere — even into the callee's own frame —
//! counts as a capture, so no intra-frame alias tracking is needed.
//!
//! rustc's `DeducedParamAttrs` covers only `PassMode::Indirect` params — its
//! MIR analysis ignores `*_k` uses and `move`/`copy` of a param, so it is
//! garbage for `&`/`&mut` params. This pass analyzes the pliron body instead,
//! where the pointer is a value like any other, and covers every param of
//! every bodied function in the CGU.

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{BranchOpInterface, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::op::op_cast;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::ops::{
    AtomicCmpxchgOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp, FCmpOp, ICmpOp,
    InlineAsmOp, ReturnOp, StoreOp,
};
use pliron_llvm::types::PointerType;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::has_body;
use crate::inline::{blocks, direct_callee, ops};
use crate::nowrite::PURE;
use crate::state::State;

const MAX_PARAMS: usize = 64;

/// Per-value derivation: (exact pointer bits, int-encoded bits).
type Masks = FxHashMap<Value, (u64, u64)>;

fn get(m: &Masks, v: Value) -> (u64, u64) {
    m.get(&v).copied().unwrap_or((0, 0))
}

/// All param bits (either band) that `v` may carry.
fn mask(m: &Masks, v: Value) -> u64 {
    let (e, f) = get(m, v);
    e | f
}

/// Both bands unioned over every operand of `op`.
fn all_ops(ctx: &Context, m: &Masks, op: Ptr<Operation>) -> u64 {
    op.deref(ctx).operands().fold(0, |a, v| a | mask(m, v))
}

/// `f`'s blocks plus per-value param-derivation masks, propagated to a
/// fixpoint: results inherit their operands' bands (exact → the value is a
/// pointer; fuzzy → an int that may encode one; comparisons produce
/// neither), branch operands flow into successor block params, and a call's
/// landing-pad exception payload conservatively inherits its args. `None`
/// when `f` has no trackable params.
fn deriv(ctx: &Context, st: &State, f: Ptr<Operation>) -> Option<(Vec<Ptr<BasicBlock>>, Masks)> {
    let bs = blocks(ctx, f);
    let mut m: Masks = FxHashMap::default();
    let mut seeds = 0;
    for (i, a) in bs[0].deref(ctx).arguments().enumerate() {
        if i < MAX_PARAMS {
            let ptr = a.get_type(ctx).deref(ctx).is::<PointerType>();
            m.insert(a, if ptr { (1 << i, 0) } else { (0, 1 << i) });
            seeds += 1;
        }
    }
    if seeds == 0 {
        return None;
    }
    let mut succs: FxHashMap<Ptr<Operation>, Vec<(Ptr<BasicBlock>, Vec<Value>)>> =
        FxHashMap::default();
    loop {
        let mut changed = false;
        for &b in &bs {
            for op in ops(ctx, b) {
                let (ex, fz) = op.deref(ctx).operands().fold((0, 0), |(ae, af), v| {
                    let (e, f) = get(&m, v);
                    (ae | e, af | f)
                });
                let d = op.deref(ctx);
                let pred =
                    Operation::is_op::<ICmpOp>(op, ctx) || Operation::is_op::<FCmpOp>(op, ctx);
                for r in d.results() {
                    // A pointer result is the derived pointer itself; an
                    // int result may still encode it. Predicates encode
                    // nothing.
                    let add = if pred {
                        (0, 0)
                    } else if r.get_type(ctx).deref(ctx).is::<PointerType>() {
                        (ex | fz, 0)
                    } else {
                        (0, ex | fz)
                    };
                    let e = m.entry(r).or_default();
                    if *e != add {
                        e.0 |= add.0;
                        e.1 |= add.1;
                        changed = true;
                    }
                }
                if d.successors().next().is_some() {
                    let list = succs.entry(op).or_insert_with(|| {
                        let o = Operation::get_op_dyn(op, ctx);
                        let Some(t) = op_cast::<dyn BranchOpInterface>(&*o) else {
                            return Vec::new();
                        };
                        d.successors()
                            .enumerate()
                            .map(|(si, s)| (s, t.successor_operands(ctx, si)))
                            .collect()
                    });
                    for (s, args) in list {
                        for (k, p) in s.deref(ctx).arguments().take(args.len()).enumerate() {
                            let add = get(&m, args[k]);
                            let e = m.entry(p).or_default();
                            if *e != add {
                                e.0 |= add.0;
                                e.1 |= add.1;
                                changed = true;
                            }
                        }
                    }
                }
                // The exception payload is produced by the callee; it may
                // encode any arg, so it lands in the exact band.
                if let Some(&(lp, _)) = st.invokes.get(&op)
                    && let Some(p) = lp.deref(ctx).arguments().next()
                {
                    let e = m.entry(p).or_default();
                    let add = ex | fz;
                    if e.0 != add {
                        e.0 |= add;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    Some((bs, m))
}

/// The captured-param mask of `f`'s body under callee table `nc`: every
/// param index whose derived value reaches a retaining use.
fn captured(
    ctx: &Context,
    st: &State,
    bs: &[Ptr<BasicBlock>],
    m: &Masks,
    nc: &FxHashMap<String, FxHashSet<u32>>,
) -> u64 {
    let mut cap = 0u64;
    for &b in bs {
        for op in ops(ctx, b) {
            macro_rules! is {
                ($t:ty) => {
                    Operation::is_op::<$t>(op, ctx)
                };
            }
            if is!(StoreOp) || is!(AtomicStoreOp) {
                cap |= mask(m, op.deref(ctx).get_operand(1));
            } else if is!(AtomicRmwOp) {
                cap |= mask(m, op.deref(ctx).get_operand(1));
            } else if is!(AtomicCmpxchgOp) {
                // (ptr, expected, new): only `new` is stored.
                cap |= mask(m, op.deref(ctx).get_operand(2));
            } else if is!(CallOp) {
                let free = direct_callee(ctx, st, op).and_then(|c| nc.get(c));
                let call = Operation::get_op::<CallOp>(op, ctx).unwrap();
                for (k, a) in call.args(ctx).iter().enumerate() {
                    if !free.is_some_and(|s| s.contains(&(k as u32))) {
                        cap |= mask(m, *a);
                    }
                }
            } else if is!(CallIntrinsicOp) {
                let d = op.deref(ctx);
                match st.intrinsics.get(&op).map(|s| s.as_str()) {
                    // Copies the src region out; its contents may hold
                    // pointers into it. dst is a region write.
                    Some("llvm.memcpy") | Some("llvm.memmove") => cap |= mask(m, d.get_operand(1)),
                    // Writes the dst region; nothing retained.
                    Some("llvm.memset") => {}
                    Some(n)
                        if n.starts_with("llvm.lifetime.")
                            || n.starts_with("llvm.dbg.")
                            || PURE.iter().any(|p| n.starts_with(p)) => {}
                    // Unknown intrinsic: may retain any operand.
                    _ => cap |= all_ops(ctx, m, op),
                }
            } else if is!(ReturnOp) || is!(InlineAsmOp) {
                cap |= all_ops(ctx, m, op);
            }
        }
    }
    cap
}

pub fn run(ctx: &Context, st: &mut State) {
    let bodied: Vec<String> = st
        .funcs
        .iter()
        .filter(|(_, f)| f.linkage != Linkage::Preemptible && has_body(ctx, f.op))
        .map(|(s, _)| s.clone())
        .collect();
    // Optimistic start — every param of every bodied function non-capturing —
    // then drop until fixpoint (handles recursion), like nowrite.rs. The
    // derivation propagation is `nc`-independent, so compute it once and only
    // re-scan capture sites per iteration.
    let ds: FxHashMap<&String, (Vec<Ptr<BasicBlock>>, Masks)> = bodied
        .iter()
        .filter_map(|s| deriv(ctx, st, st.funcs[s].op).map(|d| (s, d)))
        .collect();
    let mut nc: FxHashMap<String, FxHashSet<u32>> = bodied
        .iter()
        .map(|s| {
            let n = ds
                .get(s)
                .map(|(bs, _)| bs[0].deref(ctx).get_num_arguments())
                .unwrap_or(0)
                .min(MAX_PARAMS) as u32;
            (s.clone(), (0..n).collect())
        })
        .collect();
    loop {
        let mut changed = false;
        for s in &bodied {
            let Some((bs, m)) = ds.get(s) else { continue };
            let cap = captured(ctx, st, bs, m, &nc);
            let cur = nc.get_mut(s).unwrap();
            let keep: FxHashSet<u32> = cur
                .iter()
                .copied()
                .filter(|&i| cap & (1 << i) == 0)
                .collect();
            if keep.len() != cur.len() {
                *cur = keep;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if crate::env_set("PLIRON_STATS") {
        let (p, t): (usize, usize) = nc.values().fold((0, 0), |(p, t), s| (p + s.len(), t + 1));
        eprintln!(
            "nocapture {}: {p} non-capturing params across {t} bodies",
            st.cgu
        );
    }
    st.nocapture = nc;
}

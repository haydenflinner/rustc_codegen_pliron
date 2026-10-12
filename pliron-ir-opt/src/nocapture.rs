//! Bottom-up pointer-capture inference (LLVM FunctionAttrs' `captures(none)`):
//! a param is captured when a value derived from it is stored into memory,
//! returned, or passed to a callee that might retain it. Uses of the pointer
//! — loads, stores *through* it, GEPs, compares, math — are not captures.
//!
//! Every result derives from its operands' params (an integer result can
//! still encode a pointer via `ptrtoint`), except comparisons, which encode
//! nothing, so `p == q` doesn't capture `p`. A store of a derived value
//! anywhere — even into the callee's own frame — counts as a capture, so no
//! intra-frame alias tracking is needed.
//!
//! rustc's `DeducedParamAttrs` covers only `PassMode::Indirect` params — its
//! MIR analysis ignores `*_k` uses and `move`/`copy` of a param, so it is
//! garbage for `&`/`&mut` params. This pass analyzes the pliron body instead,
//! where the pointer is a value like any other, and covers every param of
//! every bodied function in the CGU.

use pliron::builtin::op_interfaces::{BranchOpInterface, CallOpInterface};
use pliron::context::Context;
use pliron::op::op_cast;
use pliron::operation::Operation;
use pliron::value::Value;
use pliron_llvm::ops::{
    AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp, FCmpOp,
    ICmpOp, InlineAsmOp, LoadOp, ReturnOp, StoreOp,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::inline::{blocks, direct_callee, ops};
use crate::nowrite::PURE;
use crate::state::State;
use crate::{Syms, bodied};

/// Params past this index are always treated as captured.
const MAX_PARAMS: u32 = 64;

/// The params a value may derive from, as a bit set joined by union.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
struct Mask(u64);

impl PartialOrd for Mask {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering::*;
        match (self.0 & !o.0 == 0, o.0 & !self.0 == 0) {
            (true, true) => Some(Equal),
            (true, false) => Some(Less),
            (false, true) => Some(Greater),
            _ => None,
        }
    }
}

impl ascent::Lattice for Mask {
    fn meet_mut(&mut self, o: Self) -> bool {
        let old = self.0;
        self.0 &= o.0;
        self.0 != old
    }
    fn join_mut(&mut self, o: Self) -> bool {
        let old = self.0;
        self.0 |= o.0;
        self.0 != old
    }
}

fn bits(m: Mask) -> impl Iterator<Item = u32> {
    (0..MAX_PARAMS).filter(move |i| m.0 & (1 << i) != 0)
}

ascent::ascent! {
    struct Capture;
    /// Param `i` of a function is value `v`.
    relation param(Value, u32);
    /// `d` is computed from `s` (op operand → result, branch arg → block
    /// param, call operand → its landing pad's exception payload).
    relation flow(Value, Value);
    /// Function `f` retains `v` (stores it, returns it, hands it to asm or
    /// an unknown intrinsic or an indirect call).
    relation retain(u32, Value);
    /// Function `f` passes `v` as argument `k` of a direct call to `c`.
    relation arg(u32, u32, u32, Value);
    /// Functions with an inferred table: bodied ones, with their param count.
    relation nparams(u32, u32);

    lattice der(Value, Mask);
    der(v, Mask(1 << i)) <-- param(v, i);
    der(d, m) <-- flow(s, d), der(s, m);

    /// Argument `k` of a call to `c` may be retained by `c`.
    relation keeps(u32, u32);
    keeps(c, k) <-- arg(_, c, k, _), !nparams(c, _);
    keeps(c, k) <-- arg(_, c, k, _), nparams(c, n), if *k >= *n;
    keeps(c, k) <-- arg(_, c, k, _), captured(c, k);

    relation captured(u32, u32);
    captured(f, i) <-- retain(f, v), der(v, m), for i in bits(*m);
    captured(f, i) <-- arg(f, c, k, v), keeps(c, k), der(v, m), for i in bits(*m);
}

pub fn run(ctx: &Context, st: &mut State) {
    let bodied = bodied(ctx, st);
    let mut ids = Syms::default();
    let mut p = Capture::default();
    for s in &bodied {
        let f = ids.id(s);
        let bs = blocks(ctx, st.funcs[s].op);
        let n = bs[0].deref(ctx).get_num_arguments() as u32;
        p.nparams.push((f, n.min(MAX_PARAMS)));
        for (i, a) in bs[0]
            .deref(ctx)
            .arguments()
            .enumerate()
            .take(MAX_PARAMS as usize)
        {
            p.param.push((a, i as u32));
        }
        for &b in &bs {
            for op in ops(ctx, b) {
                macro_rules! is {
                    ($t:ty) => {
                        Operation::is_op::<$t>(op, ctx)
                    };
                }
                let d = op.deref(ctx);
                // Comparisons encode nothing, and a load yields the
                // pointee, not the pointer: memory only holds a derived
                // pointer if a store put it there, which is a capture
                // already (or the caller's, as in LLVM's CaptureTracking).
                if !(is!(ICmpOp) || is!(FCmpOp) || is!(LoadOp) || is!(AtomicLoadOp)) {
                    for r in d.results() {
                        p.flow.extend(d.operands().map(|v| (v, r)));
                    }
                }
                if d.successors().next().is_some()
                    && let Some(t) =
                        op_cast::<dyn BranchOpInterface>(&*Operation::get_op_dyn(op, ctx))
                {
                    for (si, s) in d.successors().enumerate() {
                        let args = t.successor_operands(ctx, si);
                        p.flow
                            .extend(args.into_iter().zip(s.deref(ctx).arguments()));
                    }
                }
                // The exception payload is produced by the callee; it may
                // encode any arg.
                if let Some(&(lp, _)) = st.invokes.get(&op)
                    && let Some(x) = lp.deref(ctx).arguments().next()
                {
                    p.flow.extend(d.operands().map(|v| (v, x)));
                }
                let mut retain = |v: Value| p.retain.push((f, v));
                // Stored operands: (value, ptr), (ptr, value) and
                // (ptr, expected, new).
                if is!(StoreOp) || is!(AtomicStoreOp) {
                    retain(d.get_operand(0));
                } else if is!(AtomicRmwOp) {
                    retain(d.get_operand(1));
                } else if is!(AtomicCmpxchgOp) {
                    retain(d.get_operand(2));
                } else if is!(CallOp) {
                    let args = Operation::get_op::<CallOp>(op, ctx).unwrap().args(ctx);
                    match direct_callee(ctx, st, op) {
                        Some(c) => {
                            let c = ids.id(c);
                            for (k, a) in args.into_iter().enumerate() {
                                p.arg.push((f, c, k as u32, a));
                            }
                        }
                        None => args.into_iter().for_each(retain),
                    }
                } else if is!(CallIntrinsicOp) {
                    match st.intrinsics.get(&op).map(|s| s.as_str()) {
                        // Copies the src region out; its contents may hold
                        // pointers into it. dst is a region write.
                        Some("llvm.memcpy") | Some("llvm.memmove") => retain(d.get_operand(1)),
                        // Writes the dst region; nothing retained.
                        Some("llvm.memset") => {}
                        Some(n)
                            if n.starts_with("llvm.lifetime.")
                                || n.starts_with("llvm.dbg.")
                                || PURE.iter().any(|p| n.starts_with(p)) => {}
                        // Unknown intrinsic: may retain any operand.
                        _ => d.operands().for_each(retain),
                    }
                } else if is!(ReturnOp) || is!(InlineAsmOp) {
                    d.operands().for_each(retain);
                }
            }
        }
    }
    p.run();
    let mut nc: FxHashMap<String, FxHashSet<u32>> = FxHashMap::default();
    let cap: FxHashSet<(u32, u32)> = p.captured.iter().copied().collect();
    for s in &bodied {
        let f = ids.id(s);
        let n = p
            .nparams
            .iter()
            .find(|&&(g, _)| g == f)
            .map_or(0, |&(_, n)| n);
        nc.insert(
            s.clone(),
            (0..n).filter(|&i| !cap.contains(&(f, i))).collect(),
        );
    }
    if crate::env_set("PLIRON_STATS") {
        let (p, t): (usize, usize) = nc.values().fold((0, 0), |(p, t), s| (p + s.len(), t + 1));
        eprintln!(
            "nocapture {}: {p} non-capturing params across {t} bodies",
            st.cgu
        );
    }
    for (s, set) in &nc {
        let mut v: Vec<_> = set.iter().collect();
        v.sort();
        crate::dump_fact("nocapture", s, &v);
    }
    st.nocapture = nc;
}

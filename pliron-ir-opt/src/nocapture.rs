//! Bottom-up pointer-capture inference (LLVM FunctionAttrs' `captures(none)`):
//! a param is captured when a value derived from it is stored into memory,
//! returned, or passed to a callee that might retain it. Uses of the pointer
//! — loads, stores *through* it, GEPs, compares, math — are not captures.
//!
//! A result derives from its operands' params where its type can encode a
//! pointer (a `ptrtoint`/`inttoptr` is the sole int<->ptr bridge), except
//! comparisons, which encode nothing, so `p == q` doesn't capture `p`. A
//! store of a derived value counts as a capture unless the destination is a
//! frame-confined temp — `slot_loads`'s purity test — in which case the
//! pointer can only come back through that slot's load results, which the
//! flow relation tracks like any other derived value.
//!
//! rustc's `DeducedParamAttrs` covers only `PassMode::Indirect` params — its
//! MIR analysis ignores `*_k` uses and `move`/`copy` of a param, so it is
//! garbage for `&`/`&mut` params. This pass analyzes the pliron body instead,
//! where the pointer is a value like any other, and covers every param of
//! every bodied function in the CGU.

use pliron::builtin::op_interfaces::{BranchOpInterface, CallOpCallable, CallOpInterface};
use pliron::context::Context;
use pliron::linked_list::ContainsLinkedList;
use pliron::op::op_cast;
use pliron::operation::{OpDbg, Operation};
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::ops::{
    AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp, FCmpOp,
    GetElementPtrOp, ICmpOp, InlineAsmOp, IntToPtrOp, LoadOp, PtrToIntOp, ReturnOp, StoreOp,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::state::ConstVal;
use crate::types::{TyK, classify};

use crate::inline::{blocks, direct_callee};
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

/// `c` is the callee of an indirect call. If it is a load from a
/// frame-confined slot — every use a load, a store *into* it, a GEP or a
/// lifetime marker — all writes are visible `store`s, and each stored
/// value must be a known function address for the callee set to be
/// resolved. Anything else (a memcpy of opaque bytes, the slot passed to a
/// call, no stores at all) leaves the callee unknown.
fn indirect_callees(ctx: &Context, st: &State, c: Value) -> Option<Vec<String>> {
    let dbg = crate::env_get("PLIRON_NOCAPDBG").is_some();
    macro_rules! bail {
        ($m:expr) => {
            if dbg {
                eprintln!("NOCAPDBG resolve: {} at {}", $m, OpDbg { op: c.defining_op().unwrap_or_else(|| panic!()), ctx });
            }
            return None;
        };
    }
    let Some(op) = c.defining_op() else {
        return None;
    };
    if !Operation::is_op::<LoadOp>(op, ctx) {
        bail!("callee not a load");
    }
    let mut work = vec![op.deref(ctx).get_operand(0)];
    let mut seen = FxHashSet::default();
    let mut stored = Vec::new();
    while let Some(v) = work.pop() {
        if !seen.insert(v) {
            continue;
        }
        if !st.allocas.contains_key(&v) {
            let Some(d) = v.defining_op() else {
                bail!("non-alloca non-gep ptr (block arg)");
            };
            if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
                bail!("non-alloca ptr from non-gep");
            }
            work.push(d.deref(ctx).get_operand(0));
            continue;
        }
        for u in v.uses(ctx) {
            let o = u.user_op();
            if st.volatile.contains(&o) {
                bail!("volatile use");
            }
            if Operation::is_op::<GetElementPtrOp>(o, ctx) {
                work.push(o.deref(ctx).get_result(0));
            } else if Operation::is_op::<LoadOp>(o, ctx) {
            } else if Operation::is_op::<StoreOp>(o, ctx) && u.find_index(ctx) == 1 {
                stored.push(o.deref(ctx).get_operand(0));
            } else if !Operation::is_op::<CallIntrinsicOp>(o, ctx)
                || !matches!(
                    st.intrinsics.get(&o).map(|s| s.as_str()),
                    Some(n) if n.starts_with("llvm.lifetime.")
                )
            {
                bail!("impure slot use");
            }
        }
    }
    if stored.is_empty() {
        bail!("no stores into slot");
    }
    let mut syms = Vec::new();
    for v in stored {
        match st.consts.get(&v) {
            Some(ConstVal::Sym { sym, off: 0 }) => syms.push(sym.clone()),
            _ => {
                if dbg {
                    eprintln!("NOCAPDBG resolve: stored value not a const sym");
                }
                return None;
            }
        }
    }
    Some(syms)
}

pub fn run(ctx: &Context, st: &mut State) {
    let bodied = bodied(ctx, st);
    let mut ids = Syms::default();
    let mut p = Capture::default();
    let dbg = crate::env_get("PLIRON_NOCAPDBG");
    let mut dbgrets: Vec<(String, Value, String)> = Vec::new();
    let mut dbgargs: Vec<(String, Value, String)> = Vec::new();
    // `slot_loads`'s nested `accesses` walks — one memo for the whole
    // pass: the analysis never mutates the function, and the same temp
    // slot is typically the destination of many stores.
    let mut accmemo = crate::sroa::AccMemo::default();
    // `classify` on a value's type, memoized — the flow loop otherwise
    // re-walks the type per (result × operand) pair.
    let mut kinds: FxHashMap<Value, (bool, bool)> = FxHashMap::default();
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
            for op in b.deref(ctx).iter(ctx) {
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
                    // A value only carries a param's mask if it can encode
                    // a pointer derived from it: `ptrtoint`/`inttoptr` are
                    // the sole int<->ptr bridges, so e.g. an i64 call result
                    // can't be the ptr arg (no `store i8 (hash)` capture).
                    let mut pk = |v: Value| -> (bool, bool) {
                        *kinds.entry(v).or_insert_with(|| {
                            let k = classify(ctx, v.get_type(ctx));
                            (
                                matches!(k, TyK::Ptr | TyK::Func(..)),
                                matches!(k, TyK::Int(_)),
                            )
                        })
                    };
                    let bridge = is!(PtrToIntOp) || is!(IntToPtrOp);
                    for r in d.results() {
                        let (rp, ri) = pk(r);
                        for v in d.operands() {
                            let (vp, vi) = pk(v);
                            if bridge || !(vp && ri) && !(vi && rp) {
                                p.flow.push((v, r));
                            }
                        }
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
                let mut retain = |v: Value| {
                    p.retain.push((f, v));
                    if dbg.is_some_and(|d| s.contains(d)) {
                        dbgrets.push((s.clone(), v, OpDbg { op, ctx }.to_string()));
                    }
                };
                // Stored operands: (value, ptr), (ptr, value) and
                // (ptr, expected, new).
                if is!(StoreOp) {
                    // A store into a frame-confined temp (loads, stores and
                    // memcpy *into* it only — `slot_loads`'s purity test) is
                    // not a capture: the pointer can only come back through
                    // that slot's load results, so flow to them instead.
                    // Anything else retains — the pointer can roam.
                    let (sv, dp) = (d.get_operand(0), d.get_operand(1));
                    match crate::sroa::slot_loads(ctx, st, dp, 0, &mut accmemo) {
                        Some(ls) => p.flow.extend(ls.into_iter().map(|r| (sv, r))),
                        None => retain(sv),
                    }
                } else if is!(AtomicStoreOp) {
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
                                if dbg.is_some_and(|d| s.contains(d)) {
                                    dbgargs.push((s.clone(), a, OpDbg { op, ctx }.to_string()));
                                }
                            }
                        }
                        None => {
                            // A fn-ptr loaded from a frame-confined slot
                            // whose stores are all visible resolves to its
                            // possible callees — arg-typed like a direct
                            // call to each of them.
                            let cop = Operation::get_op::<CallOp>(op, ctx).unwrap();
                            let resolved =
                                if let CallOpCallable::Indirect(c) = cop.callee(ctx) {
                                    indirect_callees(ctx, st, c)
                                } else {
                                    None
                                };
                            match resolved {
                                Some(cs) => {
                                    for c in cs {
                                        let c = ids.id(&c);
                                        for (k, a) in args.clone().into_iter().enumerate() {
                                            p.arg.push((f, c, k as u32, a));
                                        }
                                    }
                                }
                                None => args.into_iter().for_each(retain),
                            }
                        }
                    }
                } else if is!(CallIntrinsicOp) {
                    match st.intrinsics.get(&op).map(|s| s.as_str()) {
                        // A bulk read+region write: neither operand is
                        // retained — the bytes move, but every derived
                        // pointer inside was already retained by the store
                        // (or memcpy chain head) that put it in memory.
                        Some("llvm.memcpy") | Some("llvm.memmove") | Some("llvm.memset") => {}
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
    if dbg.is_some() {
        let der: FxHashMap<Value, Mask> = p.der.iter().map(|(v, m)| (*v, *m)).collect();
        for (s, v, o) in &dbgrets {
            for i in bits(*der.get(v).unwrap_or(&Mask(0))) {
                eprintln!("NOCAPDBG {s}: param{i} retained via {v:?} at {o}");
            }
        }
        for (s, v, o) in &dbgargs {
            for i in bits(*der.get(v).unwrap_or(&Mask(0))) {
                eprintln!("NOCAPDBG {s}: param{i} via call-arg {v:?} at {o}");
            }
        }
    }
    let mut nc: FxHashMap<String, FxHashSet<u32>> = FxHashMap::default();
    let cap: FxHashSet<(u32, u32)> = p.captured.iter().copied().collect();
    // `nparams` is a relation Vec — linear `find` per fn is O(fns²).
    let nparams: FxHashMap<u32, u32> = p.nparams.iter().copied().collect();
    for s in &bodied {
        let f = ids.id(s);
        let n = nparams.get(&f).copied().unwrap_or(0);
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

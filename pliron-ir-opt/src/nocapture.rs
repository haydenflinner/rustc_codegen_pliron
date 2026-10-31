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
    AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp,
    ExtractValueOp, FCmpOp, GetElementPtrOp, ICmpOp, InlineAsmOp, InsertValueOp, IntToPtrOp, LoadOp,
    PtrToIntOp, ReturnOp, StoreOp,
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

/// A store into a slot with more loads than this flows through the slot
/// pointer instead of to each load (see the store case in `run`).
const SLOT_FANOUT: usize = 8;

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
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::nocapture", tracing::Level::DEBUG);
    macro_rules! bail {
        ($m:expr) => {
            if dbg {
                if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::nocapture", "NOCAPDBG resolve: {} at {}", $m, OpDbg { op: c.defining_op().unwrap_or_else(|| panic!()), ctx }) };
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
    let mut work = vec![op.deref(ctx).get_operand(ctx, 0)];
    let mut seen = FxHashSet::default();
    let mut stored = Vec::new();
    let mut ubuf = Vec::new();
    while let Some(v) = work.pop() {
        if !seen.insert(v) {
            continue;
        }
        if !st.allocas.contains(v) {
            let Some(d) = v.defining_op() else {
                bail!("non-alloca non-gep ptr (block arg)");
            };
            if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
                bail!("non-alloca ptr from non-gep");
            }
            work.push(d.deref(ctx).get_operand(ctx, 0));
            continue;
        }
        v.uses_into(ctx, &mut ubuf);
        for u in ubuf.iter().copied() {
            let o = u.user_op();
            if st.volatile.contains(o) {
                bail!("volatile use");
            }
            if Operation::is_op::<GetElementPtrOp>(o, ctx) {
                work.push(o.deref(ctx).get_result(ctx, 0));
            } else if Operation::is_op::<LoadOp>(o, ctx) {
            } else if Operation::is_op::<StoreOp>(o, ctx) && u.find_index(ctx) == 1 {
                stored.push(o.deref(ctx).get_operand(ctx, 0));
            } else if !Operation::is_op::<CallIntrinsicOp>(o, ctx)
                || !matches!(
                    st.intrinsics.get(o).map(|s| s.as_str()),
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
        match st.consts.get(v) {
            Some(ConstVal::Sym { sym, off: 0 }) => syms.push(sym.clone()),
            _ => {
                if dbg {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::nocapture", "NOCAPDBG resolve: stored value not a const sym")
                    };
                }
                return None;
            }
        }
    }
    Some(syms)
}

/// `__rust_alloc`, `__rust_alloc_zeroed`, `__rust_realloc`,
/// `__rust_dealloc` (plain or v0-mangled under `__rustc`).
fn alloc_shim(sym: &str) -> bool {
    ["__rust_alloc", "__rust_alloc_zeroed", "__rust_realloc", "__rust_dealloc"]
        .iter()
        .any(|n| sym == *n || sym.ends_with(&format!("_{n}")))
}

/// `PLIRON_NOCAP_FIELDS`: (may hold a pointer, may hold an integer) for a
/// value of type `ty` — a pointer or function is the first, an integer the
/// second, an aggregate the union over its leaves. Floats and anything
/// else count as both: a pointer can reach a float through int bitcasts.
fn carriers(ctx: &Context, ty: pliron::r#type::TypeHandle, depth: u32) -> (bool, bool) {
    match classify(ctx, ty) {
        TyK::Ptr | TyK::Func(..) => (true, false),
        TyK::Int(_) => (false, true),
        TyK::Struct(fs, _) if depth < 8 => fs.iter().fold((false, false), |(p, i), &f| {
            let (fp, fi) = carriers(ctx, f, depth + 1);
            (p || fp, i || fi)
        }),
        TyK::Array(e, _) | TyK::Vector(e, _) if depth < 8 => carriers(ctx, e, depth + 1),
        _ => (true, true),
    }
}

/// The value `extract_value` reads: walk the `insert_value` chain under
/// its aggregate to the insert of the same index path. `None` when the
/// chain ends first (block arg, load, call) or an insert overlaps the path
/// only partly — the caller then flows the whole aggregate.
fn extract_src(ctx: &Context, op: pliron::context::Ptr<Operation>) -> Option<Value> {
    let idx = Operation::get_op::<ExtractValueOp>(op, ctx)?.indices(ctx);
    let mut agg = op.deref(ctx).get_operand(ctx, 0);
    for _ in 0..64 {
        let ins = agg.defining_op().filter(|&o| Operation::is_op::<InsertValueOp>(o, ctx))?;
        let iidx = Operation::get_op::<InsertValueOp>(ins, ctx)?.indices(ctx);
        if iidx == idx {
            return Some(ins.deref(ctx).get_operand(ctx, 1));
        }
        if iidx.starts_with(&idx) || idx.starts_with(&iidx) {
            return None;
        }
        agg = ins.deref(ctx).get_operand(ctx, 0);
    }
    None
}

// r[impl iropt.ipo.nocapture]
pub fn run(ctx: &Context, st: &mut State) {
    let bodied = bodied(ctx, st);
    let mut ids = Syms::default();
    let mut p = Capture::default();
    let dbg = crate::env_get("PLIRON_NOCAPDBG");
    let fields = crate::env_get("PLIRON_NOCAP_FIELDS") == Some("1");
    let mut dbgrets: Vec<(String, Value, String)> = Vec::new();
    let mut dbgargs: Vec<(String, Value, String)> = Vec::new();
    // `slot_loads`'s nested `accesses` walks — one memo for the whole
    // pass: the analysis never mutates the function, and the same temp
    // slot is typically the destination of many stores.
    let mut accmemo = crate::sroa::AccMemo::default();
    // `classify` on a value's type, memoized — the flow loop otherwise
    // re-walks the type per (result × operand) pair.
    let mut kinds: FxHashMap<Value, (bool, bool)> = FxHashMap::default();
    // Slot pointers already linked to their loads (see the store case).
    let mut via_slot: FxHashSet<Value> = FxHashSet::default();
    for s in &bodied {
        let f = ids.id(s);
        let bs = blocks(ctx, st.funcs[s].op);
        let n = bs[0].deref(ctx).get_num_arguments() as u32;
        p.nparams.push((f, n.min(MAX_PARAMS)));
        for (i, a) in bs[0]
            .deref(ctx)
            .arguments(ctx)
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
                            if fields {
                                return carriers(ctx, v.get_type(ctx), 0);
                            }
                            let k = classify(ctx, v.get_type(ctx));
                            (
                                matches!(k, TyK::Ptr | TyK::Func(..)),
                                matches!(k, TyK::Int(_)),
                            )
                        })
                    };
                    let bridge = is!(PtrToIntOp) || is!(IntToPtrOp);
                    // A field read back from an aggregate carries only what
                    // was inserted at that index, not the whole aggregate's
                    // mask: `Enumerate::next`'s `{i64 index, ptr}` pair made
                    // every loop index "derive" from the iterated pointer, so
                    // `*w = seed * (i + 1)` captured the slice's owner.
                    // `PLIRON_NOCAP_FIELDS=1` (default off for now).
                    let field = (fields && is!(ExtractValueOp)).then(|| extract_src(ctx, op)).flatten();
                    // An allocator shim's result is a fresh allocation, not
                    // derived from its args (`__rust_realloc` only ever sees a
                    // heap pointer in a valid program).
                    let fresh = fields && is!(CallOp) && direct_callee(ctx, st, op).is_some_and(|c| alloc_shim(c));
                    for r in d.results(ctx) {
                        let (rp, ri) = pk(r);
                        let srcs: Vec<Value> = match field {
                            _ if fresh => Vec::new(),
                            Some(v) => vec![v],
                            None => d.operands(ctx).collect(),
                        };
                        for v in srcs {
                            let (vp, vi) = pk(v);
                            // Scalars: no ptr <-> int flow but through a
                            // bridge. `PLIRON_NOCAP_FIELDS`: by what each
                            // type's leaves can hold, so an all-int struct
                            // a call returns can't carry its ptr operand.
                            let ok = if fields { vp && rp || vi && ri } else { !(vp && ri) && !(vi && rp) };
                            if bridge || ok {
                                p.flow.push((v, r));
                            }
                        }
                    }
                }
                if d.successors(ctx).next().is_some()
                    && let Some(t) =
                        op_cast::<dyn BranchOpInterface>(&*Operation::get_op_dyn(op, ctx))
                {
                    for (si, s) in d.successors(ctx).enumerate() {
                        let args = t.successor_operands(ctx, si);
                        p.flow
                            .extend(args.into_iter().zip(s.deref(ctx).arguments(ctx)));
                    }
                }
                // The exception payload is produced by the callee; it may
                // encode any arg.
                if let Some(&(lp, _)) = st.invokes.get(op)
                    && let Some(x) = lp.deref(ctx).arguments(ctx).next()
                {
                    p.flow.extend(d.operands(ctx).map(|v| (v, x)));
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
                    // Many loads: route through the slot pointer, stores ->
                    // `dp` -> each load, so a slot with S stores and L loads
                    // costs S + L flows, not S x L (1.2e9 in cranelift-
                    // codegen's ISLE matchers after sroa; the fixpoint never
                    // finished). Sound: a frame-confined slot pointer only
                    // feeds loads, stores and GEPs of the same slot.
                    let (sv, dp) = (d.get_operand(ctx, 0), d.get_operand(ctx, 1));
                    match crate::sroa::slot_loads(ctx, st, dp, 0, &mut accmemo) {
                        Some(ls) if ls.len() > SLOT_FANOUT => {
                            p.flow.push((sv, dp));
                            if via_slot.insert(dp) {
                                p.flow.extend(ls.into_iter().map(|r| (dp, r)));
                            }
                        }
                        Some(ls) => p.flow.extend(ls.into_iter().map(|r| (sv, r))),
                        None => retain(sv),
                    }
                } else if is!(AtomicStoreOp) {
                    retain(d.get_operand(ctx, 0));
                } else if is!(AtomicRmwOp) {
                    retain(d.get_operand(ctx, 1));
                } else if is!(AtomicCmpxchgOp) {
                    retain(d.get_operand(ctx, 2));
                } else if is!(CallOp) {
                    let args = Operation::get_op::<CallOp>(op, ctx).unwrap().args(ctx);
                    match direct_callee(ctx, st, op) {
                        // The allocator shims retain no argument: the old block
                        // of a realloc/dealloc is dead once they return.
                        Some(c) if fields && alloc_shim(c) => {}
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
                            let resolved = if let CallOpCallable::Indirect(c) = cop.callee(ctx) {
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
                    match st.intrinsics.get(op).map(|s| s.as_str()) {
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
                        _ => d.operands(ctx).for_each(retain),
                    }
                } else if is!(ReturnOp) || is!(InlineAsmOp) {
                    d.operands(ctx).for_each(retain);
                }
            }
        }
    }
    if crate::env_get("PLIRON_TRACE_FILE").is_some() {
        let sizes = format!(
            "nocap-sizes flow={} param={} retain={} arg={}",
            p.flow.len(),
            p.param.len(),
            p.retain.len(),
            p.arg.len()
        );
        pliron_cfg::trace(&sizes, 0.0, &st.cgu);
    }
    let trun = std::time::Instant::now();
    p.run();
    pliron_cfg::trace("nocap-fixpoint", trun.elapsed().as_secs_f64() * 1e3, &st.cgu);
    if dbg.is_some()
        && pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::nocapture", tracing::Level::DEBUG)
    {
        let der: FxHashMap<Value, Mask> = p.der.iter().map(|(v, m)| (*v, *m)).collect();
        for (s, v, o) in &dbgrets {
            for i in bits(*der.get(v).unwrap_or(&Mask(0))) {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::nocapture", "NOCAPDBG {s}: param{i} retained via {v:?} at {o}")
                };
            }
        }
        for (s, v, o) in &dbgargs {
            for i in bits(*der.get(v).unwrap_or(&Mask(0))) {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::nocapture", "NOCAPDBG {s}: param{i} via call-arg {v:?} at {o}")
                };
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
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        let (p, t): (usize, usize) = nc.values().fold((0, 0), |(p, t), s| (p + s.len(), t + 1));
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "nocapture {}: {p} non-capturing params across {t} bodies",
                st.cgu
            )
        };
    }
    // Delta-debugging a bad fact: `PLIRON_NOCAP_LOG=<file>` appends every
    // function with a non-empty set (one symbol per line, per pid), and
    // `PLIRON_NOCAP_ONLY=@<file>` keeps facts only for the listed symbols.
    if let Some(log) = crate::env_get("PLIRON_NOCAP_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{log}.{}", std::process::id()))
        {
            for (s, set) in &nc {
                if !set.is_empty() {
                    let _ = writeln!(f, "{s}");
                }
            }
        }
    }
    if let Some(only) = crate::env_get("PLIRON_NOCAP_ONLY")
        && let Some(path) = only.strip_prefix('@')
    {
        let keep: FxHashSet<String> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        for (s, set) in nc.iter_mut() {
            if !keep.contains(s) {
                set.clear();
            }
        }
    }
    for (s, set) in &nc {
        let mut v: Vec<_> = set.iter().collect();
        v.sort();
        crate::dump_fact("nocapture", s, &v);
    }
    st.nocapture = nc.into_iter().collect();
}

// r[verify iropt.ipo.nocapture]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Exts, FuncInfo};
    use cranelift_module::Linkage;
    use pliron::builtin::op_interfaces::SymbolOpInterface;
    use pliron::builtin::ops::ModuleOp;
    use pliron::irfmt::parsers::spaced;
    use pliron::op::Op;
    use pliron::parsable::parse_from_str;
    use pliron_llvm::ops::{AllocaOp, FuncOp};

    /// `p` goes through an 8-byte temp slot, comes back as an `i64` and is
    /// stored to `out` — a byte copy of the pointer. Only pointer-typed
    /// slot loads were tracked, so `p` came out `nocapture`; serde_json's
    /// `BTreeMap::extract_if` (whose iterator holds `&mut self.length`)
    /// then let callers keep the map's length across `ExtractIf::next`.
    /// Parse `input`, register every `llvm.func` (and its allocas) in a
    /// fresh `State`, run the pass, and return `@f`'s nocapture set.
    fn analyze_f(input: &str, nparams: usize) -> FxHashSet<u32> {
        let ctx = &mut Context::new();
        let op = parse_from_str(spaced(Operation::top_level_parser()), ctx, input).expect("parse");
        let mop = Operation::get_op::<ModuleOp>(op, ctx).unwrap();
        let mut st = State::default();
        for fop in blocks(ctx, mop.get_operation())
            .into_iter()
            .flat_map(|b| crate::inline::ops(ctx, b))
        {
            let Some(f) = Operation::get_op::<FuncOp>(fop, ctx) else {
                continue;
            };
            for b in blocks(ctx, fop) {
                for o in crate::inline::ops(ctx, b) {
                    if Operation::is_op::<AllocaOp>(o, ctx) {
                        st.allocas.insert(o.deref(ctx).get_result(ctx, 0), (8, 8));
                    }
                }
            }
            let sym = f.get_symbol_name(ctx).to_string();
            st.ident_to_sym.insert(sym.clone(), sym.clone());
            st.funcs.insert(
                sym,
                FuncInfo {
                    op: fop,
                    ty: f.get_type(ctx).into(),
                    linkage: Linkage::Local,
                    exts: Exts::default(),
                    no_inline: false,
                    always_inline: false,
                    inline_hint: false,
                    cold: false,
                    nounwind: false,
                    odr: false,
                    sret_ty: None,
                    frozen: Vec::new(),
                    noalias: Vec::new(),
                    nonnull: Vec::new(),
                    nparams,
                    tail_cc: false,
                },
            );
        }
        run(ctx, &mut st);
        st.nocapture.get("f").expect("f analyzed").clone()
    }

    #[test]
    fn int_copy_through_slot_captures() {
        let input = r#"
        builtin.module @m {
        ^b():
          llvm.func @f: llvm.func <llvm.void(llvm.ptr (0), llvm.ptr (0)) variadic = false> [] {
          ^e(p: llvm.ptr (0), out: llvm.ptr (0)):
            one = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            s = llvm.alloca [builtin.integer i64 x one] : llvm.ptr (0);
            llvm.store *s <- p;
            w = llvm.load s : builtin.integer i64;
            llvm.store *out <- w;
            llvm.return
          }
        }
        "#;
        let nc = analyze_f(input, 2);
        assert!(!nc.contains(&0), "p escaped as i64 bytes but stayed nocapture");
        assert!(nc.contains(&1), "out is only written through");
    }

    /// A slot with more than `SLOT_FANOUT` loads routes its stores through
    /// the slot pointer (S + L flows instead of S x L; cranelift-codegen's
    /// ISLE matchers made 1.2e9 after sroa and the fixpoint never ended).
    /// The routed path must still carry the stored pointer to every load:
    /// `p` leaks through the last of twelve loads, `q` only through none.
    #[test]
    fn many_load_slot_still_carries_stores() {
        let input = r#"
        builtin.module @m {
        ^b():
          llvm.func @f: llvm.func <llvm.void(llvm.ptr (0), llvm.ptr (0), llvm.ptr (0)) variadic = false> [] {
          ^e(p: llvm.ptr (0), q: llvm.ptr (0), out: llvm.ptr (0)):
            one = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            s = llvm.alloca [llvm.ptr (0) x one] : llvm.ptr (0);
            t = llvm.alloca [llvm.ptr (0) x one] : llvm.ptr (0);
            llvm.store *s <- p;
            llvm.store *t <- q;
            w0 = llvm.load s : llvm.ptr (0);
            w1 = llvm.load s : llvm.ptr (0);
            w2 = llvm.load s : llvm.ptr (0);
            w3 = llvm.load s : llvm.ptr (0);
            w4 = llvm.load s : llvm.ptr (0);
            w5 = llvm.load s : llvm.ptr (0);
            w6 = llvm.load s : llvm.ptr (0);
            w7 = llvm.load s : llvm.ptr (0);
            w8 = llvm.load s : llvm.ptr (0);
            w9 = llvm.load s : llvm.ptr (0);
            w10 = llvm.load s : llvm.ptr (0);
            w11 = llvm.load s : llvm.ptr (0);
            llvm.store *out <- w11;
            llvm.return
          }
        }
        "#;
        let nc = analyze_f(input, 3);
        assert!(!nc.contains(&0), "p reaches `out` through the slot's 12th load");
        assert!(nc.contains(&1), "q sits in its own slot, never loaded");
        assert!(nc.contains(&2), "out is only written through");
    }
}

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
use pliron::printable::Printable;
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

/// `CapNode`'s slot flows compare byte ranges up to this many loads per slot
/// (`slot_flows`), and route slot-wide past it.
const SLOT_PRECISE: usize = 256;

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

/// Depths `CapNode` tracks per param: 0 (the param), 1 (pointers loaded out
/// of its memory), 2 (pointers loaded through those). Node `3i + d`.
pub const DEPTHS: u32 = 3;

/// For a value at depth `d` of a param stored at depth `j` of a slot that a
/// call gets: (param depth, callee depth) pairs whose capture links, and the
/// slot's own path (callee depth `m < j`: what the value points at is
/// reachable through it).
fn slot_links(d: u32, j: u32) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = (0..DEPTHS - d).filter(|x| j + x < DEPTHS).map(|x| (d + x, j + x)).collect();
    v.extend((0..j).map(|m| (d, m)));
    v
}

/// The first param depth such a store reaches past the callee's last
/// tracked depth: captured, as far as `CapNode` can tell.
fn slot_over(d: u32, j: u32) -> Vec<u32> {
    (0..DEPTHS - d).find(|x| j + x >= DEPTHS).map(|x| d + x).into_iter().collect()
}

ascent::ascent! {
    /// Capture over nodes (`PLIRON_NOCAP_DEEP`, and the summaries of
    /// `PLIRON_NOCAPSUM`): node `3i + d` is what lies at depth `d` of param
    /// `i` (0: the param; 1: pointers loaded straight out of its memory; 2:
    /// pointers loaded through those). If a node is captured, every deeper
    /// one is: what a kept pointer reaches is reachable.
    ///
    /// A pointer stored into a stack slot that only loads and call arguments
    /// see (`slot_calls`) sits at depth `j` of what the call gets (1, or 2
    /// when the slot's own address is stored into another such slot: a
    /// closure environment holding another closure's, hashbrown's `find`
    /// probe holding `&mut equivalent_key(k)`), and is captured only if the
    /// callee captures that depth. LLVM's `captures(none)` has no depth and
    /// calls every such store a capture.
    ///
    /// `local(_)`: a callee with no body and no adopted facts (`opaque`)
    /// captures every node. `summary(_)`: it becomes an edge,
    /// `dep(f, x, c, z)`, solved crate-wide behind the LTO barrier.
    struct CapNode;
    relation param(Value, u32);
    relation flow(Value, Value);
    /// `ld(address, result)` for every load.
    relation ld(Value, Value);
    relation retain(u32, Value);
    /// `f` copies memory out of `v` (memcpy/memmove source).
    relation memsrc(u32, Value);
    relation arg(u32, u32, u32, Value);
    /// `f` stores `v` at depth `j` of what argument `k` of a call to `c` is.
    relation slotarg(u32, Value, u32, u32, u32);
    relation nparams(u32, u32);
    relation opaque(u32);
    relation capin(u32, u32);
    relation local(u8);
    relation summary(u8);

    lattice der(Value, Mask);
    der(v, Mask(1 << i)) <-- param(v, i);
    der(d, m) <-- flow(s, d), der(s, m);
    lattice dd(Value, Mask);
    dd(r, m) <-- ld(a, r), der(a, m);
    dd(d, m) <-- flow(s, d), dd(s, m);
    lattice dd2(Value, Mask);
    dd2(r, m) <-- ld(a, r), dd(a, m);
    dd2(d, m) <-- flow(s, d), dd2(s, m);

    /// Node `x` of `f` is captured if node `z` of `c` is.
    relation pass(u32, u32, u32, u32);
    pass(f, 3 * i + e, c, 3 * k + e) <-- arg(f, c, k, v), der(v, m), for i in bits(*m), for e in 0..3u32;
    pass(f, 3 * i + 1 + e, c, 3 * k + e) <-- arg(f, c, k, v), dd(v, m), for i in bits(*m), for e in 0..2u32;
    pass(f, 3 * i + 2, c, 3 * k) <-- arg(f, c, k, v), dd2(v, m), for i in bits(*m);
    pass(f, 3 * i + fd, c, 3 * k + cd) <-- slotarg(f, v, c, k, j), der(v, m), for i in bits(*m), for (fd, cd) in slot_links(0, *j);
    pass(f, 3 * i + fd, c, 3 * k + cd) <-- slotarg(f, v, c, k, j), dd(v, m), for i in bits(*m), for (fd, cd) in slot_links(1, *j);
    pass(f, 3 * i + fd, c, 3 * k + cd) <-- slotarg(f, v, c, k, j), dd2(v, m), for i in bits(*m), for (fd, cd) in slot_links(2, *j);

    relation cap(u32, u32);
    cap(c, x) <-- capin(c, x);
    cap(f, 3 * i) <-- retain(f, v), der(v, m), for i in bits(*m);
    cap(f, 3 * i + 1) <-- retain(f, v), dd(v, m), for i in bits(*m);
    cap(f, 3 * i + 2) <-- retain(f, v), dd2(v, m), for i in bits(*m);
    cap(f, 3 * i + 1) <-- memsrc(f, v), der(v, m), for i in bits(*m);
    cap(f, 3 * i + 2) <-- memsrc(f, v), dd(v, m), for i in bits(*m);
    cap(f, 3 * i + fd) <-- slotarg(f, v, _, _, j), der(v, m), for i in bits(*m), for fd in slot_over(0, *j);
    cap(f, 3 * i + fd) <-- slotarg(f, v, _, _, j), dd(v, m), for i in bits(*m), for fd in slot_over(1, *j);
    cap(f, x + 1) <-- cap(f, x), if *x % 3 < 2;
    cap(f, x) <-- pass(f, x, c, z), nparams(c, n), if *z >= 3 * *n;
    cap(f, x) <-- pass(f, x, c, z), cap(c, z);
    cap(f, x) <-- pass(f, x, c, _), opaque(c), local(_);

    relation dep(u32, u32, u32, u32);
    dep(f, x, c, z) <-- pass(f, x, c, z), opaque(c), summary(_);
    dep(f, x, c2, y) <-- pass(f, x, c, z), nparams(c, n), if *z < 3 * *n, dep(c, z, c2, y);
    dep(f, x + 1, c, z) <-- dep(f, x, c, z), if *x % 3 < 2;
}

/// One node's capture summary (`PLIRON_NOCAPSUM`; node `3i + d` is depth
/// `d` of param `i`, see `CapNode`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapSum {
    /// Captured by the function itself (or through a callee with a body).
    Captured,
    /// Captured iff one of these is: (callee symbol, its fn type as this
    /// CGU declares it, callee node). Empty: never captured.
    Deps(Vec<(String, String, u32)>),
}

impl CapSum {
    /// `have` and `other` describe copies of one symbol: a param is captured
    /// if any copy captures it, and otherwise depends on the union of the
    /// copies' edges. A param one copy does not list is captured.
    pub fn merge(have: &mut Vec<CapSum>, other: &[CapSum]) {
        let n = have.len().max(other.len());
        for i in 0..n {
            let merged = match (have.get(i), other.get(i)) {
                (Some(CapSum::Deps(a)), Some(CapSum::Deps(b))) => {
                    let mut d = a.clone();
                    for e in b {
                        if !d.contains(e) {
                            d.push(e.clone());
                        }
                    }
                    CapSum::Deps(d)
                }
                _ => CapSum::Captured,
            };
            if i < have.len() {
                have[i] = merged;
            } else {
                have.push(merged);
            }
        }
    }
}

/// The crate-wide capture solve over every CGU's merged summaries: the
/// least set of captured (copy, param) closed under the edges, where an
/// edge to a symbol no CGU emitted under that type (an import, an
/// ABI-divergent copy) or to an argument past its summary captures.
/// Returns each copy's non-captured params.
// r[impl iropt.ipo.nocapsum]
pub fn solve_capsum(
    sums: &FxHashMap<(String, String), Vec<CapSum>>,
) -> FxHashMap<(String, String), FxHashSet<u32>> {
    type Node<'a> = (&'a (String, String), u32);
    let mut captured: FxHashSet<Node> = FxHashSet::default();
    let mut users: FxHashMap<Node, Vec<Node>> = FxHashMap::default();
    let mut work: Vec<Node> = Vec::new();
    for (key, ps) in sums {
        for (i, s) in ps.iter().enumerate() {
            let node = (key, i as u32);
            match s {
                CapSum::Captured => {
                    if captured.insert(node) {
                        work.push(node);
                    }
                }
                CapSum::Deps(ds) => {
                    for (cs, cty, k) in ds {
                        match sums.get_key_value(&(cs.clone(), cty.clone())) {
                            Some((ck, cps)) if (*k as usize) < cps.len() => {
                                users.entry((ck, *k)).or_default().push(node);
                            }
                            _ => {
                                if captured.insert(node) {
                                    work.push(node);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    while let Some(n) = work.pop() {
        if let Some(us) = users.get(&n) {
            for &u in us {
                if captured.insert(u) {
                    work.push(u);
                }
            }
        }
    }
    let mut out = FxHashMap::default();
    for (key, ps) in sums {
        let free: FxHashSet<u32> =
            (0..ps.len() as u32).filter(|&i| !captured.contains(&(key, i))).collect();
        out.insert(key.clone(), free);
    }
    out
}

/// `p`'s byte offset from the stack slot it points into: `(lo, exact)`, the
/// offset is `lo` (`exact`) or at least `lo`. Constant GEPs add exactly; a
/// GEP with a variable index adds at least its constant part when it is
/// `nuw` (rustc's `ptr.add`: indices non-negative, no unsigned wrap), and
/// makes the offset unknown (`None`) otherwise. `mem::swap`'s word loop in
/// hashbrown's `prepare_resize` indexes its scope guard's value that way.
fn slot_off(ctx: &Context, st: &State, mut p: Value) -> Option<(i64, bool)> {
    let (mut off, mut exact) = (0i64, true);
    for _ in 0..16 {
        if st.allocas.contains(p) {
            return Some((off, exact));
        }
        let d = p.defining_op()?;
        if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
            return None;
        }
        let (o, e) = match crate::sroa::gep_offset(ctx, st, d) {
            Some(o) => (o, true),
            None => (nuw_gep_floor(ctx, st, d)?, false),
        };
        off = off.checked_add(o)?;
        exact &= e;
        p = d.deref(ctx).get_operand(ctx, 0);
    }
    None
}

/// The least byte offset a `nuw` GEP with variable indices can add: its
/// constant indices' contributions, each variable index taken as 0. `None`
/// without `nuw`, or for a variable struct index.
fn nuw_gep_floor(ctx: &Context, st: &State, op: pliron::context::Ptr<Operation>) -> Option<i64> {
    use pliron_llvm::attributes::GepNoWrapFlags;
    use pliron_llvm::ops::GepIndex;
    let gep = Operation::get_op::<GetElementPtrOp>(op, ctx)?;
    if !gep.no_wrap_flags(ctx).contains(GepNoWrapFlags::NUW) {
        return None;
    }
    let mut cur = gep.src_elem_type(ctx);
    let mut off: i64 = 0;
    for (k, idx) in gep.indices(ctx).iter().enumerate() {
        let c: Option<i64> = match idx {
            GepIndex::Constant(c) => Some(*c as i64),
            GepIndex::Value(v) => crate::sroa::const_int(ctx, st, *v).map(|c| c as i64),
        };
        if k == 0 {
            if let Some(c) = c {
                off = off.checked_add(c.checked_mul(crate::types::size_align(ctx, cur).0 as i64)?)?;
            }
            continue;
        }
        match classify(ctx, cur) {
            TyK::Struct(fs, packed) => {
                let c = c? as usize;
                off = off.checked_add(*crate::types::struct_offsets(ctx, &fs, packed).0.get(c)? as i64)?;
                cur = *fs.get(c)?;
            }
            TyK::Array(e, _) | TyK::Vector(e, _) => {
                if let Some(c) = c {
                    off = off.checked_add(c.checked_mul(crate::types::size_align(ctx, e).0 as i64)?)?;
                }
                cur = e;
            }
            _ => return None,
        }
    }
    Some(off)
}

/// `CapNode`'s store -> load flows for a store of `sv` through `dp` into a
/// frame slot read by `loads`: only to loads whose bytes overlap the
/// store's. hashbrown's scope guard holds `&alloc` (derived from the table)
/// next to the new table's control pointer; slot-wide flows made the
/// control pointer "derived from" the table, and writing it back into
/// `self` a capture. Unknown offsets overlap everything; past
/// `SLOT_FANOUT` loads the slot-wide route through `dp` is kept.
fn slot_flows(
    ctx: &Context,
    st: &State,
    sv: Value,
    dp: Value,
    loads: &[Value],
    via: &mut FxHashSet<Value>,
    lmemo: &mut FxHashMap<Value, Option<(i64, i64)>>,
    out: &mut Vec<(Value, Value)>,
) {
    // Byte ranges are compared pairwise up to this many loads; only the
    // overlapping pairs become flows, so the flow count stays small.
    if loads.len() > SLOT_PRECISE {
        out.push((sv, dp));
        if via.insert(dp) {
            out.extend(loads.iter().map(|&r| (dp, r)));
        }
        return;
    }
    let sz = |v: Value| crate::types::size_align(ctx, v.get_type(ctx)).0 as i64;
    // byte ranges, `i64::MAX` an open upper end
    let range = |o: Option<(i64, bool)>, n: i64| o.map(|(lo, exact)| (lo, if exact { lo + n } else { i64::MAX }));
    let so = range(slot_off(ctx, st, dp), sz(sv));
    for &r in loads {
        let lo = *lmemo.entry(r).or_insert_with(|| {
            range(r.defining_op().and_then(|l| slot_off(ctx, st, l.deref(ctx).get_operand(ctx, 0))), sz(r))
        });
        let disjoint = matches!((so, lo), (Some((a, b)), Some((c, d))) if b <= c || d <= a);
        if !disjoint {
            if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::nocapture", tracing::Level::TRACE) {
                tracing::trace!(target: "pliron::nocapture", "slotflow {sv:?} -> {r:?}: store {so:?} load {lo:?}");
            }
            out.push((sv, r));
        }
    }
}

/// When the stack slot `dp` points into is seen only by loads, stores into
/// it, lifetime markers, mem* writes into it and call arguments (direct, or
/// indirect with resolved callees): its loads and every (callee, argument)
/// it is passed as. `None` for anything else (the address stored as a
/// value, copied out of, passed to an unresolved indirect call).
fn slot_calls(ctx: &Context, st: &State, dp: Value, ids: &mut Syms) -> Option<(Vec<Value>, Vec<(u32, u32, u32)>)> {
    let r = slot_calls_why(ctx, st, dp, ids, 1);
    if let Err(why) = &r
        && pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::nocapture", tracing::Level::TRACE)
    {
        tracing::trace!(target: "pliron::nocapture", "slot_calls {dp:?}: {why}");
    }
    r.ok()
}

/// `j`: the depth a pointer stored into this slot has in what the calls
/// get (1 for the slot itself; 2 for a slot whose address is stored into
/// another one, which may then have no loads: a reload would hand the
/// caller this slot's address and so its contents).
fn slot_calls_why(ctx: &Context, st: &State, dp: Value, ids: &mut Syms, j: u32) -> Result<(Vec<Value>, Vec<(u32, u32, u32)>), String> {
    let mut root = dp;
    for _ in 0..8 {
        if st.allocas.contains(root) {
            break;
        }
        let d = root.defining_op().ok_or("root is a block arg")?;
        if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
            return Err(format!("root through {}", Operation::get_opid(d, ctx)));
        }
        root = d.deref(ctx).get_operand(ctx, 0);
    }
    if !st.allocas.contains(root) {
        return Err("not a slot".into());
    }
    let (mut loads, mut calls) = (Vec::new(), Vec::new());
    let mut work = vec![root];
    let mut budget = 512usize;
    while let Some(v) = work.pop() {
        for u in v.uses_iter(ctx) {
            budget = budget.checked_sub(1).ok_or("budget")?;
            let o = u.user_op();
            let i = u.find_index(ctx);
            if st.volatile.contains(o) {
                return Err("volatile".into());
            }
            if Operation::is_op::<GetElementPtrOp>(o, ctx) && i == 0 {
                work.push(o.deref(ctx).get_result(ctx, 0));
            } else if Operation::is_op::<LoadOp>(o, ctx) {
                loads.push(o.deref(ctx).get_result(ctx, 0));
            } else if Operation::is_op::<StoreOp>(o, ctx) && i == 1 {
            } else if Operation::is_op::<StoreOp>(o, ctx) && i == 0 && j + 1 < DEPTHS {
                // this slot's address goes into another slot: one level deeper
                let outer = o.deref(ctx).get_operand(ctx, 1);
                let (ls, cs) = slot_calls_why(ctx, st, outer, ids, j + 1).map_err(|e| format!("outer slot: {e}"))?;
                if !ls.is_empty() {
                    return Err("outer slot is loaded".into());
                }
                calls.extend(cs);
            } else if Operation::is_op::<ICmpOp>(o, ctx) {
            } else if let Some(n) = st.intrinsics.get(o).map(|s| s.as_str()) {
                let ok = n.starts_with("llvm.lifetime.")
                    || (matches!(n, "llvm.memcpy" | "llvm.memmove" | "llvm.memset") && i == 0);
                if !ok {
                    return Err(format!("intrinsic {n} operand {i}"));
                }
            } else if let Some(c) = Operation::get_op::<CallOp>(o, ctx) {
                let direct = c.get_attr_llvm_call_callee(ctx).is_some();
                if !direct && i == 0 {
                    return Err("slot is the callee".into());
                }
                let k = (i - usize::from(!direct)) as u32;
                match direct_callee(ctx, st, o) {
                    Some(c) if alloc_shim(c) => {}
                    Some(c) => calls.push((ids.id(c), k, j)),
                    None => {
                        let CallOpCallable::Indirect(cv) = c.callee(ctx) else { return Err("callee".into()) };
                        for c in indirect_callees(ctx, st, cv).ok_or("unresolved indirect call")? {
                            calls.push((ids.id(&c), k, j));
                        }
                    }
                }
            } else {
                return Err(format!("use by {} operand {i}", Operation::get_opid(o, ctx)));
            }
        }
    }
    Ok((loads, calls))
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
    // A constant function address: a vtable slot `constload2` folded after
    // the one `devirt` round (hashbrown's `&mut dyn FnMut` probe callback
    // reaches a constant only once `phisimp2` has run).
    // ... or a join of such constants (`crate::helpers::const_callees`).
    if let Some(cs) = crate::helpers::const_callees(ctx, st, c) {
        return Some(cs);
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
thread_local! {
    /// Which `run` this is in the current CGU (`NOCAPDBG` lines say).
    static RUN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static RUN_CGU: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

pub fn run(ctx: &Context, st: &mut State) {
    run_with(ctx, st, crate::env_is("PLIRON_NOCAP_DEEP", "1"), crate::env_is("PLIRON_NOCAPSUM", "1"))
}

/// `run` with the knobs given: `deep` (`PLIRON_NOCAP_DEEP`) and `capsum_on`
/// (`PLIRON_NOCAPSUM`).
pub fn run_with(ctx: &Context, st: &mut State, deep: bool, capsum_on: bool) {
    RUN_CGU.with(|c| {
        let mut c = c.borrow_mut();
        if *c != st.cgu {
            *c = st.cgu.clone();
            RUN.with(|r| r.set(0));
        }
    });
    RUN.with(|r| r.set(r.get() + 1));
    let bodied = bodied(ctx, st);
    let mut ids = Syms::default();
    let mut p = Capture::default();
    let dbg = crate::env_get("PLIRON_NOCAPDBG");
    let fields = crate::env_get("PLIRON_NOCAP_FIELDS") == Some("1");
    // `PLIRON_NOCAP_DEEP=1`: the local verdicts come from `CapNode` (depth-1
    // facts, slots passed to calls); `PLIRON_NOCAPSUM=1`: summaries for the
    // crate-wide solve, from the same program. The inputs only `CapNode`
    // reads are kept apart so the default `Capture` run sees what it always
    // did.
    let node_on = deep || capsum_on;
    let mut node_retain: Vec<(u32, Value)> = Vec::new();
    let mut node_flow: Vec<(Value, Value)> = Vec::new();
    let mut old_only_retain: Vec<(u32, Value)> = Vec::new();
    let mut ld: Vec<(Value, Value)> = Vec::new();
    let mut memsrc: Vec<(u32, Value)> = Vec::new();
    let mut slotarg: Vec<(u32, Value, u32, u32, u32)> = Vec::new();
    // Store -> load flows through frame slots: the default `Capture` keeps
    // its slot-wide form (every load of a slot gets every stored value);
    // `CapNode` gets them by byte range (`slot_flows`). Neither goes into
    // `p.flow` until each program's inputs are assembled.
    let mut old_slot_flow: Vec<(Value, Value)> = Vec::new();
    let mut via_slot_node: FxHashSet<Value> = FxHashSet::default();
    let mut load_ranges: FxHashMap<Value, Option<(i64, i64)>> = FxHashMap::default();
    let mut dbgrets: Vec<(String, Value, String)> = Vec::new();
    let mut dbgargs: Vec<(String, Value, String, u32, u32)> = Vec::new();
    // `slot_loads`'s nested `accesses` walks — one memo for the whole
    // pass: the analysis never mutates the function, and the same temp
    // slot is typically the destination of many stores.
    let mut accmemo = crate::sroa::AccMemo::default();
    // `classify` on a value's type, memoized — the flow loop otherwise
    // re-walks the type per (result × operand) pair.
    let mut kinds: FxHashMap<Value, (bool, bool)> = FxHashMap::default();
    // Slot pointers already linked to their loads (see the store case).
    let mut via_slot: FxHashSet<Value> = FxHashSet::default();
    // Verdicts adopted from other CGUs (`State::facts_adopt`, bodiless
    // decls only): a call into them keeps exactly what that copy keeps.
    // Without this seed every cross-CGU callee counted as retaining all
    // its args, so `Vec::push`'s `grow_one` (one copy per crate, in some
    // other CGU) captured the vector in every function that pushed:
    // regex's `follow_epsilons` lost `nocapture` on its sparse set, and
    // `next_state` then reloaded the set's fields per decoded varint.
    let bodied_set: FxHashSet<&str> = bodied.iter().map(String::as_str).collect();
    let mut imported: Vec<(String, FxHashSet<u32>)> = st
        .funcs
        .iter()
        .filter(|(s, f)| !bodied_set.contains(s.as_str()) && !crate::has_body(ctx, f.op))
        .filter_map(|(s, _)| st.nocapture.get(s).map(|v| (s.clone(), v.clone())))
        .collect();
    imported.sort_by(|a, b| a.0.cmp(&b.0));
    for (s, set) in &imported {
        let c = ids.id(s);
        p.nparams.push((c, MAX_PARAMS));
        p.captured.extend((0..MAX_PARAMS).filter(|i| !set.contains(i)).map(|i| (c, i)));
    }
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
                if node_on && (is!(LoadOp) || is!(AtomicLoadOp)) {
                    ld.push((d.get_operand(ctx, 0), d.get_result(ctx, 0)));
                }
                // Comparisons encode nothing, and a load yields the
                // pointee, not the pointer: memory only holds a derived
                // pointer if a store put it there, which is a capture
                // already (or the caller's, as in LLVM's CaptureTracking).
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
                // Does a mask flow from `v` to `r`? Scalars: no ptr <-> int
                // flow but through a bridge (checked at the use).
                let mut flows = |v: Value, r: Value| -> bool {
                    let (vp, vi) = pk(v);
                    let (rp, ri) = pk(r);
                    if fields { vp && rp || vi && ri } else { !(vp && ri) && !(vi && rp) }
                };
                if !(is!(ICmpOp) || is!(FCmpOp) || is!(LoadOp) || is!(AtomicLoadOp)) {
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
                    let fresh = is!(CallOp) && direct_callee(ctx, st, op).is_some_and(|c| alloc_shim(c));
                    for r in d.results(ctx) {
                        let srcs: Vec<Value> = match field {
                            _ if fresh => Vec::new(),
                            Some(v) => vec![v],
                            None => d.operands(ctx).collect(),
                        };
                        for v in srcs {
                            // `PLIRON_NOCAP_FIELDS`: by what each type's
                            // leaves can hold, so an all-int struct a call
                            // returns can't carry its ptr operand.
                            if bridge || flows(v, r) {
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
                    if node_on {
                        node_retain.push((f, v));
                    }
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
                    // Every load of the slot inherits the stored value's
                    // mask, whatever its type: memory is an int <-> ptr
                    // bridge (`int_copy_through_slot_captures`).
                    match crate::sroa::slot_loads(ctx, st, dp, 0, &mut accmemo) {
                        Some(ls) => {
                            if node_on {
                                slot_flows(ctx, st, sv, dp, &ls, &mut via_slot_node, &mut load_ranges, &mut node_flow);
                            }
                            if ls.len() > SLOT_FANOUT {
                                old_slot_flow.push((sv, dp));
                                if via_slot.insert(dp) {
                                    old_slot_flow.extend(ls.into_iter().map(|r| (dp, r)));
                                }
                            } else {
                                old_slot_flow.extend(ls.into_iter().map(|r| (sv, r)));
                            }
                        }
                        None => match node_on.then(|| slot_calls(ctx, st, dp, &mut ids)).flatten() {
                            // `CapNode` only: the pointer sits at depth 1 of a
                            // slot that only loads and calls see.
                            Some((ls, calls)) => {
                                old_only_retain.push((f, sv));
                                slot_flows(ctx, st, sv, dp, &ls, &mut via_slot_node, &mut load_ranges, &mut node_flow);
                                slotarg.extend(calls.into_iter().map(|(c, k, j)| (f, sv, c, k, j)));
                            }
                            None => retain(sv),
                        },
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
                        // of a realloc/dealloc is dead once they return. (Was
                        // `PLIRON_NOCAP_FIELDS` only: every `Vec::push` then
                        // captured its vector through `grow_one` ->
                        // `finish_grow` -> `__rust_realloc`, and regex's
                        // `next_state` reloaded its sparse set's fields per
                        // element because `qnext` never became `nocapture`.)
                        Some(c) if alloc_shim(c) => {}
                        Some(c) => {
                            let c = ids.id(c);
                            for (k, a) in args.into_iter().enumerate() {
                                p.arg.push((f, c, k as u32, a));
                                if dbg.is_some_and(|d| s.contains(d)) {
                                    dbgargs.push((s.clone(), a, OpDbg { op, ctx }.to_string(), c, k as u32));
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
                        Some("llvm.memcpy") | Some("llvm.memmove") | Some("llvm.memset") => {
                            // `CapNode`: copying a param's memory elsewhere
                            // carries its depth-1 pointers along.
                            if node_on && st.intrinsics.get(op).map(|s| s.as_str()) != Some("llvm.memset") {
                                memsrc.push((f, d.get_operand(ctx, 1)));
                            }
                        }
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
    p.retain.extend(old_only_retain);
    // `CapNode` runs: one local (`PLIRON_NOCAP_DEEP`), one summary
    // (`PLIRON_NOCAPSUM`), over the shared inputs plus the node-only ones.
    let node_run = |summary: bool, ids: &mut Syms| -> CapNode {
        let mut q = CapNode::default();
        q.param = p.param.clone();
        q.flow = p.flow.iter().chain(&node_flow).copied().collect();
        q.ld = ld.clone();
        q.retain = node_retain.clone();
        q.memsrc = memsrc.clone();
        q.arg = p.arg.clone();
        q.slotarg = slotarg.clone();
        q.nparams = p.nparams.clone();
        let known: FxHashSet<u32> = q.nparams.iter().map(|&(c, _)| c).collect();
        let mut opaque: FxHashSet<u32> = FxHashSet::default();
        opaque.extend(q.arg.iter().map(|&(_, c, _, _)| c).filter(|c| !known.contains(c)));
        opaque.extend(q.slotarg.iter().map(|&(_, _, c, _, _)| c).filter(|c| !known.contains(c)));
        q.opaque = opaque.into_iter().map(|c| (c,)).collect();
        // adopted callees: depth 0 from `nocapture`, deeper from `freenodes`
        // (none known: depth 1 captures, and with it depth 2)
        for (sym, set) in &imported {
            let c = ids.id(sym);
            let free = st.freenodes.get(sym);
            for k in 0..MAX_PARAMS {
                if !set.contains(&k) {
                    q.capin.push((c, DEPTHS * k));
                }
                for d in 1..DEPTHS {
                    if !free.is_some_and(|fr| fr.contains(&(DEPTHS * k + d))) {
                        q.capin.push((c, DEPTHS * k + d));
                    }
                }
            }
        }
        if summary {
            q.summary.push((0,));
        } else {
            q.local.push((0,));
        }
        q.run();
        // `PLIRON_NOCAPDBG=<fn>`: why each captured node of a matching fn is
        if let Some(want) = dbg
            && pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::nocapture", tracing::Level::DEBUG)
        {
            let names = ids.names();
            let der: FxHashMap<Value, Mask> = q.der.iter().map(|(v, m)| (*v, *m)).collect();
            let ddm: FxHashMap<Value, Mask> = q.dd.iter().map(|(v, m)| (*v, *m)).collect();
            let cap: FxHashSet<(u32, u32)> = q.cap.iter().copied().collect();
            let opq: FxHashSet<u32> = q.opaque.iter().map(|&(c,)| c).collect();
            let mode = if summary { "sum" } else { "loc" };
            for &(f, x) in &q.cap {
                let fname = &names[f as usize];
                if !fname.contains(want) || x >= 2 * DEPTHS {
                    continue;
                }
                let (i, depth) = (x / DEPTHS, x % DEPTHS);
                let mut why: Vec<String> = Vec::new();
                for &(g, v) in &q.retain {
                    let m = match depth { 0 => der.get(&v), 1 => ddm.get(&v), _ => None };
                    if g == f && m.is_some_and(|m| m.0 & (1 << i) != 0) {
                        let at = dbgrets.iter().find(|(_, w, _)| *w == v).map(|(_, _, o)| o.as_str()).unwrap_or("?");
                        // two levels of incoming flow, the ones carrying bit i
                        let carrying = |w: Value| -> Vec<Value> {
                            q.flow.iter().filter(|&&(a, b)| b == w && der.get(&a).is_some_and(|m| m.0 & (1 << i) != 0)).map(|&(a, _)| a).take(3).collect()
                        };
                        let up: Vec<(Value, Vec<Value>)> = carrying(v).into_iter().map(|a| (a, carrying(a))).collect();
                        why.push(format!("retain at {} from {up:?}", &at[..at.len().min(100)]));
                    }
                }
                for &(g, v) in &q.memsrc {
                    if depth == 1 && g == f && der.get(&v).is_some_and(|m| m.0 & (1 << i) != 0) {
                        why.push(format!("memcpy from {v:?}"));
                    }
                }
                for &(g, xx, c, z) in &q.pass {
                    if g == f && xx == x && (cap.contains(&(c, z)) || (opq.contains(&c) && !summary)) {
                        why.push(format!("pass to {} node {z}{}", &names[c as usize][..names[c as usize].len().min(110)], if opq.contains(&c) { " (opaque)" } else { "" }));
                    }
                }
                why.truncate(6);
                tracing::debug!(target: "pliron::nocapture", "NODEDBG {mode} run{} {fname}: node {x} captured: {why:?}", RUN.with(|r| r.get()));
            }
        }
        q
    };
    if capsum_on {
        let q = node_run(true, &mut ids);
        let names = ids.names();
        let cap: FxHashSet<(u32, u32)> = q.cap.iter().copied().collect();
        let mut deps: FxHashMap<(u32, u32), Vec<(String, String, u32)>> = FxHashMap::default();
        for &(f, x, c, z) in &q.dep {
            let cs = &names[c as usize];
            let cty = st.funcs.get(cs).map(|cf| cf.ty.disp(ctx).to_string()).unwrap_or_default();
            deps.entry((f, x)).or_default().push((cs.clone(), cty, z));
        }
        let nparams: FxHashMap<u32, u32> = q.nparams.iter().copied().collect();
        st.capsum = Default::default();
        for s in &bodied {
            let f = ids.id(s);
            let n = nparams.get(&f).copied().unwrap_or(0);
            let v = (0..DEPTHS * n)
                .map(|x| {
                    if cap.contains(&(f, x)) {
                        CapSum::Captured
                    } else {
                        let mut d = deps.remove(&(f, x)).unwrap_or_default();
                        d.sort();
                        d.dedup();
                        CapSum::Deps(d)
                    }
                })
                .collect();
            st.capsum.insert(s.clone(), v);
        }
    }
    let local_nodes = deep.then(|| node_run(false, &mut ids));
    p.flow.extend(old_slot_flow);
    let trun = std::time::Instant::now();
    if local_nodes.is_none() {
        p.run();
    }
    pliron_cfg::trace("nocap-fixpoint", trun.elapsed().as_secs_f64() * 1e3, &st.cgu);
    if dbg.is_some()
        && local_nodes.is_none()
        && pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::nocapture", tracing::Level::DEBUG)
    {
        let der: FxHashMap<Value, Mask> = p.der.iter().map(|(v, m)| (*v, *m)).collect();
        for (s, v, o) in &dbgrets {
            for i in bits(*der.get(v).unwrap_or(&Mask(0))) {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::nocapture", "NOCAPDBG run{} {s}: param{i} retained via {v:?} at {o}", RUN.with(|r| r.get()))
                };
            }
        }
        let nparams: FxHashMap<u32, u32> = p.nparams.iter().copied().collect();
        let keeps: FxHashSet<(u32, u32)> = p.keeps.iter().copied().collect();
        for (s, v, o, c, k) in &dbgargs {
            for i in bits(*der.get(v).unwrap_or(&Mask(0))) {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::nocapture", "NOCAPDBG run{} {s}: param{i} via call-arg {v:?} (callee known={} keeps={}) at {o}", RUN.with(|r| r.get()), nparams.contains_key(c), keeps.contains(&(*c, *k)))
                };
            }
        }
    }
    let mut nc: FxHashMap<String, FxHashSet<u32>> = FxHashMap::default();
    // `nparams` is a relation Vec — linear `find` per fn is O(fns²).
    let nparams: FxHashMap<u32, u32> = p.nparams.iter().copied().collect();
    if let Some(q) = &local_nodes {
        let cap: FxHashSet<(u32, u32)> = q.cap.iter().copied().collect();
        let mut dn: Vec<(String, FxHashSet<u32>)> = Vec::new();
        for s in &bodied {
            let f = ids.id(s);
            let n = nparams.get(&f).copied().unwrap_or(0);
            nc.insert(s.clone(), (0..n).filter(|&i| !cap.contains(&(f, DEPTHS * i))).collect());
            dn.push((s.clone(), (0..DEPTHS * n).filter(|&x| !cap.contains(&(f, x))).collect()));
        }
        // bodiless decls keep their adopted node sets
        let mut keep: Vec<(String, FxHashSet<u32>)> = imported
            .iter()
            .filter_map(|(sym, _)| st.freenodes.get(sym).map(|d| (sym.clone(), d.clone())))
            .collect();
        keep.extend(dn);
        st.freenodes = keep.into_iter().collect();
    } else {
        let cap: FxHashSet<(u32, u32)> = p.captured.iter().copied().collect();
        for s in &bodied {
            let f = ids.id(s);
            let n = nparams.get(&f).copied().unwrap_or(0);
            nc.insert(
                s.clone(),
                (0..n).filter(|&i| !cap.contains(&(f, i))).collect(),
            );
        }
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
    // Adopted verdicts for bodiless decls stay (lowering's call-arg
    // `nocap` sets read them for every callee).
    nc.extend(imported);
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
        analyze_f_with(input, nparams, false)
    }

    fn analyze_f_with(input: &str, nparams: usize, deep: bool) -> FxHashSet<u32> {
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
                    ret_nonnull: 0,
                    nparams,
                    tail_cc: false,
                },
            );
        }
        run_with(ctx, &mut st, deep, false);
        st.nocapture.get("f").expect("f analyzed").clone()
    }

    /// `PLIRON_NOCAP_DEEP`: `p` stored into a slot that only `@reads` sees,
    /// which loads it back and reads through it (hashbrown's scope guards
    /// and closure environments). Depth 1 of the slot is not retained, so
    /// `p` is not either; the default analysis calls the store a capture.
    #[test]
    fn deep_slot_to_a_reader_does_not_capture() {
        let input = r#"
        builtin.module @m {
        ^b():
          llvm.func @reads: llvm.func <builtin.integer i64(llvm.ptr (0)) variadic = false> [] {
          ^e(q: llvm.ptr (0)):
            r = llvm.load q : llvm.ptr (0);
            x = llvm.load r : builtin.integer i64;
            llvm.return x
          };
          llvm.func @f: llvm.func <builtin.integer i64(llvm.ptr (0)) variadic = false> [] {
          ^e(p: llvm.ptr (0)):
            one = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            s = llvm.alloca [llvm.ptr (0) x one] : llvm.ptr (0);
            llvm.store *s <- p;
            v = llvm.call @reads (s) : llvm.func <builtin.integer i64(llvm.ptr (0)) variadic = false>;
            llvm.return v
          }
        }"#;
        assert!(analyze_f_with(input, 1, true).contains(&0), "deep");
        assert!(!analyze_f_with(input, 1, false).contains(&0), "default");
    }

    /// `p` two levels down: stored into slot `a`, whose address goes into
    /// slot `b`, which `@probe` gets and reads through both (hashbrown's
    /// `find` probe closure holding `&mut equivalent_key(k)`). Depth 2 of
    /// `b` is not retained, so `p` is not. A version whose callee keeps the
    /// depth-2 pointer captures.
    #[test]
    fn deep_nested_slots() {
        let input = |keep: bool| format!(r#"
        builtin.module @m {{
        ^b():
          llvm.func @probe: llvm.func <builtin.integer i64(llvm.ptr (0), llvm.ptr (0)) variadic = false> [] {{
          ^e(q: llvm.ptr (0), out: llvm.ptr (0)):
            a = llvm.load q : llvm.ptr (0);
            k = llvm.load a : llvm.ptr (0);
            {}
            x = llvm.load k : builtin.integer i64;
            llvm.return x
          }};
          llvm.func @f: llvm.func <builtin.integer i64(llvm.ptr (0), llvm.ptr (0)) variadic = false> [] {{
          ^e(p: llvm.ptr (0), out: llvm.ptr (0)):
            one = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            sa = llvm.alloca [llvm.ptr (0) x one] : llvm.ptr (0);
            sb = llvm.alloca [llvm.ptr (0) x one] : llvm.ptr (0);
            llvm.store *sa <- p;
            llvm.store *sb <- sa;
            v = llvm.call @probe (sb, out) : llvm.func <builtin.integer i64(llvm.ptr (0), llvm.ptr (0)) variadic = false>;
            llvm.return v
          }}
        }}"#, if keep {{ "llvm.store *out <- k;" }} else {{ "" }});
        assert!(analyze_f_with(&input(false), 2, true).contains(&0), "reader");
        assert!(!analyze_f_with(&input(true), 2, true).contains(&0), "keeper");
        assert!(!analyze_f_with(&input(false), 2, false).contains(&0), "default");
    }

    /// The callee keeps what it loads out of the slot (stores it through
    /// another param): `p` is captured, deep or not.
    #[test]
    fn deep_slot_to_a_keeper_captures() {
        let input = r#"
        builtin.module @m {
        ^b():
          llvm.func @keeps: llvm.func <llvm.void(llvm.ptr (0), llvm.ptr (0)) variadic = false> [] {
          ^e(q: llvm.ptr (0), out: llvm.ptr (0)):
            r = llvm.load q : llvm.ptr (0);
            llvm.store *out <- r;
            llvm.return
          };
          llvm.func @f: llvm.func <llvm.void(llvm.ptr (0), llvm.ptr (0)) variadic = false> [] {
          ^e(p: llvm.ptr (0), out: llvm.ptr (0)):
            one = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            s = llvm.alloca [llvm.ptr (0) x one] : llvm.ptr (0);
            llvm.store *s <- p;
            c = llvm.call @keeps (s, out) : llvm.func <llvm.void(llvm.ptr (0), llvm.ptr (0)) variadic = false>;
            llvm.return
          }
        }"#;
        assert!(!analyze_f_with(input, 2, true).contains(&0));
    }

    /// The slot goes to a function with no body: depth 1 is unknown.
    #[test]
    fn deep_slot_to_an_unknown_callee_captures() {
        let input = r#"
        builtin.module @m {
        ^b():
          llvm.func @f: llvm.func <llvm.void(llvm.ptr (0)) variadic = false> [] {
          ^e(p: llvm.ptr (0)):
            one = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            s = llvm.alloca [llvm.ptr (0) x one] : llvm.ptr (0);
            llvm.store *s <- p;
            c = llvm.call @ext (s) : llvm.func <llvm.void(llvm.ptr (0)) variadic = false>;
            llvm.return
          };
          llvm.func @ext: llvm.func <llvm.void(llvm.ptr (0)) variadic = false> []
        }"#;
        assert!(!analyze_f_with(input, 1, true).contains(&0));
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

#[cfg(test)]
mod capsum_tests {
    use super::*;

    fn key(s: &str) -> (String, String) {
        (s.to_string(), "fn".to_string())
    }
    fn dep(s: &str, k: u32) -> (String, String, u32) {
        (s.to_string(), "fn".to_string(), k)
    }

    /// `a` -> `b` -> `c` across three CGUs, each hop through a callee with
    /// no body where the summary was made: the chain resolves only when
    /// every link is known not to capture.
    #[test]
    fn chain_resolves_through_every_hop() {
        let mut sums = FxHashMap::default();
        sums.insert(key("a"), vec![CapSum::Deps(vec![dep("b", 0)])]);
        sums.insert(key("b"), vec![CapSum::Deps(vec![dep("c", 1)])]);
        sums.insert(key("c"), vec![CapSum::Captured, CapSum::Deps(vec![])]);
        let s = solve_capsum(&sums);
        assert!(s[&key("a")].contains(&0));
        assert!(s[&key("b")].contains(&0));
        assert!(!s[&key("c")].contains(&0) && s[&key("c")].contains(&1));
        // the last link captures: everything up the chain does
        sums.insert(key("c"), vec![CapSum::Captured, CapSum::Captured]);
        let s = solve_capsum(&sums);
        assert!(s[&key("a")].is_empty() && s[&key("b")].is_empty());
    }

    /// An edge to a symbol no CGU summarized (an import), or past its
    /// params, captures; a cycle with no capture in it does not.
    #[test]
    fn unknown_edges_capture_and_cycles_do_not() {
        let mut sums = FxHashMap::default();
        sums.insert(key("x"), vec![CapSum::Deps(vec![dep("ext", 0)])]);
        sums.insert(key("y"), vec![CapSum::Deps(vec![dep("z", 3)])]);
        sums.insert(key("z"), vec![CapSum::Deps(vec![dep("w", 0)])]);
        sums.insert(key("w"), vec![CapSum::Deps(vec![dep("z", 0)])]);
        let s = solve_capsum(&sums);
        assert!(s[&key("x")].is_empty(), "import");
        assert!(s[&key("y")].is_empty(), "past the params");
        assert!(s[&key("z")].contains(&0) && s[&key("w")].contains(&0), "cycle");
    }

    /// Copies merge param by param: captured in one copy is captured, a
    /// param one copy lacks is captured, edges union.
    #[test]
    fn merge_is_conservative() {
        let mut a = vec![CapSum::Deps(vec![dep("p", 0)]), CapSum::Deps(vec![])];
        CapSum::merge(&mut a, &[CapSum::Deps(vec![dep("q", 0)])]);
        assert_eq!(a[0], CapSum::Deps(vec![dep("p", 0), dep("q", 0)]));
        assert_eq!(a[1], CapSum::Captured);
        let mut b = vec![CapSum::Deps(vec![])];
        CapSum::merge(&mut b, &[CapSum::Captured]);
        assert_eq!(b[0], CapSum::Captured);
    }
}

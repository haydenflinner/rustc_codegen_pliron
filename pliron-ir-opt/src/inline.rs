//! Pliron-level inliner. Cranelift does no inlining, so small callees defined
//! in the same CGU are copied into their call sites before lowering. Callees are
//! processed bottom-up, so inlined bodies are inlined transitively; at invoke sites
//! the callee's calls become invokes to the same landing pad.
//! `PLIRON_INLINE=<max ops>` tunes the size limit; `0` disables it.
//!
//! `PLIRON_INLINE_MODEL` (default on; `0` = the old fixed thresholds) weighs
//! per-callsite cost vs. savings: see [`run`]. `PLIRON_INLINE_VERBOSE=1` logs
//! every callsite verdict with its score under either model.

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::cols::{ColMap, ColSet};
use pliron::builtin::op_interfaces::{BranchOpInterface, CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::irbuild::cloning::{IrMapping, clone_blocks_into};
use pliron::irbuild::inserter::OpInsertionPoint;
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::{IRRewriter, Rewriter};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::{Op, op_cast};
use pliron::operation::OpDbg;
use pliron::operation::Operation;
use pliron::r#type::{Typed, TypedHandle};
use pliron::value::Value;
use pliron_llvm::ops::{
    AddrSpaceCastOp, AllocaOp, AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp,
    BitcastOp, BrOp, CallOp, CondBrOp, ExtractValueOp, FreezeOp, FuncOp, GetElementPtrOp,
    IndirectBrOp, InsertValueOp, LoadOp, PoisonOp, ReturnOp, StoreOp, SwitchOp, UndefOp,
    UnreachableOp,
};
use pliron_llvm::types::FuncType;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::has_body;
use crate::state::{ConstVal, State, leaf_arg};
use crate::types::{leaf_range, size_align};

const DEFAULT_LIMIT: usize = 320;
pub const SMALL_LIMIT: usize = 40;
/// The `small` limit for wasm: still the flat no-bonus rules, but the
/// 40-op cap left iterator/UTF-8 helpers out of line in every hot loop —
/// +67% geomean on `bench/wasm` vs LLVM for ~6% less code. 512 is the
/// measured sweet spot: `hashbrown`'s `hash_one` (SipHash + probe glue)
/// sits just over `DEFAULT_LIMIT`, and inlining it drops hashmap from
/// +81% to +16% and hmget from +163% to +43% (geomean 1.08); 768 buys
/// nothing further and lets `tokenize` slip back to parity.
pub const WASM_LIMIT: usize = 512;
/// wasm loop-callsite bonus (x the flat limit): a call every iteration
/// pays the call+prologue cost every iteration, so a fatter callee is
/// worth it there — hashbrown's `hash_one` (SipHash, ~926 ops in cgu.0)
/// sits just over `WASM_LIMIT` but inlining it into `get`'s probe loop
/// is where most of the `bench/wasm` hashmap gap lives. Kept off for
/// -Os/-Oz, where growth is the point of `small`.
pub const WASM_HOT: usize = 3;

// Cost-model weights (`PLIRON_INLINE_MODEL`): a callsite is inlined when
// `cost <= threshold`.
/// Each call inside the callee costs this much on top of its own op: inlining
/// moves the call into the caller, it doesn't remove it. LLVM charges a call
/// roughly 5-10 instructions' worth; at 3, dispatchers like a 40-variant
/// enum's drop glue (126 ops, 40 calls) passed the base limit and every
/// `Option<Box<Pat>>` overwrite in syn pulled the whole tree in. 10 leaves
/// the bench kernels' hot code unchanged and cuts syn's CPU ~22%; 20 halves
/// it but reshapes `strwork::run` — unmeasured, see PARITY.md.
const CALL_OP_W: usize = 10;
/// Bonus per use a constant actual can fold (a scalar arg feeding arithmetic,
/// compares, selects, casts, ...).
const FOLD_USE: usize = 25;
/// Bonus per memory op a by-ref arg reaches through GEP/bitcast chains when
/// the actual is a local slot or a constant address: the inlined loads/stores
/// become direct (SROA/`loadfwd`/`constload` material).
const MEM_USE: usize = 20;
/// `#[cold]` callees pollute the caller's hot layout: cost doubles.
const COLD_W: usize = 2;
/// Threshold cap (x the base limit) so the bonus terms can't grow without
/// bound; a single callsite never budgets more than this.
const MAX_T: usize = 32;
/// Default per-caller budget (x the base limit) of non-free inlined ops
/// (`PLIRON_INLINE_GROWTH`).
const GROWTH_MULT: usize = 25;

pub fn blocks(ctx: &Context, f: Ptr<Operation>) -> Vec<Ptr<BasicBlock>> {
    // Bodiless `llvm.func` ops (extern decls like `rust_eh_personality`)
    // legitimately have no regions — callers iterating all funcs see an
    // empty body rather than a panic.
    let Some(r) = f.deref(ctx).regions(ctx).next() else {
        return Vec::new();
    };
    r.deref(ctx).iter(ctx).collect()
}

pub fn ops(ctx: &Context, b: Ptr<BasicBlock>) -> Vec<Ptr<Operation>> {
    b.deref(ctx).iter(ctx).collect()
}

/// Lazy `ops` — no `Vec` collect for read-only scans. The iterated
/// block's op list must not be mutated while iterating.
pub fn ops_iter<'a>(
    ctx: &'a Context,
    b: Ptr<BasicBlock>,
) -> impl Iterator<Item = Ptr<Operation>> + 'a {
    b.deref(ctx).iter(ctx)
}

/// Lazy `blocks` — no `Vec` collect for read-only scans. The function's
/// region-0 block list must not be mutated while iterating.
pub fn blocks_iter<'a>(
    ctx: &'a Context,
    f: Ptr<Operation>,
) -> impl Iterator<Item = Ptr<BasicBlock>> + 'a {
    let r = f.deref(ctx).regions(ctx).next();
    r.into_iter().flat_map(move |r| r.deref(ctx).iter(ctx))
}

/// `ops` into a caller-owned buffer so per-block walks allocate once.
pub fn ops_into(ctx: &Context, b: Ptr<BasicBlock>, out: &mut Vec<Ptr<Operation>>) {
    out.clear();
    out.extend(b.deref(ctx).iter(ctx));
}

pub fn direct_callee<'a>(ctx: &Context, st: &'a State, op: Ptr<Operation>) -> Option<&'a String> {
    // `st.calls` memoizes the resolved callee at scan time — one hash
    // probe instead of the attr-dict get + ident lookup every pass was
    // repeating per call site. The callee attr is constructor-only, so
    // the memo can't go stale; ops without an entry fall back below.
    if let Some(sym) = st.calls.get(op).and_then(|ci| ci.callee.as_ref()) {
        return Some(sym);
    }
    let c = Operation::get_op::<CallOp>(op, ctx)?;
    // Borrow the callee attr directly: `callee()` clones it into an owned
    // Identifier (one String alloc) and the obvious lookup then spends a
    // second on `to_string()` — per call op, per rescan round.
    if let Some(id) = c.get_attr_llvm_call_callee(ctx) {
        return st.ident_to_sym.get(AsRef::<str>::as_ref(id.as_ref()));
    }
    match c.callee(ctx) {
        CallOpCallable::Direct(_) => unreachable!("checked above"),
        // An indirect call through a provable function address (a monomorphized
        // closure shim like hashbrown's `make_hasher`) is direct for inlining:
        // `devirt` rewrites these later, but that runs after the main pass.
        CallOpCallable::Indirect(p) => match st.consts.get(p) {
            Some(ConstVal::Sym { sym, off: 0 }) => st.funcs.get_key_value(sym).map(|(k, _)| k),
            // `consts` is codegen-time; passes that rewrite the callee
            // operand leave the Sym const on an aliased-away value, so
            // fall through to the operand's definition before giving up.
            _ => indirect_callee(ctx, st, p),
        },
    }
}

/// Resolve an indirect-call operand to a known function through its
/// definition chain: `llvm.addressof` and aliases of `sym_addr` values.
fn indirect_callee<'a>(ctx: &Context, st: &'a State, mut p: Value) -> Option<&'a String> {
    for _ in 0..8 {
        if let Some(ConstVal::Sym { sym, off: 0 }) = st.consts.get(p) {
            return st.funcs.get_key_value(sym).map(|(k, _)| k);
        }
        let d = p.defining_op()?;
        if let Some(a) = Operation::get_op::<pliron_llvm::ops::AddressOfOp>(d, ctx) {
            let id = a.get_global_name(ctx);
            let sym = st.ident_to_sym.get(AsRef::<str>::as_ref(id.as_ref()));
            return sym.and_then(|s| st.funcs.get_key_value(s).map(|(k, _)| k));
        }
        // Transparent defs (casts/bitwise ops on the pointer) — keep walking.
        if Operation::is_op::<BitcastOp>(d, ctx) || Operation::is_op::<AddrSpaceCastOp>(d, ctx) {
            p = d.deref(ctx).get_operand(ctx, 0);
            continue;
        }
        return None;
    }
    None
}

/// Callee-body facts the callsite score is computed from.
struct CInfo {
    op: Ptr<Operation>,
    eh: bool,
    /// Op count.
    n: usize,
    /// Call ops inside the body (`CALL_OP_W` extra cost each under the model).
    ncalls: usize,
    /// Params feeding an indirect call's target (see `devirt_params`).
    dvirt: Vec<usize>,
    /// Params reaching a branch condition (see `cond_params`).
    conds: Vec<usize>,
    /// Per-param memory-op uses a direct actual makes resolvable.
    mem: Vec<u32>,
    /// Per-param other foldable uses.
    fold: Vec<u32>,
}

/// Legality plus the body facts the callsite score needs, from ONE walk of
/// the callee body (the legality count, the indirect-call targets, and the
/// branch-condition roots used to be three separate scans; the small
/// def-chain DFSs they feed stay per-feature). The fixed thresholds only
/// profile callees a devirt/fold multiplier could rescue (as the old `dv` map
/// did); the model profiles every plausibly inlinable body.
fn callee_info(
    ctx: &Context,
    st: &State,
    sym: &str,
    model: bool,
    limit: usize,
    bonus: usize,
    fold: usize,
) -> Option<CInfo> {
    let f = &st.funcs[sym];
    if f.no_inline || f.linkage == Linkage::Preemptible || !has_body(ctx, f.op) {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::inline", "callee {sym}: no_inline={} linkage={:?} has_body={}",
            f.no_inline, f.linkage, has_body(ctx, f.op))
        };
        return None;
    }
    let (mut n, mut ncalls, mut eh) = (0, 0, false);
    let mut dvirt_tgt = Vec::new();
    let mut cond_roots = Vec::new();
    for b in blocks_iter(ctx, f.op) {
        for op in b.deref(ctx).iter(ctx) {
            n += 1;
            let intr = st.intrinsics.get(op);
            if intr.is_some_and(|s| s.starts_with("llvm.va_") || s.starts_with("pliron.va."))
                || direct_callee(ctx, st, op).is_some_and(|c| c == sym)
            {
                return None;
            }
            eh |= st.invokes.contains(op) || intr.is_some_and(|s| s.starts_with("pliron.eh"));
            if let Some(c) = Operation::get_op::<CallOp>(op, ctx) {
                ncalls += 1;
                // Indirect target is operand 0; `callee()` would clone the
                // attr for a direct call only to discard it here.
                if c.get_attr_llvm_call_callee(ctx).is_none() {
                    dvirt_tgt.push(op.deref(ctx).get_operand(ctx, 0));
                }
            }
            if Operation::is_op::<CondBrOp>(op, ctx)
                || (model && Operation::is_op::<SwitchOp>(op, ctx))
            {
                cond_roots.push(op.deref(ctx).get_operand(ctx, 0));
            }
        }
    }
    if eh && !crate::pass_enabled("PLIRON_INLINE_EH") {
        return None;
    }
    let mut ci = CInfo {
        op: f.op,
        eh,
        n,
        ncalls,
        dvirt: Vec::new(),
        conds: Vec::new(),
        mem: Vec::new(),
        fold: Vec::new(),
    };
    let want = if model {
        n <= limit.saturating_mul(MAX_T)
    } else {
        n > limit && n <= limit.saturating_mul(bonus.max(fold))
    };
    if !want {
        return Some(ci);
    }
    let params: Vec<Value> = match blocks_iter(ctx, f.op).next() {
        Some(entry) => entry.deref(ctx).arguments(ctx).collect(),
        None => return Some(ci),
    };
    if model || bonus > 1 {
        ci.dvirt = devirt_params(ctx, &params, &dvirt_tgt);
    }
    if model || fold > 1 {
        ci.conds = cond_params(ctx, &params, &cond_roots);
    }
    if model {
        (ci.mem, ci.fold) = param_uses(ctx, st, f.op);
    }
    Some(ci)
}

// Side-table annotation bits for [`CalleeTags`]. A callee's annotations
// are discovered once (lazily, on first inline) and replayed for every
// later inline of that body — otherwise each clone pays ~15 `ColMap`
// probes per op plus two `Vec` collects per block all over again.
const OP_CALLS: u16 = 1 << 0;
const OP_INTRINSICS: u16 = 1 << 1;
const OP_RMW: u16 = 1 << 2;
const OP_EXPECT: u16 = 1 << 3;
const OP_PROF_EDGES: u16 = 1 << 4;
const OP_VOLATILE: u16 = 1 << 5;
const OP_NONNULL: u16 = 1 << 6;
const OP_INBOUNDS: u16 = 1 << 7;
const OP_BOOL01: u16 = 1 << 8;
const OP_FROZEN_LOADS: u16 = 1 << 9;
const OP_RETURN: u16 = 1 << 10;

const V_ALLOCAS: u8 = 1;
const V_PROMOTED: u8 = 1 << 1;
const V_CONSTS: u8 = 1 << 2;
const V_RFROZEN: u8 = 1 << 3;
const V_RNOALIAS: u8 = 1 << 4;
const V_RFROZEN_OP: u8 = 1 << 5;

const B_REGION: u8 = 1;
const B_PARENT: u8 = 1 << 1;
const B_BCOUNT: u8 = 1 << 2;

/// Which `State` side tables carry an entry for each block/op/value of a
/// callee body. Built by one body walk and cached per callee op; a body
/// that is itself rewritten as a caller drops its entry (see the
/// `tagcache.remove` after each caller's fixpoint).
#[derive(Default)]
struct CalleeTags {
    /// `blocks(ctx, callee)` — cached so each inline skips the collect.
    src: Vec<Ptr<BasicBlock>>,
    blocks: Vec<(Ptr<BasicBlock>, u8)>,
    ops: Vec<(Ptr<Operation>, u16)>,
    vals: Vec<(Value, u8)>,
    /// Min/max/count of value uids, op ords and block ords in the region-0
    /// walk — bounds for the dense `IrMapping` (out-of-range keys spill to
    /// hashing; counts bound the dense fill ratio).
    vb: (usize, usize, usize),
    ob: (usize, usize, usize),
    bb: (usize, usize, usize),
}

fn tag_value(tags: &mut CalleeTags, st: &State, v: Value) {
    let mut m = 0;
    if st.allocas.contains(v) {
        m |= V_ALLOCAS;
    }
    if st.promoted.contains(v) {
        m |= V_PROMOTED;
    }
    if st.consts.contains(v) {
        m |= V_CONSTS;
    }
    if st.region_frozen.contains(v) {
        m |= V_RFROZEN;
    }
    if st.region_noalias.contains(v) {
        m |= V_RNOALIAS;
    }
    if st.region_frozen_op.contains(v) {
        m |= V_RFROZEN_OP;
    }
    if m != 0 {
        tags.vals.push((v, m));
    }
}

fn callee_tags(ctx: &Context, st: &State, callee: Ptr<Operation>) -> CalleeTags {
    let src = blocks(ctx, callee);
    let mut tags = CalleeTags::default();
    let (mut vlo, mut vhi, mut nv) = (usize::MAX, 0usize, 0usize);
    let (mut olo, mut ohi, mut no) = (usize::MAX, 0usize, 0usize);
    let (mut blo, mut bhi) = (usize::MAX, 0usize);
    for &b in &src {
        blo = blo.min(b.ord());
        bhi = bhi.max(b.ord());
        let mut bm = 0;
        if st.region_blocks.contains(b) {
            bm |= B_REGION;
        }
        if st.region_parent.contains(b) {
            bm |= B_PARENT;
        }
        if st.prof_bcount.contains(b) {
            bm |= B_BCOUNT;
        }
        if bm != 0 {
            tags.blocks.push((b, bm));
        }
        for a in b.deref(ctx).arguments(ctx) {
            let u = a.uid() as usize;
            vlo = vlo.min(u);
            vhi = vhi.max(u);
            nv += 1;
            tag_value(&mut tags, st, a);
        }
        for op in b.deref(ctx).iter(ctx) {
            let o = op.ord();
            olo = olo.min(o);
            ohi = ohi.max(o);
            no += 1;
            let mut m = 0;
            if st.calls.contains(op) {
                m |= OP_CALLS;
            }
            if st.intrinsics.contains(op) {
                m |= OP_INTRINSICS;
            }
            if st.rmw.contains(op) {
                m |= OP_RMW;
            }
            if st.expect.contains(op) {
                m |= OP_EXPECT;
            }
            if st.prof_edges.contains(op) {
                m |= OP_PROF_EDGES;
            }
            if st.volatile.contains(op) {
                m |= OP_VOLATILE;
            }
            if st.nonnull.contains(op) {
                m |= OP_NONNULL;
            }
            if st.inbounds.contains(op) {
                m |= OP_INBOUNDS;
            }
            if st.bool01.contains(op) {
                m |= OP_BOOL01;
            }
            if st.frozen_loads.contains(op) {
                m |= OP_FROZEN_LOADS;
            }
            if Operation::is_op::<ReturnOp>(op, ctx) {
                m |= OP_RETURN;
            }
            if m != 0 {
                tags.ops.push((op, m));
            }
            for v in op.deref(ctx).results(ctx) {
                let u = v.uid() as usize;
                vlo = vlo.min(u);
                vhi = vhi.max(u);
                nv += 1;
                tag_value(&mut tags, st, v);
            }
        }
    }
    // usize::MAX lo means the callee had no entity of that kind — the
    // range then wraps to nothing and every key hashes.
    tags.vb = (vlo.min(vhi), vhi, nv);
    tags.ob = (olo.min(ohi), ohi, no);
    tags.bb = (blo.min(bhi), bhi, src.len());
    tags.src = src;
    tags
}

/// Per entry-param use shape: `mem` = loads/stores/mem intrinsics reached
/// through GEP/bitcast chains (a by-ref arg whose actual is a local slot or a
/// constant becomes direct access the cleanup passes can resolve); `fold` =
/// other uses a constant actual can fold. Call/branch/return uses earn
/// neither — calls are priced by `ncalls`, block args don't fold in place.
fn param_uses(ctx: &Context, st: &State, f: Ptr<Operation>) -> (Vec<u32>, Vec<u32>) {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else {
        return (Vec::new(), Vec::new());
    };
    let nparams = entry.deref(ctx).arguments(ctx).count();
    let (mut mem, mut fold) = (vec![0u32; nparams], vec![0u32; nparams]);
    for (i, p) in entry.deref(ctx).arguments(ctx).enumerate() {
        let (mut work, mut seen, mut budget) = (vec![p], FxHashSet::default(), 64);
        while let Some(v) = work.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            for u in v.uses_iter(ctx) {
                let o = u.user_op();
                let ui = u.find_index(ctx);
                if (Operation::is_op::<GetElementPtrOp>(o, ctx)
                    || Operation::is_op::<BitcastOp>(o, ctx))
                    && ui == 0
                {
                    if seen.insert(o.deref(ctx).get_result(ctx, 0)) {
                        work.push(o.deref(ctx).get_result(ctx, 0));
                    }
                } else if Operation::is_op::<LoadOp>(o, ctx)
                    || Operation::is_op::<AtomicLoadOp>(o, ctx)
                    || ((Operation::is_op::<StoreOp>(o, ctx)
                        || Operation::is_op::<AtomicStoreOp>(o, ctx))
                        && ui != 0)
                    || st
                        .intrinsics.get(o)
                        .is_some_and(|s| s.starts_with("llvm.mem") && ui <= 1)
                {
                    mem[i] += 1;
                } else if !(Operation::is_op::<CallOp>(o, ctx)
                    || Operation::is_op::<BrOp>(o, ctx)
                    || Operation::is_op::<CondBrOp>(o, ctx)
                    || Operation::is_op::<SwitchOp>(o, ctx)
                    || Operation::is_op::<IndirectBrOp>(o, ctx)
                    || Operation::is_op::<ReturnOp>(o, ctx)
                    || Operation::is_op::<UnreachableOp>(o, ctx))
                {
                    fold[i] += 1;
                }
            }
        }
    }
    (mem, fold)
}

/// Whether `v` is a scalar constant the model credits: literal bits, zero, or
/// a symbol address (vtables and fn pointers fold indirect calls; global
/// addresses make by-ref memory direct).
fn const_arg(st: &State, v: &Value) -> bool {
    matches!(
        st.consts.get(*v),
        Some(ConstVal::Bits(_) | ConstVal::Zero | ConstVal::Sym { .. })
    )
}

/// Successor blocks, counting invoke landing-pad edges.
fn bsuccs(ctx: &Context, st: &State, b: Ptr<BasicBlock>) -> Vec<Ptr<BasicBlock>> {
    let mut v = Vec::new();
    // Invokes are non-terminator CallOps — they need the whole block scan;
    // successor lists live only on the terminator (`iter().last()` is O(1)).
    for op in b.deref(ctx).iter(ctx) {
        v.extend(st.invokes.get(op).map(|&(l, _)| l));
    }
    if let Some(t) = b.deref(ctx).iter(ctx).last() {
        v.extend(t.deref(ctx).successors(ctx));
    }
    v
}

/// Blocks of `f` that can reach themselves: the callsite-context "in a loop"
/// signal (an inlined body there runs once per iteration, so a little extra
/// code is worth it). One Tarjan pass — a block loops iff its SCC has more
/// than one member or it has a self-edge; the quadratic per-block DFS this
/// replaces took ~100ms per rebuild on a post-inline `main`.
fn loop_blocks(ctx: &Context, st: &State, f: Ptr<Operation>) -> FxHashSet<Ptr<BasicBlock>> {
    // One ptr→local-ordinal map, then the whole Tarjan state is dense
    // Vecs — five hash structures shrank to one.
    let mut bs = blocks(ctx, f);
    let n = bs.len();
    let mut ord: FxHashMap<Ptr<BasicBlock>, usize> =
        bs.iter().enumerate().map(|(i, &b)| (b, i)).collect();
    let mut index = vec![u32::MAX; n];
    let mut low = vec![0u32; n];
    let mut onstack = vec![false; n];
    let mut selfedge = vec![false; n];
    let mut tstack: Vec<usize> = Vec::new();
    let mut out = FxHashSet::default();
    let mut next = 0u32;
    for i in 0..n {
        let ro = i;
        if index[ro] != u32::MAX {
            continue;
        }
        let mut work: Vec<(usize, Vec<Ptr<BasicBlock>>)> = Vec::new();
        index[ro] = next;
        low[ro] = next;
        next += 1;
        tstack.push(ro);
        onstack[ro] = true;
        work.push((ro, bsuccs(ctx, st, bs[ro])));
        while let Some((v, succs)) = work.last_mut() {
            let v = *v;
            let Some(w) = succs.pop() else {
                work.pop();
                if low[v] == index[v] {
                    let mut scc = Vec::new();
                    while let Some(w) = tstack.pop() {
                        onstack[w] = false;
                        scc.push(w);
                        if w == v {
                            break;
                        }
                    }
                    if scc.len() > 1 {
                        out.extend(scc.iter().map(|&o| bs[o]));
                    } else if selfedge[scc[0]] {
                        out.insert(bs[scc[0]]);
                    }
                }
                if let Some(&(u, _)) = work.last() {
                    low[u] = low[u].min(low[v]);
                }
                continue;
            };
            // A successor can be a block `blocks` didn't list (a
            // terminator referencing outside region 0) — the old code
            // treated it as a fresh node; grow the dense state to match.
            let wo = match ord.get(&w) {
                Some(&o) => o,
                None => {
                    let o = bs.len();
                    bs.push(w);
                    ord.insert(w, o);
                    index.push(u32::MAX);
                    low.push(0);
                    onstack.push(false);
                    selfedge.push(false);
                    o
                }
            };
            if wo == v {
                selfedge[v] = true;
            } else if index[wo] == u32::MAX {
                index[wo] = next;
                low[wo] = next;
                next += 1;
                tstack.push(wo);
                onstack[wo] = true;
                work.push((wo, bsuccs(ctx, st, w)));
            } else if onstack[wo] {
                low[v] = low[v].min(low[wo]);
            }
        }
    }
    out
}

/// Parameter indices of `f` that (through loads / GEPs) feed an indirect call's
/// target: a constant address there (a vtable, a fn pointer) lets `devirt`
/// make the call direct once `f` is inlined, as LLVM's inline-cost bonus does.
/// `targets` is the indirect-call operand-0s `callee_info` already collected.
fn devirt_params(ctx: &Context, params: &[Value], targets: &[Value]) -> Vec<usize> {
    let mut out = Vec::new();
    for &t in targets {
        let mut p = t;
        for _ in 0..4 {
            if let Some(i) = params.iter().position(|&a| a == p) {
                if !out.contains(&i) {
                    out.push(i);
                }
                break;
            }
            match p.defining_op() {
                Some(d)
                    if Operation::is_op::<LoadOp>(d, ctx)
                        || Operation::is_op::<GetElementPtrOp>(d, ctx) =>
                {
                    p = d.deref(ctx).get_operand(ctx, 0);
                }
                _ => break,
            }
        }
    }
    out
}

/// Parameter indices of `f` that reach a conditional branch's condition
/// through a short def chain: a scalar-constant argument there folds the
/// branch in the inlined clone, so the callsite gets a size-limit bonus.
/// `roots` is the branch-condition operands `callee_info` already collected
/// (it decides whether `switch` conditions are included).
fn cond_params(ctx: &Context, params: &[Value], roots: &[Value]) -> Vec<usize> {
    let mut out = Vec::new();
    for &root in roots {
        let mut work = vec![root];
        let mut seen: FxHashSet<Value> = FxHashSet::default();
        let mut budget = 32;
        while let Some(v) = work.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            if let Some(i) = params.iter().position(|&a| a == v) {
                if !out.contains(&i) {
                    out.push(i);
                }
                continue;
            }
            if !seen.insert(v) {
                continue;
            }
            let Some(d) = v.defining_op() else {
                continue;
            };
            for oi in 0..d.deref(ctx).get_num_operands() {
                work.push(d.deref(ctx).get_operand(ctx, oi));
            }
        }
    }
    out
}

fn call_counts(ctx: &Context, st: &State) -> FxHashMap<String, usize> {
    let mut n: FxHashMap<String, usize> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op) || st.dead_fns.contains(sym) {
            continue;
        }
        for b in blocks_iter(ctx, f.op) {
            for op in b.deref(ctx).iter(ctx) {
                if let Some(c) = direct_callee(ctx, st, op) {
                    if let Some(n) = n.get_mut(c) {
                        *n += 1;
                    } else {
                        n.insert(c.clone(), 1);
                    }
                }
            }
        }
    }
    n
}

/// Callers among *optimizable* bodies: stripped (`dead_fns`) and emit-only
/// (`dead_unopt`) bodies don't count — a dead caller's call shouldn't keep
/// its callee in the optimization pipeline (the callee still emits, so
/// the emit-only call always links).
fn live_counts(ctx: &Context, st: &State) -> FxHashMap<String, usize> {
    let mut n: FxHashMap<String, usize> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op)
            || st.dead_fns.contains(sym)
            || st.dead_unopt.contains(sym)
        {
            continue;
        }
        for b in blocks_iter(ctx, f.op) {
            for op in b.deref(ctx).iter(ctx) {
                if let Some(c) = direct_callee(ctx, st, op) {
                    if let Some(n) = n.get_mut(c) {
                        *n += 1;
                    } else {
                        n.insert(c.clone(), 1);
                    }
                }
            }
        }
    }
    n
}

/// Callee -> callsite-owner syms adjacency plus the `single`-inline inputs,
/// built once and kept exact across the runs that share it: inlining
/// `caller -> callee` removes one entry from `callers_of[callee]` and adds
/// `caller` to every callee the inlined body brought with it (`taken` is a
/// `st.consts` fact — a body can't introduce a new use of a symbol it
/// didn't already reference, so it needs no maintenance here).
pub struct Adj {
    /// One entry per direct-call site (a caller holding two sites to the
    /// same callee appears twice, like `call_counts`).
    pub callers_of: FxHashMap<String, Vec<String>>,
    /// Symbols whose address is referenced outside calls.
    taken: FxHashSet<String>,
}

impl Adj {
    fn build(ctx: &Context, st: &State) -> Self {
        let mut callers_of: FxHashMap<String, Vec<String>> = FxHashMap::default();
        for (sym, f) in &st.funcs {
            if !has_body(ctx, f.op)
                || st.dead_fns.contains(sym)
                || st.dead_unopt.contains(sym)
            {
                continue;
            }
            for b in blocks_iter(ctx, f.op) {
                for op in b.deref(ctx).iter(ctx) {
                    if let Some(c) = direct_callee(ctx, st, op) {
                        callers_of.entry(c.clone()).or_default().push(sym.clone());
                    }
                }
            }
        }
        Adj {
            callers_of,
            taken: crate::address_taken(ctx, st),
        }
    }

    /// `call` (`sym -> cs`) was inlined: the site is gone, and `cs`'s own
    /// calls now live in `sym` (its out-of-line copy still exists — its
    /// calls stay).
    fn inlined(&mut self, ctx: &Context, st: &State, sym: &str, cs: &str) {
        if let Some(v) = self.callers_of.get_mut(cs) {
            if let Some(p) = v.iter().position(|x| x == sym) {
                v.swap_remove(p);
            }
        }
        for m in callees(ctx, st, cs) {
            self.callers_of.entry(m).or_default().push(sym.to_string());
        }
    }
}

/// What a `run` changed, for driving the next round's caller filter.
#[derive(Default)]
pub struct Delta {
    /// Callers that received inlined code.
    pub callers: FxHashSet<String>,
    /// Callees that lost a callsite — their remaining callers may now see
    /// the single-callsite free inline.
    pub callees: FxHashSet<String>,
}

/// Marks local fns that nothing references any more (typically fully inlined)
/// so lowering skips them. Iterates because a dead fn's calls don't count.
/// `PLIRON_DEADFN=0` disables it.
pub fn dead_fns(ctx: &Context, st: &mut State) {
    let taken = crate::address_taken(ctx, st);
    loop {
        let counts = call_counts(ctx, st);
        let dead: Vec<String> = st
            .funcs
            .iter()
            .filter(|(n, f)| {
                f.linkage == Linkage::Local
                    && has_body(ctx, f.op)
                    && !st.dead_fns.contains(n.as_str())
                    && (st.xcgufn_kill || !st.xcgu_copies.contains(n.as_str()))
                    && !taken.contains(n.as_str())
                    && !counts.contains_key(n.as_str())
            })
            .map(|(n, _)| n.clone())
            .collect();
        if dead.is_empty() {
            break;
        }
        st.dead_fns.extend(dead);
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "deadfn {}: {} of {} fns not lowered",
                st.cgu,
                st.dead_fns.len(),
                st.funcs.len()
            )
        };
    }
    // Protected (`xcgu_copies`) fns that trip the same dead test get a
    // cheaper sentence than stripping: `dead_unopt` emits them through
    // only the `Gate::Always` correctness lowerings. They stay "live"
    // for `call_counts` — an invisible caller still links and runs a
    // correct (unoptimized) body, and their own callees keep counting.
    mark_dead_unopt(ctx, st);
}

/// Mark `xcgu_copies` members nothing live references as `dead_unopt`:
/// `Local` + bodied + no `live_counts` caller + not `live`-address-taken,
/// iterated to a fixpoint (a marked caller's calls stop counting).
///
/// The mark only ever costs optimization, never correctness: a marked
/// fn still emits a (raw, `Gate::Always`-lowered) body, so a caller
/// `direct_callee` can't see — or one a later pass newly creates —
/// still links and runs. And the mark can't become stale: a marked fn
/// is untaken, so no `Sym` const of it exists for a later devirt/spec
/// rewrite to aim a new call at.
///
/// Call sites: `dead_fns` (final catch-all) plus, under
/// `PLIRON_EARLYDEAD`, the `iropt_head` entry and the post-devirt
/// point — every `bodied`/`opt_body` pass after that skips the marked
/// bodies entirely.
pub fn mark_dead_unopt(ctx: &Context, st: &mut State) -> usize {
    if !st.deadunopt_on || st.xcgu_copies.is_empty() {
        return 0;
    }
    let before = st.dead_unopt.len();
    loop {
        let taken = crate::address_taken_ex(ctx, st, true);
        let counts = live_counts(ctx, st);
        let dead: Vec<String> = st
            .funcs
            .iter()
            .filter(|(n, f)| {
                f.linkage == Linkage::Local
                    && has_body(ctx, f.op)
                    && st.xcgu_copies.contains(n.as_str())
                    && !st.dead_fns.contains(n.as_str())
                    && !st.dead_unopt.contains(n.as_str())
                    && !taken.contains(n.as_str())
                    && !counts.contains_key(n.as_str())
            })
            .map(|(n, _)| n.clone())
            .collect();
        if dead.is_empty() {
            break;
        }
        for n in dead {
            st.dead_unopt_ops.insert(st.funcs[&n].op);
            st.dead_unopt.insert(n);
        }
    }
    let marked = st.dead_unopt.len() - before;
    if marked > 0 && pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::stats",
            "deadunopt {}: +{} = {} of {} fns minimal-pipelined",
            st.cgu,
            marked,
            st.dead_unopt.len(),
            st.funcs.len()
        );
    }
    marked
}

/// Functions with bodies, callees before callers (cycles broken arbitrarily).
/// `filter` restricts both the roots and which callees the DFS descends
/// into — a restricted run still orders its callees before their callers.
fn post_order(ctx: &Context, st: &State, filter: Option<&FxHashSet<String>>) -> Vec<String> {
    let mut seen = rustc_hash::FxHashSet::default();
    let mut out = Vec::new();
    let roots: Vec<&String> = match filter {
        Some(f) => f.iter().collect(),
        None => st.funcs.keys().collect(),
    };
    for root in roots {
        if !has_body(ctx, st.funcs[root].op)
            || st.dead_unopt.contains(root.as_str())
            || !seen.insert(root.clone())
        {
            continue;
        }
        let mut stack = vec![(root.clone(), callees(ctx, st, root), 0usize)];
        while let Some((sym, cs, i)) = stack.last_mut() {
            if let Some(c) = cs.get(*i).cloned() {
                *i += 1;
                if filter.is_none_or(|f| f.contains(&c))
                    && has_body(ctx, st.funcs[&c].op)
                    && seen.insert(c.clone())
                {
                    let cc = callees(ctx, st, &c);
                    stack.push((c, cc, 0));
                }
            } else {
                out.push(sym.clone());
                stack.pop();
            }
        }
    }
    out
}

fn callees(ctx: &Context, st: &State, sym: &str) -> Vec<String> {
    blocks_iter(ctx, st.funcs[sym].op)
        .flat_map(|b| b.deref(ctx).iter(ctx))
        .filter_map(|op| direct_callee(ctx, st, op).cloned())
        .filter(|c| st.funcs.contains_key(c))
        .collect()
}

/// `small`: size-sensitive output (wasm, -Copt-level=s/z) keeps the old 40-op limit and no
/// single-caller inlining; rustc.wasm tripled in code size (past V8's 1 GB module cap) without it.
///
/// The cost model (`PLIRON_INLINE_MODEL`, default on; `0` = the fixed
/// thresholds) inlines a callsite when `cost <= threshold`:
///
/// - cost = the callee's op count, plus `CALL_OP_W` per nested call (inlining
///   moves, doesn't remove, them), times `COLD_W` for `#[cold]` callees.
/// - threshold = the `PLIRON_INLINE` base, x10 for `#[inline(always)]` or a
///   local callee with a single callsite (its out-of-line copy then dies),
///   x`PLIRON_INLINE_DEVIRT` when a constant-address actual feeds an
///   indirect-call param, x`PLIRON_INLINE_FOLD` when a constant actual feeds
///   a branch condition, plus `FOLD_USE`/`MEM_USE` per use a constant or
///   local-slot actual resolves, plus half the base when the callsite sits in
///   a loop — all capped at `MAX_T`x the base.
/// - `PLIRON_INLINE_GROWTH` (default `GROWTH_MULT`x the base) caps the
///   non-free ops one caller may absorb; `PLIRON_INLINE_CALLER_MAX` still
///   bounds total size. `small` disables every bonus and the budget.
/// `hot` (small mode only): callsites inside a loop get `hot`x the flat
/// `limit` as their threshold — see [`WASM_HOT`]. 0 disables.
///
/// `callers` restricts which callers get (re)scored: the incremental
/// rounds pass the callers previous runs/devirt touched instead of
/// rescanning the whole module. A caller outside the filter can't inline
/// anything anyway — `only` still gates which of its *pre-existing* sites
/// may go. `adj` is the callsite adjacency, reused across back-to-back
/// rounds; pass an empty `Option` to force a rebuild when other passes ran
/// in between. Returns the `Delta` the next round's filter is built from.
pub fn run(
    ctx: &mut Context,
    st: &mut State,
    small: bool,
    small_limit: usize,
    hot: usize,
    only: Option<&FxHashSet<Ptr<Operation>>>,
    always_only: bool,
    callers: Option<&FxHashSet<String>>,
    adj: &mut Option<Adj>,
) -> Delta {
    let limit = crate::env_get("PLIRON_INLINE")
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { small_limit } else { DEFAULT_LIMIT });
    // -O0 still honours `#[inline(always)]` like LLVM's AlwaysInliner: wasm-bindgen
    // needs its describe markers inlined into each monomorphised shim.
    let limit = if always_only { usize::MAX / 16 } else { limit };
    if limit == 0 {
        return Delta::default();
    }
    // Local fns with one direct call site and no address use get 10x the limit:
    // inlining them removes the out-of-line copy (`PLIRON_INLINE_ONCE=0` disables).
    let once = !always_only && crate::env_get("PLIRON_INLINE_ONCE").map_or(!small, |v| v != "0");
    if once || callers.is_some() {
        adj.get_or_insert_with(|| Adj::build(ctx, st));
    }
    let single: FxHashSet<String> = if once {
        let adj = adj.as_ref().unwrap();
        st.funcs
            .iter()
            .filter(|(n, f)| {
                f.linkage == Linkage::Local
                    && !adj.taken.contains(n.as_str())
                    && adj.callers_of.get(n.as_str()).is_some_and(|v| v.len() == 1)
            })
            .map(|(n, _)| n.clone())
            .collect()
    } else {
        FxHashSet::default()
    };
    // PLIRON_INLINE_BU=0: one flat round, non-invoke sites only (the old heuristic).
    let bottom_up = !always_only && crate::pass_enabled("PLIRON_INLINE_BU");
    // EH-invoke inlining grows code ~14% (regex-syntax), so size-sensitive output skips it.
    let eh_invoke = crate::env_get("PLIRON_INLINE_EH_INVOKE").map_or(!small, |v| v != "0");
    let cap: usize = crate::env_get("PLIRON_INLINE_CALLER_MAX")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    // Callees up to `bonus`x the limit whose indirect call target comes from a
    // parameter are inlined where that argument is a constant address
    // (`PLIRON_INLINE_DEVIRT=<x>`; e.g. hashbrown's `find_or_find_insert_index_inner`
    // taking `&mut dyn FnMut`).
    let bonus: usize = crate::env_get("PLIRON_INLINE_DEVIRT")
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { 0 } else { 4 });
    // Similarly, callees up to `fold`x the limit whose param reaches a branch
    // condition are inlined where that argument is a scalar constant, which
    // folds the branch in the clone (`PLIRON_INLINE_FOLD=<x>`).
    let fold: usize = crate::env_get("PLIRON_INLINE_FOLD")
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { 0 } else { 2 });
    // `#[inline]` callees get up to `hint`x the limit (`PLIRON_INLINE_HINT`):
    // rustc's hint means "small or hot leaf" — std's `hash_one`/iterator
    // adapters sit just over the base limit but must inline to match LLVM.
    let hint: usize = crate::env_get("PLIRON_INLINE_HINT")
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { 0 } else { 4 });
    // The profitability model (`PLIRON_INLINE_MODEL=0` gives the fixed
    // thresholds). `small` stays on the legacy rules: wasm/-Os/-Oz must not
    // grow code on heuristics tuned for speed.
    let model = crate::pass_enabled("PLIRON_INLINE_MODEL") && !small;
    // `PLIRON_INLINE_LOOPCALLS=<n>`: the in-loop bonus only for callees
    // with at most `n` calls of their own (small mode's leaf-ish rule).
    let loop_calls: usize = crate::env_get("PLIRON_INLINE_LOOPCALLS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    // `PLIRON_INLINE_CALLW`: per-nested-call cost weight (`CALL_OP_W`).
    let call_w: usize = crate::env_get("PLIRON_INLINE_CALLW")
        .and_then(|s| s.parse().ok())
        .unwrap_or(CALL_OP_W);
    // `PLIRON_INLINE_HIST=0` lets a site cloned out of an inlined body
    // inline a callee already on its clone chain (`State::inline_hist`).
    let hist = crate::pass_enabled("PLIRON_INLINE_HIST");
    // `PLIRON_INLINE_SCC` (`=0` disables): a site cloned out of an inlined body may not
    // inline a callee in its caller's own (nontrivial) call-graph SCC —
    // LLVM's CGSCC rule for edges inlining created inside an SCC. Original
    // sites still inline one level; re-entering the recursion stops.
    let scc: FxHashMap<String, u32> = if crate::pass_enabled("PLIRON_INLINE_SCC") {
        scc_ids(ctx, st)
    } else {
        FxHashMap::default()
    };
    let verbose = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::inline", tracing::Level::DEBUG);
    // Non-free ops a single caller may absorb (`PLIRON_INLINE_GROWTH`; the
    // model only). `cap` still bounds the caller's absolute size.
    let growth: usize = crate::env_get("PLIRON_INLINE_GROWTH")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| limit.saturating_mul(GROWTH_MULT));
    // Callee infos are computed lazily at first use — a filtered round
    // profiles only the callees its callers actually reference.
    let mut infos: FxHashMap<String, Option<CInfo>> = FxHashMap::default();
    macro_rules! add_info {
        ($sym:expr) => {
            infos.insert(
                $sym.to_string(),
                callee_info(ctx, st, $sym, model, limit, bonus, fold),
            );
        };
    }
    let order: Vec<String> = if bottom_up {
        post_order(ctx, st, callers)
    } else {
        st.funcs
            .iter()
            .filter(|(_, f)| has_body(ctx, f.op))
            .filter(|(s, _)| callers.is_none_or(|c| c.contains(*s)))
            .filter(|(s, _)| !st.dead_unopt.contains(s.as_str()))
            .map(|(s, _)| s.clone())
            .collect()
    };
    let mut delta = Delta::default();
    let mut rw = IRRewriter::<DummyListener>::default();
    let _setup_end = tracing::info_span!(target: "pliron::iropt", "inline-prep").entered();
    drop(_setup_end);
    let _scan = tracing::info_span!(target: "pliron::iropt", "inline-scan").entered();
    let (mut t_scan, mut t_call, mut t_info) = (0f64, 0f64, 0f64);
    let mut t0 = std::time::Instant::now();
    // Lazily-built per-callee side-table annotation lists; a callee's
    // entry is dropped when that function's own body is rewritten below
    // (it may be inlined again afterwards with the new body).
    let mut tagcache: FxHashMap<Ptr<Operation>, CalleeTags> = FxHashMap::default();
    for sym in order {
        let caller = st.funcs[&sym].op;
        if st.opt_skip.contains(&caller) {
            continue;
        }
        let mut size: usize = blocks_iter(ctx, caller)
            .map(|b| b.deref(ctx).iter(ctx).count())
            .sum();
        let mut grown = 0usize;
        // `only` (the devirt-inline pass) restricts which pre-existing
        // callsites may inline; ops this run itself splices in are fair game —
        // e.g. devirt'd `{closure}` exposes `hash_one` inside `reserve_rehash`.
        let pre: FxHashSet<Ptr<Operation>> = only
            .map(|_| {
                blocks_iter(ctx, caller)
                    .flat_map(|b| b.deref(ctx).iter(ctx))
                    .collect()
            })
            .unwrap_or_default();
        // Inlining exposes nested callsites the scan hadn't seen: e.g.
        // `HashMap::get` brings `hash_one` inside the caller's loop, where
        // the in-loop bonus may now accept it. Rescan until a round finds
        // nothing worth inlining (`size`/`grown`/`cap` bound the fixpoint).
        // `inline_call` only ever *adds* blocks (the split continuation and
        // the cloned callee body) — existing blocks never gain a new CallOp
        // (branches/erase don't produce one), so each block is scanned once
        // and later rounds walk only freshly spliced blocks.
        let mut done_blocks: ColSet<Ptr<BasicBlock>> = ColSet::default();
        // Persistent mirror of the caller's block order: inlining splices
        // the split continuation right after the callsite block and appends
        // callee clones at the region end, so a rescan can't just extend —
        // `pending` holds continuations keyed by the block they follow and
        // the scan splices them in at the same point the real list would.
        let mut blklist = blocks(ctx, caller);
        let mut pending: ColMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = ColMap::default();
        for _round in 0..32 {
            let mut sites = Vec::new();
            let mut loops: Option<FxHashSet<Ptr<BasicBlock>>> = None;
            let mut bi = 0;
            while let Some(&b) = blklist.get(bi) {
                bi += 1;
                if let Some(tail) = pending.remove(b) {
                    blklist.splice(bi..bi, tail);
                }
                if !done_blocks.insert(b) {
                    continue;
                }
                for op in b.deref(ctx).iter(ctx) {
                    if !bottom_up && st.invokes.contains(op) {
                        continue;
                    }
                    if pre.contains(&op) && only.is_some_and(|o| !o.contains(&op)) {
                        continue;
                    }
                    let Some(cs) = direct_callee(ctx, st, op) else {
                        continue;
                    };
                    // `always_only` profiles `#[inline(always)]` callees
                    // only — the old upfront profiling skipped the rest.
                    if always_only && !st.funcs[cs].always_inline {
                        continue;
                    }
                    if !infos.contains_key(cs) {
                        let ti = std::time::Instant::now();
                        infos.insert(
                            cs.clone(),
                            callee_info(ctx, st, cs, model, limit, bonus, fold),
                        );
                        t_info += ti.elapsed().as_secs_f64() * 1e3;
                    }
                    let ci = &infos[cs];
                    let Some(ci) = ci.as_ref() else {
                        if verbose {
                            if pliron_cfg::tracing_on() {
                                tracing::debug!(target: "pliron::inline", "inline {}: {sym} -> {cs}: reject (ineligible)", st.cgu)
                            };
                        }
                        continue;
                    };
                    let f = &st.funcs[cs];
                    // PGO: a callsite the profile says never ran isn't
                    // worth growing the caller for (`always_inline` is a
                    // semantic requirement and still fires).
                    let site_count = crate::pgo::block_count(st, b);
                    if !f.always_inline && site_count == Some(0) {
                        if verbose {
                            if pliron_cfg::tracing_on() {
                                tracing::debug!(target: "pliron::inline", "inline {}: {sym} -> {cs}: reject (cold callsite)",
                                    st.cgu
                                )
                            };
                        }
                        continue;
                    }
                    // Free inlines: the out-of-line copy disappears
                    // (single-callsite locals) or rustc demanded it.
                    let free = f.always_inline || (once && single.contains(cs));
                    let mut t = if free {
                        limit.saturating_mul(10)
                    } else {
                        limit
                    };
                    // `args` only feeds the bonus/fold/model scoring below —
                    // collect the operand Vec lazily so plain candidates skip
                    // the allocation entirely.
                    let need_args = (bonus > 1 && !ci.dvirt.is_empty())
                        || (fold > 1 && !ci.conds.is_empty())
                        || model;
                    let args = if need_args {
                        Operation::get_op::<CallOp>(op, ctx)
                            .map(|c| c.args(ctx))
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    // A constant address feeding an indirect-call param lets
                    // `devirt` make the call direct; a scalar constant feeding a
                    // branch condition folds it. (The old `dv` map's bonuses.)
                    if bonus > 1
                        && ci.dvirt.iter().any(|&i| {
                            args.get(i).is_some_and(|a| {
                                matches!(st.consts.get(*a), Some(ConstVal::Sym { .. }))
                            })
                        })
                    {
                        t = t.max(limit.saturating_mul(bonus));
                    }
                    if fold > 1
                        && ci
                            .conds
                            .iter()
                            .any(|&i| args.get(i).is_some_and(|a| const_arg(st, a)))
                    {
                        t = t.max(limit.saturating_mul(fold));
                    }
                    if hint > 1 && f.inline_hint {
                        t = t.max(limit.saturating_mul(hint));
                    }
                    // PGO: a callsite the profile proves runs is worth
                    // more code growth than a static guess.
                    if site_count.is_some_and(|c| c >= 4) {
                        t = t.saturating_add(limit.saturating_mul(2));
                    }
                    let mut cost = ci.n;
                    if model {
                        cost = ci.n.saturating_add(ci.ncalls.saturating_mul(call_w));
                        let mut save = 0usize;
                        for (i, a) in args.iter().enumerate() {
                            if const_arg(st, a) {
                                save = save.saturating_add(
                                    FOLD_USE * ci.fold.get(i).copied().unwrap_or(0) as usize,
                                );
                            }
                            if const_arg(st, a) || st.allocas.contains(*a) {
                                save = save.saturating_add(
                                    MEM_USE * ci.mem.get(i).copied().unwrap_or(0) as usize,
                                );
                            }
                        }
                        t = t.saturating_add(save);
                        if f.cold {
                            cost = cost.saturating_mul(COLD_W);
                        }
                        if ci.ncalls <= loop_calls
                            && loops
                                .get_or_insert_with(|| loop_blocks(ctx, st, caller))
                                .contains(&b)
                        {
                            // `PLIRON_INLINE_LOOP` = bonus in limit/4 units.
                            // A callsite in a loop pays the call+prologue cost
                            // every iteration, so a fatter callee is worth it
                            // (hashbrown's `hash_one`: SipHash inlined into the
                            // probe loop wins ~30% on `HashMap::get`).
                            let lb: usize = crate::env_get("PLIRON_INLINE_LOOP")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(8);
                            t = t.saturating_add(limit * lb / 4);
                        }
                        t = t.min(limit.saturating_mul(MAX_T));
                        if free {
                            // Net global growth is ~0 — the callee's out-of-line
                            // copy dies — so nested calls and coldness aren't
                            // charged; `cap` still bounds the caller's size.
                            cost = ci.n;
                        }
                    } else if hot > 1
                        && ci.ncalls <= 2
                        && loops
                            .get_or_insert_with(|| loop_blocks(ctx, st, caller))
                            .contains(&b)
                    {
                        // Small mode keeps the flat rule everywhere except a
                        // callsite in a loop: it runs the callee once per
                        // iteration, so up to `hot`x the limit is worth it
                        // (std `hash_one` into hashbrown's probe loop).
                        // Leaf-ish bodies only — a callee that still carries
                        // many calls of its own (`try_fold` folds, ~5 calls)
                        // just moves that call traffic into the loop and
                        // regressed `tokenize` ~30% at `hot=2`.
                        t = t.max(limit.saturating_mul(hot));
                    }
                    // A callee with landing pads may go into an invoke whose pad is a
                    // cleanup: its `_Unwind_Resume` becomes an invoke of that pad. Not
                    // into a catch pad: unwinder phase 1 would never see that catch.
                    let eh_block = ci.eh
                        && st
                            .invokes.get(op)
                            .is_some_and(|&(_, catch)| catch || !eh_invoke);
                    let why = if cost > t {
                        "reject (score)"
                    } else if eh_block {
                        "reject (eh)"
                    } else if ci.op == caller || st.calls[op].fn_ty != f.ty {
                        "reject (self/fnty)"
                    } else if hist && on_chain(st, op, ci.op) {
                        "reject (recursive)"
                    } else if !scc.is_empty()
                        && st.inline_hist.contains(op)
                        && scc.get(cs).is_some_and(|c| scc.get(&sym) == Some(c))
                    {
                        "reject (scc)"
                    } else if size + ci.n > cap {
                        "reject (cap)"
                    } else if model && !free && grown + ci.n > growth {
                        "reject (growth)"
                    } else {
                        if model && !free {
                            grown += ci.n;
                        }
                        size += ci.n;
                        sites.push((op, ci.op, cs.clone()));
                        "accept"
                    };
                    if verbose {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::inline", "inline {}: {sym} -> {cs}: n={} calls={} cost={cost} t={t}: {why}",
                                st.cgu, ci.n, ci.ncalls
                            )
                        };
                    }
                }
            }
            t_scan += t0.elapsed().as_secs_f64() * 1e3;
            t0 = std::time::Instant::now();
            if sites.is_empty() {
                break;
            }
            for (call, callee, cs) in sites {
                let tags = tagcache
                    .entry(callee)
                    .or_insert_with(|| callee_tags(&*ctx, st, callee));
                let (b, mut news) =
                    inline_call(ctx, st, &mut rw, call, callee, &cs, &sym, tags);
                // `news[0]` is the split continuation (follows `b` in region
                // order); the rest are callee clones appended at region end.
                pending.or_default(b).push(news.remove(0));
                blklist.extend(news);
                if let Some(a) = adj.as_mut() {
                    a.inlined(ctx, st, &sym, &cs);
                }
                delta.callers.insert(sym.clone());
                delta.callees.insert(cs);
            }
            t_call += t0.elapsed().as_secs_f64() * 1e3;
            t0 = std::time::Instant::now();
        }
        if bottom_up {
            add_info!(&sym);
        }
        tagcache.remove(&caller);
    }
    pliron_cfg::trace("in.scan", t_scan, &st.cgu);
    pliron_cfg::trace("in.call", t_call, &st.cgu);
    pliron_cfg::trace("in.info", t_info, &st.cgu);
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        left_stats(ctx, st, &mut infos, model, limit, bonus, fold);
    }
    delta
}

/// Call-graph SCC id of every bodied function in a nontrivial SCC (two
/// or more members; self-recursion is handled by `callee_info`).
/// Iterative Tarjan over `callees`.
fn scc_ids(ctx: &Context, st: &State) -> FxHashMap<String, u32> {
    let names: Vec<&String> = st
        .funcs
        .iter()
        .filter(|(n, f)| has_body(ctx, f.op) && !st.dead_unopt.contains(n.as_str()))
        .map(|(n, _)| n)
        .collect();
    let ix: FxHashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
    let succ: Vec<Vec<usize>> = names
        .iter()
        .map(|n| {
            callees(ctx, st, n)
                .iter()
                .filter_map(|c| ix.get(c.as_str()).copied())
                .collect()
        })
        .collect();
    const UNSEEN: u32 = u32::MAX;
    let n = names.len();
    let (mut index, mut low) = (vec![UNSEEN; n], vec![0u32; n]);
    let mut on = vec![false; n];
    let (mut stack, mut next, mut out) = (Vec::new(), 0u32, FxHashMap::default());
    let mut nscc = 0u32;
    for root in 0..n {
        if index[root] != UNSEEN {
            continue;
        }
        let mut dfs = vec![(root, 0usize)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        while let Some(&mut (v, ref mut k)) = dfs.last_mut() {
            if let Some(&w) = succ[v].get(*k) {
                *k += 1;
                if index[w] == UNSEEN {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on[w] = true;
                    dfs.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            dfs.pop();
            if let Some(&(p, _)) = dfs.last() {
                low[p] = low[p].min(low[v]);
            }
            if low[v] == index[v] {
                let mut members = Vec::new();
                loop {
                    let w = stack.pop().unwrap();
                    on[w] = false;
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                if members.len() > 1 {
                    for w in members {
                        out.insert(names[w].clone(), nscc);
                    }
                    nscc += 1;
                }
            }
        }
    }
    out
}

/// Whether `callee` was already inlined somewhere on `site`'s clone chain.
fn on_chain(st: &State, site: Ptr<Operation>, callee: Ptr<Operation>) -> bool {
    let mut i = st.inline_hist.get(site).copied().unwrap_or(u32::MAX);
    while let Some(&(f, p)) = st.inline_hist_nodes.get(i as usize) {
        if f == callee {
            return true;
        }
        i = p;
    }
    false
}

/// Indirect calls whose callee is a constant function address (a vtable slot
/// folded by `constload`, e.g. hashbrown's `&mut dyn FnMut` probe callback)
/// become direct calls. Returns the rewritten calls plus the callers owning
/// them, so the next inline round can restrict itself to those callers.
pub fn devirt(ctx: &mut Context, st: &mut State) -> (FxHashSet<Ptr<Operation>>, FxHashSet<String>) {
    let mut out = FxHashSet::default();
    let mut owners = FxHashSet::default();
    let fns: Vec<(String, _)> = st
        .funcs
        .iter()
        .filter(|(_, f)| crate::opt_body(ctx, st, f.op))
        .map(|(n, f)| (n.clone(), f.op))
        .collect();
    for (fname, f) in fns {
        for b in blocks(ctx, f) {
            for c in ops(ctx, b) {
                let Some(call) = Operation::get_op::<CallOp>(c, ctx) else {
                    continue;
                };
                // `callee()` clones the attr for direct calls only to
                // discard it here — test the attr's presence instead.
                if call.get_attr_llvm_call_callee(ctx).is_some() {
                    continue;
                }
                let p = call.get_operation().deref(ctx).get_operand(ctx, 0);
                let Some(ConstVal::Sym { sym, off: 0 }) = st.consts.get(p) else {
                    continue;
                };
                let (Some(fi), Some(info), Some(id)) = (
                    st.funcs.get(sym),
                    st.calls.get(c),
                    st.sym_to_ident.get(sym),
                ) else {
                    continue;
                };
                if info.fn_ty != fi.ty {
                    continue;
                }
                let (id, fty) = (id.clone(), info.fn_ty);
                let fty = TypedHandle::<FuncType>::from_handle(fty, ctx).unwrap();
                let args = call.args(ctx);
                let nc = CallOp::new(ctx, CallOpCallable::Direct(id), fty, args).get_operation();
                nc.insert_before(ctx, c);
                if c.deref(ctx).get_num_results() > 0 {
                    let (old, new) = (c.deref(ctx).get_result(ctx, 0), nc.deref(ctx).get_result(ctx, 0));
                    old.replace_all_uses_with(ctx, &new);
                }
                let info = st.calls.remove(c).unwrap();
                st.calls.insert(nc, info);
                if let Some(h) = st.inline_hist.remove(c) {
                    st.inline_hist.insert(nc, h);
                }
                if let Some(u) = st.invokes.remove(c) {
                    st.invokes.insert(nc, u);
                }
                if let Some(e) = st.expect.remove(c) {
                    st.expect.insert(nc, e);
                }
                if st.last_call == Some(c) {
                    st.last_call = Some(nc);
                }
                Operation::erase(c, ctx);
                out.insert(nc);
                owners.insert(fname.clone());
                if pliron_cfg::tracing_on()
                    && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
                {
                    if pliron_cfg::tracing_on() {
                        tracing::info!(target: "pliron::stats", "devirt {fname:?} -> {sym}")
                    };
                }
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "devirt {}: {} calls", st.cgu, out.len())
        };
    }
    (out, owners)
}

/// Why direct calls to defined functions were left in place (`PLIRON_STATS`).
/// Fills unqueried callee infos on demand so filtered rounds report right.
#[allow(clippy::too_many_arguments)]
fn left_stats(
    ctx: &Context,
    st: &State,
    infos: &mut FxHashMap<String, Option<CInfo>>,
    model: bool,
    limit: usize,
    bonus: usize,
    fold: usize,
) {
    let mut why: FxHashMap<&str, usize> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op) {
            continue;
        }
        for b in blocks_iter(ctx, f.op) {
            for op in b.deref(ctx).iter(ctx) {
                let Some(cs) = direct_callee(ctx, st, op) else {
                    continue;
                };
                let g = &st.funcs[cs];
                let r = if !has_body(ctx, g.op) {
                    "external"
                } else if g.no_inline {
                    "no_inline"
                } else if g.linkage == Linkage::Preemptible {
                    "preemptible"
                } else if cs == sym {
                    "self"
                } else if let Some(ci) = infos
                    .entry(cs.clone())
                    .or_insert_with(|| callee_info(ctx, st, cs, model, limit, bonus, fold))
                    .as_ref()
                {
                    if ci.eh && st.invokes.contains(op) {
                        "eh callee at invoke (catch pad / EH_INVOKE=0)"
                    } else if st.calls[op].fn_ty != g.ty {
                        "fn type mismatch"
                    } else {
                        "eligible but left"
                    }
                } else {
                    "too big / recursive / va"
                };
                *why.entry(r).or_default() += 1;
            }
        }
    }
    if pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::stats", "inline left {}: {why:?}", st.cgu)
    };
}

/// The callee's `noalias readonly` params point at memory the caller can't
/// write for the call's duration (rustc's `&T` contract — `partition`'s
/// pivot scan is the payoff: the pivot stays in a register instead of
/// reloading per element). Record the cloned entry args they bind as
/// `region_frozen`: lowering seeds them into `noalias`/`deref`, so licm can
/// hoist their loads to a cloned loop's preheader — still inside the
/// region. `readonly can_move` flags would be wrong: the freeze holds only
/// for the clone's duration, and once a later pass forwards the entry arg
/// to a caller value the flag would leak out of the region.
///
/// `v`'s uniform source through `b`'s predecessors: `Some` only when every
/// pred passes the same value (self-passes on back edges ignored). `None`
/// on any disagreement — a merged value can't be proven to carry one
/// pointer.
fn arg_src(
    ctx: &Context,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    b: Ptr<BasicBlock>,
    v: Value,
) -> Option<Value> {
    let pr = preds.get(&b)?;
    if pr.is_empty() {
        return None;
    }
    let i = v.try_find_index(ctx).ok()?;
    let na = b.deref(ctx).get_num_arguments();
    let mut uniq = None;
    let ok = pr.iter().all(|&(t, si)| {
        let o = Operation::get_op_dyn(t, ctx);
        let Some(ops) =
            op_cast::<dyn BranchOpInterface>(&*o).map(|br| br.successor_operands(ctx, si))
        else {
            return false;
        };
        if ops.len() != na {
            return false;
        };
        let w = ops[i];
        w == v || *uniq.get_or_insert(w) == w
    });
    uniq.filter(|_| ok)
}

/// The `sub`-th leaf of an aggregate value: `insertvalue` chains decompose
/// into the inserted element; block args resolve leaf-wise through `preds`
/// (each pred's operand must yield the same leaf); opaque producers (loads)
/// yield `v` itself only when `sub == 0`.
fn extract_leaf(
    ctx: &Context,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    mut v: Value,
    mut sub: usize,
    depth: usize,
) -> Option<Value> {
    let mut seen = FxHashSet::default();
    let scan = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::frozen", tracing::Level::DEBUG);
    if depth > 24 {
        return None;
    }
    loop {
        if !seen.insert(v) {
            return None;
        }
        if let Some(b) = v.defining_block() {
            // An aggregate forwarded through a block arg: extract the leaf
            // from each pred's operand and require uniformity — preds may
            // pass different aggregates with the same leaf (the pivot tuple
            // is rebuilt per path).
            // Entry/edge-incomplete blocks have no recorded preds — the arg
            // itself is the resolution endpoint (it may carry a frozen
            // record).
            let Some(pr) = preds.get(&b).filter(|p| !p.is_empty()) else {
                return (sub == 0).then_some(v);
            };
            let Ok(i) = v.try_find_index(ctx) else {
                if scan {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::frozen", "extract_leaf: no idx for {v:?} in {b:?}")
                    };
                }
                return None;
            };
            let mut uniq = None;
            for &(t, si) in pr {
                let o = Operation::get_op_dyn(t, ctx);
                let Some(ops) =
                    op_cast::<dyn BranchOpInterface>(&*o).map(|br| br.successor_operands(ctx, si))
                else {
                    if scan {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::frozen", "extract_leaf: non-branch pred {t:?} of {b:?}")
                        };
                    }
                    return None;
                };
                let Some(&w) = ops.get(i) else {
                    if scan {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::frozen", "extract_leaf: arg {i} missing in {t:?} ops for {b:?}")
                        };
                    }
                    return None;
                };
                if w == v {
                    continue;
                }
                let l = extract_leaf(ctx, preds, w, sub, depth + 1)?;
                if uniq.is_some_and(|u| u != l) {
                    if scan {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::frozen", "extract_leaf: arg {v:?} of {b:?} merges {uniq:?} with {l:?}")
                        };
                    }
                    return None;
                }
                uniq = Some(l);
            }
            match uniq {
                Some(l) => {
                    v = l;
                    continue;
                }
                None => return (sub == 0).then_some(v),
            }
        }
        let Some(op) = v.defining_op() else {
            return (sub == 0).then_some(v);
        };
        if Operation::is_op::<InsertValueOp>(op, ctx) {
            let idx = Operation::get_op::<InsertValueOp>(op, ctx)
                .unwrap()
                .indices(ctx);
            let (s, n, _) = leaf_range(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx), &idx);
            if sub >= s && sub < s + n {
                sub -= s;
                v = op.deref(ctx).get_operand(ctx, 1);
            } else {
                v = op.deref(ctx).get_operand(ctx, 0);
            }
            continue;
        }
        if Operation::is_op::<UndefOp>(op, ctx) || Operation::is_op::<PoisonOp>(op, ctx) {
            return None;
        }
        return (sub == 0).then_some(v);
    }
}

/// `true` when `v` is produced by `llvm.alloca` — a distinct local object.
fn is_alloca(ctx: &Context, v: Value) -> bool {
    v.defining_op()
        .is_some_and(|o| Operation::is_op::<AllocaOp>(o, ctx))
}

/// The stored value reaching `load` through memory in its block: the last
/// store to the same resolved address, when every intervening write
/// provably misses it. This is how the comparator's `(a, b)` tuple reaches
/// `lt`: rustc spills it to a fresh alloca — `store b,slot+8; b' = load
/// slot+8` — and forwarding it keeps `b`'s frozen provenance.
/// Intervening same-object stores must be disjoint or exactly cover the
/// load; a store through a different resolved root can only be skipped
/// when the two are distinct `alloca` objects; calls, atomics, mem
/// intrinsics, and volatile ops all end the scan in a miss.
fn store_src(
    ctx: &Context,
    st: &State,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    load: Ptr<Operation>,
    mem: &mut FxHashSet<Ptr<Operation>>,
) -> Option<Value> {
    let scan = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::frozen", tracing::Level::DEBUG);
    let fail = |why: &str| -> Option<Value> {
        if scan {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::frozen", "store_src: {why} for {load:?}")
            };
        }
        None
    };
    if st.volatile.contains(load) {
        return fail("volatile");
    }
    let addr = load.deref(ctx).get_operand(ctx, 0);
    let Some((lb, ll, loff)) = ptr_base(ctx, st, preds, addr, mem) else {
        return fail("ptr_base");
    };
    let Some(loff) = loff else {
        return fail("dynoff");
    };
    let lsz = size_align(ctx, load.deref(ctx).get_result(ctx, 0).get_type(ctx)).0 as i64;
    let Some(b) = load.deref(ctx).get_parent_block() else {
        return fail("noblock");
    };
    let pos = b.deref(ctx).iter(ctx).position(|o| o == load);
    let mut fuel = 32;
    store_src_in(
        ctx, st, preds, lb, ll, loff, lsz, b, pos, &mut fuel, scan, load, mem,
    )
}

/// Find the value stored to slot `(lb,ll,loff..+lsz)` that dominates
/// position `pos` in `b` (None = block end). Single-predecessor chains are
/// walked; at a merge every predecessor's chain must yield the same value
/// (as `extract_leaf` does for block args). `fuel` bounds total work.
#[allow(clippy::too_many_arguments)]
fn store_src_in(
    ctx: &Context,
    st: &State,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    lb: Value,
    ll: Option<usize>,
    loff: i64,
    lsz: i64,
    mut b: Ptr<BasicBlock>,
    mut pos: Option<usize>,
    fuel: &mut usize,
    scan: bool,
    load: Ptr<Operation>,
    mem: &mut FxHashSet<Ptr<Operation>>,
) -> Option<Value> {
    loop {
        *fuel = fuel.checked_sub(1)?;
        let block_ops = ops(ctx, b);
        let start = pos.unwrap_or(block_ops.len());
        // The backward scan must see every op that could clobber the slot:
        // a truncated window would fall through to the pred walk as if the
        // block were transparent and silently skip an older same-slot store.
        if start > 400 {
            return None;
        }
        for (oi, &o) in block_ops[..start].iter().enumerate().rev().take(400) {
            // `llvm.memcpy/memmove(dst, src, n)`: a load fully inside the
            // copied range reads the same bytes the *source* held just
            // before the copy — resolve the store to that source slot.
            if let Some(name) = st.intrinsics.get(o) {
                if name != "llvm.memcpy" && name != "llvm.memmove" {
                    if name.starts_with("llvm.mem") {
                        return None;
                    }
                    continue;
                }
                let od = o.deref(ctx);
                let Some(n) = crate::sroa::const_int(ctx, st, od.get_operand(ctx, 2)) else {
                    return None;
                };
                let Some((db, dl, doff)) = ptr_base(ctx, st, preds, od.get_operand(ctx, 0), mem) else {
                    return None;
                };
                let Some(doff) = doff else { return None };
                if db == lb && dl == ll {
                    let (lo, hi) = (doff, doff + n as i64);
                    if loff >= lo && loff + lsz <= hi {
                        let Some((sb2, sl2, soff2)) =
                            ptr_base(ctx, st, preds, od.get_operand(ctx, 1), mem)
                        else {
                            return None;
                        };
                        let Some(soff2) = soff2 else { return None };
                        return store_src_in(
                            ctx,
                            st,
                            preds,
                            sb2,
                            sl2,
                            soff2 + (loff - doff),
                            lsz,
                            b,
                            Some(oi),
                            fuel,
                            scan,
                            load,
                            mem,
                        );
                    }
                    if hi <= loff || lo >= loff + lsz {
                        continue;
                    }
                    return None;
                }
                if !(is_alloca(ctx, db) && is_alloca(ctx, lb)) {
                    return None;
                }
                continue;
            }
            if Operation::is_op::<StoreOp>(o, ctx) && !st.volatile.contains(o) {
                let od = o.deref(ctx);
                let (sb, sl, soff) = ptr_base(ctx, st, preds, od.get_operand(ctx, 1), mem)?;
                let soff = soff?;
                let ssz = size_align(ctx, od.get_operand(ctx, 0).get_type(ctx)).0 as i64;
                if sb == lb && sl == ll {
                    if soff == loff && ssz == lsz {
                        return Some(od.get_operand(ctx, 0));
                    }
                    if soff < loff + lsz && loff < soff + ssz {
                        return None;
                    }
                    continue;
                }
                // Different resolved roots may still alias — only two
                // distinct `alloca` objects provably don't.
                if !(is_alloca(ctx, sb) && is_alloca(ctx, lb)) {
                    return None;
                }
                continue;
            }
            if Operation::is_op::<CallOp>(o, ctx)
                || Operation::is_op::<AtomicStoreOp>(o, ctx)
                || Operation::is_op::<AtomicRmwOp>(o, ctx)
                || Operation::is_op::<AtomicCmpxchgOp>(o, ctx)
                || Operation::is_op::<StoreOp>(o, ctx)
            {
                if scan {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::frozen", "store_src: writer {} for {load:?}", OpDbg { op: o, ctx })
                    };
                }
                return None;
            }
        }
        // No decisive store here — follow the predecessors.
        let mut pr: Vec<Ptr<BasicBlock>> = Vec::new();
        let Some(pr_ops) = preds.get(&b) else {
            if scan {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::frozen", "store_src: no preds {b:?} for {load:?}")
                };
            }
            return None;
        };
        for (t, _) in pr_ops {
            let Some(p) = t.deref(ctx).get_parent_block() else {
                continue;
            };
            if !pr.contains(&p) {
                pr.push(p);
            }
        }
        if pr.len() == 1 {
            b = pr[0];
            pos = None;
            continue;
        }
        // Merge: every predecessor path must store the same value —
        // compared after arg/cast normalization: paths can store the same
        // pointer under different SSA names (`pivot` reborrowed into two
        // arg chains is still one pointer).
        let mut it = pr.iter();
        let v = store_src_in(
            ctx,
            st,
            preds,
            lb,
            ll,
            loff,
            lsz,
            *it.next()?,
            None,
            fuel,
            scan,
            load,
            mem,
        )?;
        for &p in it {
            let pv = store_src_in(
                ctx, st, preds, lb, ll, loff, lsz, p, None, fuel, scan, load, mem,
            );
            if pv != Some(v)
                && !pv.is_some_and(|w| {
                    let eq = same_ptr(ctx, st, preds, w, v, mem);
                    if scan && !eq {
                        fn d(ctx: &Context, v: Value) -> String {
                            v.defining_op()
                                .map(|o| OpDbg { op: o, ctx }.to_string())
                                .unwrap_or_else(|| format!("{v:?}"))
                        }
                        if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::frozen", "store_src: merge diff {} vs {}", d(ctx, w), d(ctx, v)) };
                    }
                    eq
                })
            {
                if scan {
                    if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::frozen", "store_src: merge {b:?} pred {p:?} -> {pv:?} vs {v:?} for {load:?}") };
                }
                return None;
            }
        }
        return Some(v);
    }
}

/// Same pointer after arg/cast normalization: two SSA names prove equal
/// when they resolve — through `arg_src`, GEPs, bitcasts, freezes, and
/// `extractvalue`, and store-forwarded loads — to the same base at the
/// same constant offset. `mem` bounds the load forwarding the same way it
/// bounds `ptr_base`.
fn same_ptr(
    ctx: &Context,
    st: &State,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    v1: Value,
    v2: Value,
    mem: &mut FxHashSet<Ptr<Operation>>,
) -> bool {
    fn norm(
        ctx: &Context,
        st: &State,
        preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
        mut v: Value,
        mem: &mut FxHashSet<Ptr<Operation>>,
    ) -> (Value, Option<i64>) {
        let mut off = Some(0i64);
        let mut seen = FxHashSet::default();
        loop {
            if !seen.insert(v) {
                return (v, off);
            }
            if let Some(b) = v.defining_block()
                && let Some(w) = arg_src(ctx, preds, b, v)
            {
                v = w;
                continue;
            }
            let Some(op) = v.defining_op() else {
                return (v, off);
            };
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                off = off.and_then(|o| {
                    crate::sroa::gep_offset(ctx, st, op).and_then(|d| o.checked_add(d))
                });
                v = op.deref(ctx).get_operand(ctx, 0);
            } else if Operation::is_op::<BitcastOp>(op, ctx)
                || Operation::is_op::<FreezeOp>(op, ctx)
                || Operation::is_op::<AddrSpaceCastOp>(op, ctx)
            {
                v = op.deref(ctx).get_operand(ctx, 0);
            } else if Operation::is_op::<ExtractValueOp>(op, ctx) {
                let idx = Operation::get_op::<ExtractValueOp>(op, ctx)
                    .unwrap()
                    .indices(ctx);
                let agg = op.deref(ctx).get_operand(ctx, 0);
                let (s, n, _) = leaf_range(ctx, agg.get_type(ctx), &idx);
                if n != 1 {
                    return (v, off);
                }
                match extract_leaf(ctx, preds, agg, s, 0) {
                    Some(l) => v = l,
                    None => return (v, off),
                }
            } else if Operation::is_op::<LoadOp>(op, ctx) && mem.insert(op) {
                // Same-slot loads forwarded through their stores — the
                // `(a, b)` tuple is rebuilt and re-spilled per diamond
                // arm, so the arms' `b` values are distinct loads of
                // what is nonetheless the same source pointer.
                match store_src(ctx, st, preds, op, mem) {
                    Some(sv) => v = sv,
                    None => return (v, off),
                }
            } else {
                return (v, off);
            }
        }
    }
    // Each normalization gets its own `mem` clone — seeded with the
    // shared set for cycle-breaking, but independent so that a load `v1`
    // already forwarded isn't skipped when `v2`'s chain needs it.
    let (n1, n2) = (
        norm(ctx, st, preds, v1, &mut mem.clone()),
        norm(ctx, st, preds, v2, &mut mem.clone()),
    );
    if n1 != n2
        && pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::frozen", tracing::Level::DEBUG)
    {
        let d = |v: Value| {
            v.defining_op()
                .map(|o| OpDbg { op: o, ctx }.to_string())
                .unwrap_or_else(|| format!("{v:?}"))
        };
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::frozen", "same_ptr: {n1:?} ({}) vs {n2:?} ({})", d(n1.0), d(n2.0))
        };
    }
    n1 == n2
}

/// Follow GEPs/bitcasts back to a block arg or instruction result,
/// accumulating the constant byte offset. `None` when an index is dynamic —
/// an unknown offset can't be bounded against the frozen pointee.
/// `preds` maps clone blocks to their (terminator, successor-index) preds:
/// a clone-interior block arg that every predecessor feeds the same value
/// (self-passes on back edges ignored) resolves to that value —
/// `partition`'s pivot reaches the scan loop phi-forwarded this way.
/// An `extractvalue` of an opaque aggregate yields `(agg, Some(leaf))` — the
/// leaf position — so callers can match a recorded frozen leaf.
/// `llvm.load` pointers forward through same-block stores — see
/// `store_src`.
fn ptr_base(
    ctx: &Context,
    st: &State,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    mut v: Value,
    mem: &mut FxHashSet<Ptr<Operation>>,
) -> Option<(Value, Option<usize>, Option<i64>)> {
    let mut off = Some(0i64);
    let mut seen = FxHashSet::default();
    loop {
        if !seen.insert(v) {
            return None;
        }
        if let Some(b) = v.defining_block()
            // A recorded arg is already the best base — `frozen_args`
            // captured any outer-clone region when the record was made.
            // Forwarding past it into the caller's operand chain can only
            // strand the record on an unreachable side path.
            && !st.region_frozen.contains(v)
            && let Some(w) = arg_src(ctx, preds, b, v)
        {
            v = w;
            continue;
        }
        let Some(op) = v.defining_op() else {
            return Some((v, None, off));
        };
        if Operation::is_op::<GetElementPtrOp>(op, ctx) {
            // A dynamic index poisons the offset but not the base — region
            // inheritance only needs which frozen arg it lands in.
            off = off
                .and_then(|o| crate::sroa::gep_offset(ctx, st, op).and_then(|d| o.checked_add(d)));
            v = op.deref(ctx).get_operand(ctx, 0);
        } else if Operation::is_op::<BitcastOp>(op, ctx)
            || Operation::is_op::<FreezeOp>(op, ctx)
            || Operation::is_op::<AddrSpaceCastOp>(op, ctx)
        {
            v = op.deref(ctx).get_operand(ctx, 0);
        } else if Operation::is_op::<ExtractValueOp>(op, ctx) {
            // `extractvalue agg, idx` of a pointer leaf: decomposing `agg`
            // (`insertvalue` chains) keeps provenance; an opaque aggregate
            // returns the leaf position so callers can match a recorded
            // frozen leaf of the aggregate.
            let idx = Operation::get_op::<ExtractValueOp>(op, ctx)
                .unwrap()
                .indices(ctx);
            let agg = op.deref(ctx).get_operand(ctx, 0);
            let (s, n, _) = leaf_range(ctx, agg.get_type(ctx), &idx);
            if n != 1 {
                return Some((v, None, off));
            }
            // Same rule as the arg case: don't decompose a recorded
            // aggregate arg past its own leaf record.
            if st
                .region_frozen.get(agg)
                .is_some_and(|rs| rs.iter().any(|&(ss, _, _)| ss as usize == s))
            {
                return Some((agg, Some(s), off));
            }
            match extract_leaf(ctx, preds, agg, s, 0) {
                Some(l) => v = l,
                None => return Some((agg, Some(s), off)),
            }
        } else if Operation::is_op::<LoadOp>(op, ctx) {
            // A pointer fetched through memory — the comparator's args
            // tuple is spilled to an alloca. Forward through the store
            // that produced it when the clobber scan proves it out.
            // `mem` is shared across the whole `ptr_base`/`store_src`
            // recursion: store pointers can themselves be load results,
            // and without it a value cycle overflows the stack (stage1
            // `core` did — SIGBUS).
            if mem.len() >= 64 || !mem.insert(op) {
                return Some((v, None, off));
            }
            match store_src(ctx, st, preds, op, mem) {
                Some(sv) => v = sv,
                None => return Some((v, None, off)),
            }
        } else {
            return Some((v, None, off));
        }
    }
}

/// `(bytes, region)` for a base resolved at leaf `bl`: an enclosing
/// clone-arg record first; else a frozen param of the containing function
/// itself — the outermost region, anchored at its entry.
/// `anc` is `b` or a proper ancestor of it in the clone-region tree.
fn anc_or_self(st: &State, anc: Ptr<BasicBlock>, mut b: Ptr<BasicBlock>) -> bool {
    for _ in 0..64 {
        if b == anc {
            return true;
        }
        match st.region_parent.get(b) {
            Some(&p) if p != b => b = p,
            _ => return false,
        }
    }
    false
}

/// `(bytes, region)` for `*base` frozen within a region containing `entry`
/// (the clone being processed): an enclosing clone-arg record, a caller
/// operand record, or a frozen param of the containing function itself —
/// the outermost region, anchored at its entry. Among several claims the
/// shallowest anchor wins: it promises the widest frozen span, which is
/// what lets a nested load reach an enclosing loop's preheader.
fn frozen_of(
    ctx: &Context,
    st: &State,
    caller_frz: &[(usize, u64)],
    fentry: Option<Ptr<BasicBlock>>,
    entry: Ptr<BasicBlock>,
    base: Value,
    bl: Option<usize>,
) -> Option<(u64, Ptr<BasicBlock>)> {
    let mut best: Option<(u64, Ptr<BasicBlock>)> = None;
    let mut pick = |st: &State, bytes: u64, r: Ptr<BasicBlock>| {
        if anc_or_self(st, r, entry) && best.is_none_or(|rb| anc_or_self(st, r, rb.1)) {
            best = Some((bytes, r));
        }
    };
    if let Some(rs) = st.region_frozen.get(base) {
        for &(s, bytes, r) in rs {
            if bl.unwrap_or(0) == s as usize {
                pick(st, bytes, r);
            }
        }
    }
    if bl.is_none_or(|b| b == 0)
        && let Some(rs) = st.region_frozen_op.get(base)
    {
        for &(bytes, r) in rs {
            pick(st, bytes, r);
        }
    }
    if best.is_some() {
        return best;
    }
    let fe = fentry?;
    caller_frz.iter().find_map(|&(leaf, bytes)| {
        let (a, s) = leaf_arg(ctx, fe, leaf)?;
        (a == base && bl.unwrap_or(0) == s).then_some((bytes, fe))
    })
}

/// The callee's `noalias readonly` params point at memory the caller can't
/// write for the call's duration (rustc's `&T` contract — `partition`'s
/// pivot scan is the payoff: the pivot stays in a register instead of
/// reloading per element). Record:
/// - `region_frozen[arg] = (leaf, bytes, region)` for each frozen binding;
///   when the caller-side value is itself region-frozen, the region is the
///   *enclosing* clone's — the freeze outlives this call.
/// - `frozen_loads[op] = region` for clone loads proven to read inside the
///   frozen pointee (constant offset, bounded by `dereferenceable`).
/// - `region_blocks[b] = entry` for every cloned block, so lowering can map
///   the region to Cranelift blocks.
/// `readonly can_move` load flags would be wrong here: the freeze holds
/// only inside the clone, and a function-wide flag would escape the region.
fn frozen_args(
    ctx: &Context,
    st: &mut State,
    callee: &str,
    caller: &str,
    src: &[Ptr<BasicBlock>],
    map: &IrMapping,
    call_args: &[Value],
    call_block: Ptr<BasicBlock>,
) {
    if !crate::pass_enabled("PLIRON_FROZEN_INLINE") {
        return;
    }
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::frozen", tracing::Level::DEBUG);
    let Some(entry) = src.first().and_then(|b| map.lookup_block(*b)) else {
        return;
    };
    // The containing function's entry block anchors the outermost region:
    // the function's own frozen params hold for the whole body.
    let fop = call_block.deref(ctx).get_parent_op(ctx);
    let fentry = fop
        .and_then(|o| Operation::get_op::<FuncOp>(o, ctx))
        .and_then(|o| o.get_entry_block(ctx));
    // The clone nests inside whatever region the callsite sits in (the
    // function body when the call is in original code) — the containment
    // chain licm walks to decide whether a preheader is in a tagged
    // load's region. This bookkeeping must run even when the callee has
    // no FuncInfo: a missing link breaks the ancestor walk for every
    // clone nested inside it.
    let parent = st
        .region_blocks.get(call_block)
        .copied()
        .or(fentry)
        .unwrap_or(entry);
    st.region_parent.insert(entry, parent);
    for &b in src {
        if let Some(nb) = map.lookup_block(b) {
            // Nested clones inside `src` keep their own (remapped) region;
            // only unclaimed blocks belong to this clone's region.
            st.region_blocks.or_insert_with(nb, || entry);
        }
    }
    let Some(f) = st.funcs.get(callee) else {
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::frozen", "frozen_args: callee {callee:?} not in funcs")
            };
        }
        return;
    };
    let callee_frozen = !f.frozen.is_empty();
    if dbg {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::frozen", "frozen_args: na={:?} frz={:?} callee={callee:?} entry={entry:?}",
                f.noalias.len(),
                f.frozen.len()
            )
        };
    }
    // CFG edges of the whole containing function, for `ptr_base`'s
    // block-arg resolution: a load's pointer (or a caller operand bound to
    // a frozen arg) is often phi-forwarded through args of *enclosing*
    // clone blocks — `partition`'s pivot reaches `lt` callsites that way.
    let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>> = FxHashMap::default();
    if callee_frozen
        && let Some(region) = call_block.deref(ctx).get_parent_region()
    {
        for b in region.deref(ctx).iter(ctx) {
            if let Some(t) = b.deref(ctx).iter(ctx).last() {
                for (si, s) in t.deref(ctx).successors(ctx).enumerate() {
                    preds.entry(s).or_default().push((t, si));
                }
            }
        }
    }
    // `(bytes, region)` for a base resolved at leaf `bl`: an enclosing
    // clone-arg record first; else a frozen param of the containing
    // function itself — the outermost region, anchored at its entry.
    let caller_frz: Vec<(usize, u64)> = st
        .funcs
        .get(caller)
        .map(|x| x.frozen.clone())
        .unwrap_or_default();
    // Caller-side operands bound to each entry arg, for region inheritance.
    // `inline_call` branches to `entry` with `args` positionally, so the
    // entry arg at index k is bound to `call_args[k]`.
    let eargs: Vec<Value> = entry.deref(ctx).arguments(ctx).collect();
    for &(leaf, bytes) in &f.frozen {
        let Some((a, sub)) = leaf_arg(ctx, entry, leaf) else {
            if dbg {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::frozen", "frozen_args: leaf {leaf} unresolved")
                };
            }
            continue;
        };
        let cv = eargs
            .iter()
            .position(|&x| x == a)
            .and_then(|k| call_args.get(k).copied());
        // `a` may cover several leaves (comparator tuples pack both refs
        // into one operand) — resolve the `sub`-th leaf of the operand.
        let lv = cv.and_then(|v| extract_leaf(ctx, &preds, v, sub, 0));
        let rb = lv.and_then(|v| ptr_base(ctx, st, &preds, v, &mut FxHashSet::default()));
        // Inherit the enclosing region only when the operand's pointee
        // provably sits inside the outer frozen range — `a = pivot + 8`
        // resolves to pivot's base but points *past* its 8 frozen bytes.
        // The clone-entry region is always valid: `a`'s own `readonly`
        // contract holds for this call regardless.
        let inh = rb.and_then(|(b, l, off)| {
            frozen_of(ctx, st, &caller_frz, fentry, entry, b, l)
                .filter(|&(rb_, _)| off.is_some_and(|o| o >= 0 && o + bytes as i64 <= rb_ as i64))
                .map(|(_, r)| r)
        });
        let region = inh.unwrap_or(entry);
        if dbg
            && pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::frozen", tracing::Level::DEBUG)
        {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::frozen", "frozen_args: leaf {leaf} sub {sub} entry {entry:?} inh={inh:?} rb={rb:?} cv={cv:?}"
                )
            };
        }
        if dbg {
            let rbs = rb
                .and_then(|(v, ..)| v.defining_op())
                .map(|o| OpDbg { op: o, ctx }.to_string())
                .unwrap_or_default();
            let cvs = cv
                .and_then(|v| v.defining_op())
                .map(|o| OpDbg { op: o, ctx }.to_string())
                .unwrap_or_default();
            let cas = call_args
                .iter()
                .enumerate()
                .map(|(k, &v)| {
                    v.defining_op()
                        .map(|o| format!("{k}: {}", OpDbg { op: o, ctx }))
                        .unwrap_or_else(|| format!("{k}: {v:?}"))
                })
                .collect::<Vec<_>>()
                .join(" ; ");
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::frozen", "frozen_args: record leaf {leaf} sub {sub} bytes {bytes} region={region:?} callee={callee:?} cv={cv:?} rb={rb:?} {rbs} | {cvs} || {cas}"
                )
            };
        }
        st.region_frozen.or_default(a)
            .push((sub as u32, bytes, region));
        // `*operand` is frozen for the call's duration — exactly this
        // clone's span. Record the caller-side leaf too, in
        // `region_frozen_op` (not `region_frozen`: seeding `noalias` on a
        // caller value would claim function-wide disjointness it doesn't
        // have). Clone loads that forward it through entry args — the
        // partition pivot reaches `lt` callsites as the outer loop's own
        // block param — tag this clone's region, which reaches an
        // enclosing loop's preheader where the load's own entry can't.
        if let Some(lv) = lv {
            st.region_frozen_op.or_default(lv)
                .push((bytes, entry));
        }
    }
    // Loads inside the clone that provably read inside a frozen pointee:
    // they may hoist within the region without a clobber scan. Nothing can
    // match while no frozen record exists anywhere — the usual case for
    // leaf callees — so skip the cloned-op walk outright.
    let any_frozen = callee_frozen
        || !caller_frz.is_empty()
        || !st.region_frozen.is_empty()
        || !st.region_frozen_op.is_empty();
    if any_frozen {
        for &b in src {
            let Some(nb) = map.lookup_block(b) else {
                continue;
            };
            for op in nb.deref(ctx).iter(ctx) {
                if !Operation::is_op::<LoadOp>(op, ctx) {
                    continue;
                }
                let ptr = op.deref(ctx).get_operand(ctx, 0);
                let Some((base, bl, Some(off))) =
                    ptr_base(ctx, st, &preds, ptr, &mut FxHashSet::default())
                else {
                    continue;
                };
                let Some((bytes, region)) =
                    frozen_of(ctx, st, &caller_frz, fentry, entry, base, bl)
                else {
                    if dbg
                        && pliron_cfg::tracing_on()
                        && tracing::enabled!(target: "pliron::frozen", tracing::Level::DEBUG)
                    {
                        let bs = base
                            .defining_op()
                            .map(|o| OpDbg { op: o, ctx }.to_string())
                            .unwrap_or_default();
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::frozen", "frozen_args: miss {op:?} base {base:?} leaf {bl:?} off {off} {bs}")
                        };
                    }
                    continue;
                };
                let sz =
                    size_align(ctx, op.deref(ctx).get_result(ctx, 0).get_type(ctx)).0 as i64;
                if off < 0 || off + sz > bytes as i64 {
                    continue;
                }
                st.frozen_loads.insert(op, region);
                if dbg {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::frozen", "frozen_args: load {op:?} base {base:?} off {off} region {region:?}")
                    };
                }
            }
        }
    }
    for &(leaf, _, _) in &f.noalias {
        let Some((a, sub)) = leaf_arg(ctx, entry, leaf) else {
            continue;
        };
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::frozen", "frozen_args: na record leaf {leaf} a={a:?} entry={entry:?}")
            };
        }
        st.region_noalias.or_default(a).push(sub as u32);
    }
}

/// Returns the callsite block plus the freshly created blocks in region
/// order (`cont` follows `block`; callee clones sit at the region end), so
/// the rescan worklist can track the list without re-walking it.
fn inline_call(
    ctx: &mut Context,
    st: &mut State,
    rw: &mut IRRewriter<DummyListener>,
    call: Ptr<Operation>,
    callee: Ptr<Operation>,
    cs: &str,
    caller: &str,
    tags: &CalleeTags,
) -> (Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>) {
    let block = call.deref(ctx).get_parent_block().unwrap();
    let region = block.deref(ctx).get_parent_region().unwrap();
    let args = Operation::get_op::<CallOp>(call, ctx).unwrap().args(ctx);
    let res = (call.deref(ctx).get_num_results() > 0).then(|| call.deref(ctx).get_result(ctx, 0));
    let unwind = st.invokes.remove(call);
    let cont = rw.split_block(ctx, block, OpInsertionPoint::AfterOperation(call), None);
    // PGO: scale the callee's counts by callsite/entry so cloned blocks
    // carry an honest count into the caller (LLVM's scaling rule).
    let callsite_c = st.prof_bcount.get(block).copied();
    if let Some(c) = callsite_c {
        st.prof_bcount.insert(cont, c);
    }
    let callee_entry = st.prof.get(callee).and_then(|p| p.blocks.first().copied());
    let scale = |x: u64| match (callsite_c, callee_entry) {
        (Some(c), Some(e)) => ((x as u128 * c as u128) / e.max(1) as u128) as u64,
        _ => x,
    };

    let src = &tags.src;
    let mut map = IrMapping::bounded(tags.vb, tags.ob, tags.bb);
    clone_blocks_into(src, region, ctx, rw, &mut map);
    // Region ids are `Ptr<BasicBlock>`s in the source's block space: every
    // record copied below must remap through `map` so a tag anchored at
    // the callee's own entry (or a nested clone's) resolves inside the
    // clone — verbatim copies point at blocks the caller's `region_blocks`
    // never maps, and licm's ancestry walk dead-ends on them.
    let remap = |r: Ptr<BasicBlock>| map.lookup_block(r).unwrap_or(r);
    for &(b, m) in &tags.blocks {
        let nb = map.lookup_block(b).unwrap();
        if m & B_REGION != 0 {
            st.region_blocks.insert(nb, remap(st.region_blocks[b]));
        }
        if m & B_PARENT != 0 {
            st.region_parent.insert(nb, remap(st.region_parent[b]));
        }
        if m & B_BCOUNT != 0 {
            st.prof_bcount.insert(nb, scale(st.prof_bcount[b]));
        }
    }
    for &(v, m) in &tags.vals {
        let Some(nv) = map.lookup_value(v) else {
            continue;
        };
        if m & V_ALLOCAS != 0 {
            st.allocas.insert(nv, st.allocas[v]);
        }
        if m & V_PROMOTED != 0 {
            st.promoted.insert(nv, st.promoted[v]);
        }
        if m & V_CONSTS != 0 {
            st.consts.insert(nv, st.consts[v].clone());
        }
        if m & V_RFROZEN != 0 {
            let f = st.region_frozen[v]
                .iter()
                .map(|&(s, b, r)| (s, b, remap(r)))
                .collect();
            st.region_frozen.insert(nv, f);
        }
        if m & V_RNOALIAS != 0 {
            st.region_noalias.insert(nv, st.region_noalias[v].clone());
        }
        if m & V_RFROZEN_OP != 0 {
            let f = st.region_frozen_op[v]
                .iter()
                .map(|&(b, r)| (b, remap(r)))
                .collect();
            st.region_frozen_op.insert(nv, f);
        }
    }
    // History node for calls cloned out of this body, pushed on first use.
    let parent = st.inline_hist.get(call).copied().unwrap_or(u32::MAX);
    let mut hist_node = None;
    for &(op, m) in &tags.ops {
        let Some(new) = map.lookup_op(op) else {
            continue;
        };
        if m & OP_CALLS != 0 {
            let node = *hist_node.get_or_insert_with(|| {
                st.inline_hist_nodes.push((callee, parent));
                st.inline_hist_nodes.len() as u32 - 1
            });
            st.inline_hist.insert(new, node);
            st.calls.insert(new, st.calls[op].clone());
            if let Some(&(lp, catch)) = st.invokes.get(op) {
                st.invokes
                    .insert(new, (map.lookup_block(lp).unwrap(), catch));
            } else if let Some(u) = unwind {
                st.invokes.insert(new, u);
            }
        }
        if m & OP_INTRINSICS != 0 {
            st.intrinsics.insert(new, st.intrinsics[op].clone());
        }
        if m & OP_RMW != 0 {
            st.rmw.insert(new, st.rmw[op].clone());
        }
        if m & OP_EXPECT != 0 {
            st.expect.insert(new, st.expect[op]);
        }
        if m & OP_PROF_EDGES != 0 {
            st.prof_edges
                .insert(new, st.prof_edges[op].iter().map(|&c| scale(c)).collect());
        }
        if m & OP_VOLATILE != 0 {
            st.volatile.insert(new);
        }
        if m & OP_NONNULL != 0 {
            st.nonnull.insert(new);
        }
        if m & OP_INBOUNDS != 0 {
            st.inbounds.insert(new);
        }
        if m & OP_BOOL01 != 0 {
            st.bool01.insert(new);
        }
        if m & OP_FROZEN_LOADS != 0 {
            st.frozen_loads.insert(new, remap(st.frozen_loads[op]));
        }
        if m & OP_RETURN != 0 {
            let r = Operation::get_op::<ReturnOp>(new, ctx).unwrap();
            let rv: Vec<_> = r.retval(ctx).into_iter().collect();
            let br = BrOp::new(ctx, cont, rv).get_operation();
            br.insert_before(ctx, new);
            Operation::erase(new, ctx);
        }
    }
    if let Some(res) = res {
        let ty = res.get_type(ctx);
        BasicBlock::push_argument(cont, ctx, ty);
        let arg = cont.deref(ctx).get_argument(ctx, 0);
        res.replace_all_uses_with(ctx, &arg);
    }
    // Create the branch to the clone before `frozen_args` so its
    // predecessor map sees the callsite edge — the clone entry's sole pred
    // is what lets `store_src` walk back to the args-tuple stores.
    let entry = map.lookup_block(src[0]).unwrap();
    let br = BrOp::new(ctx, entry, args.clone()).get_operation();
    br.insert_before(ctx, call);
    frozen_args(ctx, st, cs, caller, src, &map, &args, block);
    st.calls.remove(call);
    Operation::erase(call, ctx);
    let clones = src.iter().map(|b| map.lookup_block(*b).unwrap());
    (block, std::iter::once(cont).chain(clones).collect())
}

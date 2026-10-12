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
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::context::{ConstVal, State, leaf_arg};
use crate::lower::has_body;
use crate::types::{leaf_range, size_align};

const DEFAULT_LIMIT: usize = 320;
const SMALL_LIMIT: usize = 40;

// Cost-model weights (`PLIRON_INLINE_MODEL`): a callsite is inlined when
// `cost <= threshold`.
/// Each call inside the callee costs this much on top of its own op: inlining
/// moves the call into the caller, it doesn't remove it.
const CALL_OP_W: usize = 3;
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

pub(crate) fn blocks(ctx: &Context, f: Ptr<Operation>) -> Vec<Ptr<BasicBlock>> {
    if f.deref(ctx).regions().next().is_none() {
        panic!("blocks: op has no regions: {}", OpDbg { op: f, ctx });
    }
    f.deref(ctx).get_region(0).deref(ctx).iter(ctx).collect()
}

pub(crate) fn ops(ctx: &Context, b: Ptr<BasicBlock>) -> Vec<Ptr<Operation>> {
    b.deref(ctx).iter(ctx).collect()
}

pub(crate) fn direct_callee<'a>(
    ctx: &Context,
    st: &'a State<'_>,
    op: Ptr<Operation>,
) -> Option<&'a String> {
    let c = Operation::get_op::<CallOp>(op, ctx)?;
    let CallOpCallable::Direct(id) = c.callee(ctx) else {
        return None;
    };
    st.ident_to_sym.get(&id.to_string())
}

/// Legality only — `Some((has_eh, ops, calls))` if `sym` may be inlined at
/// all; profitability is judged per callsite. `has_eh` = it has invokes or
/// reads the exception pointer. `PLIRON_INLINE_EH=0` rejects those callees
/// outright.
fn legal(ctx: &Context, st: &State<'_>, sym: &str) -> Option<(bool, usize, usize)> {
    let f = &st.funcs[sym];
    if f.no_inline || f.linkage == Linkage::Preemptible || !has_body(ctx, f.op) {
        return None;
    }
    let (mut n, mut ncalls, mut eh) = (0, 0, false);
    for b in blocks(ctx, f.op) {
        for op in ops(ctx, b) {
            n += 1;
            let intr = st.intrinsics.get(&op);
            if intr.is_some_and(|s| s.starts_with("llvm.va_") || s.starts_with("pliron.va."))
                || direct_callee(ctx, st, op).is_some_and(|c| c == sym)
            {
                return None;
            }
            eh |= st.invokes.contains_key(&op) || intr.is_some_and(|s| s.starts_with("pliron.eh"));
            ncalls += usize::from(Operation::is_op::<CallOp>(op, ctx));
        }
    }
    if eh && !crate::pass_enabled("PLIRON_INLINE_EH") {
        return None;
    }
    Some((eh, n, ncalls))
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

/// `legal` plus the body scans the score needs. The fixed thresholds only
/// profile callees a devirt/fold multiplier could rescue (as the old `dv` map
/// did); the model profiles every plausibly inlinable body.
fn callee_info(
    ctx: &Context,
    st: &State<'_>,
    sym: &str,
    model: bool,
    limit: usize,
    bonus: usize,
    fold: usize,
) -> Option<CInfo> {
    let (eh, n, ncalls) = legal(ctx, st, sym)?;
    let mut ci = CInfo {
        op: st.funcs[sym].op,
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
    if model || bonus > 1 {
        ci.dvirt = devirt_params(ctx, st.funcs[sym].op);
    }
    if model || fold > 1 {
        ci.conds = cond_params(ctx, st.funcs[sym].op, model);
    }
    if model {
        (ci.mem, ci.fold) = param_uses(ctx, st, st.funcs[sym].op);
    }
    Some(ci)
}

/// Per entry-param use shape: `mem` = loads/stores/mem intrinsics reached
/// through GEP/bitcast chains (a by-ref arg whose actual is a local slot or a
/// constant becomes direct access the cleanup passes can resolve); `fold` =
/// other uses a constant actual can fold. Call/branch/return uses earn
/// neither — calls are priced by `ncalls`, block args don't fold in place.
fn param_uses(ctx: &Context, st: &State<'_>, f: Ptr<Operation>) -> (Vec<u32>, Vec<u32>) {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else {
        return (Vec::new(), Vec::new());
    };
    let nparams = entry.deref(ctx).arguments().count();
    let (mut mem, mut fold) = (vec![0u32; nparams], vec![0u32; nparams]);
    for (i, p) in entry.deref(ctx).arguments().enumerate() {
        let (mut work, mut seen, mut budget) = (vec![p], FxHashSet::default(), 64);
        while let Some(v) = work.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            for u in v.uses(ctx) {
                let o = u.user_op();
                let ui = u.find_index(ctx);
                if (Operation::is_op::<GetElementPtrOp>(o, ctx)
                    || Operation::is_op::<BitcastOp>(o, ctx))
                    && ui == 0
                {
                    if seen.insert(o.deref(ctx).get_result(0)) {
                        work.push(o.deref(ctx).get_result(0));
                    }
                } else if Operation::is_op::<LoadOp>(o, ctx)
                    || Operation::is_op::<AtomicLoadOp>(o, ctx)
                    || ((Operation::is_op::<StoreOp>(o, ctx)
                        || Operation::is_op::<AtomicStoreOp>(o, ctx))
                        && ui != 0)
                    || st
                        .intrinsics
                        .get(&o)
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
fn const_arg(st: &State<'_>, v: &Value) -> bool {
    matches!(
        st.consts.get(v),
        Some(ConstVal::Bits(_) | ConstVal::Zero | ConstVal::Sym { .. })
    )
}

/// Successor blocks, counting invoke landing-pad edges.
fn bsuccs(ctx: &Context, st: &State<'_>, b: Ptr<BasicBlock>) -> Vec<Ptr<BasicBlock>> {
    let mut v = Vec::new();
    for op in ops(ctx, b) {
        v.extend(op.deref(ctx).successors());
        v.extend(st.invokes.get(&op).map(|&(l, _)| l));
    }
    v
}

/// Blocks of `f` that can reach themselves: the callsite-context "in a loop"
/// signal (an inlined body there runs once per iteration, so a little extra
/// code is worth it).
fn loop_blocks(ctx: &Context, st: &State<'_>, f: Ptr<Operation>) -> FxHashSet<Ptr<BasicBlock>> {
    let mut out = FxHashSet::default();
    for b in blocks(ctx, f) {
        let mut seen = FxHashSet::default();
        let mut stack = bsuccs(ctx, st, b);
        while let Some(x) = stack.pop() {
            if x == b {
                out.insert(b);
                break;
            }
            if seen.insert(x) {
                stack.extend(bsuccs(ctx, st, x));
            }
        }
    }
    out
}

/// Parameter indices of `f` that (through loads / GEPs) feed an indirect call's
/// target: a constant address there (a vtable, a fn pointer) lets `devirt`
/// make the call direct once `f` is inlined, as LLVM's inline-cost bonus does.
fn devirt_params(ctx: &Context, f: Ptr<Operation>) -> Vec<usize> {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else {
        return Vec::new();
    };
    let params: Vec<Value> = entry.deref(ctx).arguments().collect();
    let mut out = Vec::new();
    for &b in &bs {
        for op in ops(ctx, b) {
            let Some(c) = Operation::get_op::<CallOp>(op, ctx) else {
                continue;
            };
            let CallOpCallable::Indirect(mut p) = c.callee(ctx) else {
                continue;
            };
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
                        p = d.deref(ctx).get_operand(0);
                    }
                    _ => break,
                }
            }
        }
    }
    out
}

/// Parameter indices of `f` that reach a conditional branch's condition
/// through a short def chain: a scalar-constant argument there folds the
/// branch in the inlined clone, so the callsite gets a size-limit bonus.
/// `incl_switch` (the cost model) also counts `switch` conditions.
fn cond_params(ctx: &Context, f: Ptr<Operation>, incl_switch: bool) -> Vec<usize> {
    let bs = blocks(ctx, f);
    let Some(&entry) = bs.first() else {
        return Vec::new();
    };
    let params: Vec<Value> = entry.deref(ctx).arguments().collect();
    let mut out = Vec::new();
    for &b in &bs {
        for op in ops(ctx, b) {
            if !Operation::is_op::<CondBrOp>(op, ctx)
                && !(incl_switch && Operation::is_op::<SwitchOp>(op, ctx))
            {
                continue;
            }
            let mut work = vec![op.deref(ctx).get_operand(0)];
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
                    work.push(d.deref(ctx).get_operand(oi));
                }
            }
        }
    }
    out
}

fn call_counts(ctx: &Context, st: &State<'_>) -> FxHashMap<String, usize> {
    let mut n: FxHashMap<String, usize> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op) || st.dead_fns.contains(sym) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
                if let Some(c) = direct_callee(ctx, st, op) {
                    *n.entry(c.clone()).or_default() += 1;
                }
            }
        }
    }
    n
}

/// Marks local fns that nothing references any more (typically fully inlined)
/// so lowering skips them. Iterates because a dead fn's calls don't count.
/// `PLIRON_DEADFN=0` disables it.
pub fn dead_fns(ctx: &Context, st: &mut State<'_>) {
    let taken = crate::lower::address_taken(ctx, st);
    loop {
        let counts = call_counts(ctx, st);
        let dead: Vec<String> = st
            .funcs
            .iter()
            .filter(|(n, f)| {
                f.linkage == Linkage::Local
                    && has_body(ctx, f.op)
                    && !st.dead_fns.contains(n.as_str())
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
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!(
            "deadfn {}: {} of {} fns not lowered",
            st.cgu,
            st.dead_fns.len(),
            st.funcs.len()
        );
    }
}

/// Functions with bodies, callees before callers (cycles broken arbitrarily).
fn post_order(ctx: &Context, st: &State<'_>) -> Vec<String> {
    let mut seen = rustc_data_structures::fx::FxHashSet::default();
    let mut out = Vec::new();
    for root in st.funcs.keys() {
        if !has_body(ctx, st.funcs[root].op) || !seen.insert(root.clone()) {
            continue;
        }
        let mut stack = vec![(root.clone(), callees(ctx, st, root), 0usize)];
        while let Some((sym, cs, i)) = stack.last_mut() {
            if let Some(c) = cs.get(*i).cloned() {
                *i += 1;
                if has_body(ctx, st.funcs[&c].op) && seen.insert(c.clone()) {
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

fn callees(ctx: &Context, st: &State<'_>, sym: &str) -> Vec<String> {
    blocks(ctx, st.funcs[sym].op)
        .into_iter()
        .flat_map(|b| ops(ctx, b))
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
pub fn run(
    ctx: &mut Context,
    st: &mut State<'_>,
    small: bool,
    only: Option<&FxHashSet<Ptr<Operation>>>,
    always_only: bool,
) {
    let limit = std::env::var("PLIRON_INLINE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { SMALL_LIMIT } else { DEFAULT_LIMIT });
    // -O0 still honours `#[inline(always)]` like LLVM's AlwaysInliner: wasm-bindgen
    // needs its describe markers inlined into each monomorphised shim.
    let limit = if always_only { usize::MAX / 16 } else { limit };
    if limit == 0 {
        return;
    }
    // Local fns with one direct call site and no address use get 10x the limit:
    // inlining them removes the out-of-line copy (`PLIRON_INLINE_ONCE=0` disables).
    let once = !always_only && std::env::var("PLIRON_INLINE_ONCE").map_or(!small, |v| v != "0");
    let single: FxHashSet<String> = if once {
        let taken = crate::lower::address_taken(ctx, st);
        let counts = call_counts(ctx, st);
        st.funcs
            .iter()
            .filter(|(n, f)| {
                f.linkage == Linkage::Local
                    && !taken.contains(n.as_str())
                    && counts.get(n.as_str()) == Some(&1)
            })
            .map(|(n, _)| n.clone())
            .collect()
    } else {
        FxHashSet::default()
    };
    // PLIRON_INLINE_BU=0: one flat round, non-invoke sites only (the old heuristic).
    let bottom_up = !always_only && crate::pass_enabled("PLIRON_INLINE_BU");
    // EH-invoke inlining grows code ~14% (regex-syntax), so size-sensitive output skips it.
    let eh_invoke = std::env::var("PLIRON_INLINE_EH_INVOKE").map_or(!small, |v| v != "0");
    let cap: usize = std::env::var("PLIRON_INLINE_CALLER_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    // Callees up to `bonus`x the limit whose indirect call target comes from a
    // parameter are inlined where that argument is a constant address
    // (`PLIRON_INLINE_DEVIRT=<x>`; e.g. hashbrown's `find_or_find_insert_index_inner`
    // taking `&mut dyn FnMut`).
    let bonus: usize = std::env::var("PLIRON_INLINE_DEVIRT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { 0 } else { 4 });
    // Similarly, callees up to `fold`x the limit whose param reaches a branch
    // condition are inlined where that argument is a scalar constant, which
    // folds the branch in the clone (`PLIRON_INLINE_FOLD=<x>`).
    let fold: usize = std::env::var("PLIRON_INLINE_FOLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { 0 } else { 2 });
    // `#[inline]` callees get up to `hint`x the limit (`PLIRON_INLINE_HINT`):
    // rustc's hint means "small or hot leaf" — std's `hash_one`/iterator
    // adapters sit just over the base limit but must inline to match LLVM.
    let hint: usize = std::env::var("PLIRON_INLINE_HINT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if small { 0 } else { 4 });
    // The profitability model (`PLIRON_INLINE_MODEL=0` gives the fixed
    // thresholds). `small` stays on the legacy rules: wasm/-Os/-Oz must not
    // grow code on heuristics tuned for speed.
    let model = crate::pass_enabled("PLIRON_INLINE_MODEL") && !small;
    let verbose = std::env::var("PLIRON_INLINE_VERBOSE").map_or(false, |v| v != "0");
    // Non-free ops a single caller may absorb (`PLIRON_INLINE_GROWTH`; the
    // model only). `cap` still bounds the caller's absolute size.
    let growth: usize = std::env::var("PLIRON_INLINE_GROWTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| limit.saturating_mul(GROWTH_MULT));
    let mut infos: FxHashMap<String, CInfo> = FxHashMap::default();
    macro_rules! add_info {
        ($sym:expr) => {
            if let Some(ci) = callee_info(ctx, st, $sym, model, limit, bonus, fold) {
                infos.insert($sym.to_string(), ci);
            }
        };
    }
    let order: Vec<String> = if bottom_up {
        post_order(ctx, st)
    } else {
        for sym in st.funcs.keys() {
            if always_only && !st.funcs[sym].always_inline {
                continue;
            }
            add_info!(sym);
        }
        st.funcs
            .iter()
            .filter(|(_, f)| has_body(ctx, f.op))
            .map(|(s, _)| s.clone())
            .collect()
    };
    let mut rw = IRRewriter::<DummyListener>::default();
    for sym in order {
        let caller = st.funcs[&sym].op;
        let mut size: usize = blocks(ctx, caller).iter().map(|&b| ops(ctx, b).len()).sum();
        let mut grown = 0usize;
        let mut seen: FxHashSet<Ptr<Operation>> = FxHashSet::default();
        // Inlining exposes nested callsites the scan hadn't seen: e.g.
        // `HashMap::get` brings `hash_one` inside the caller's loop, where
        // the in-loop bonus may now accept it. Rescan until a round finds
        // nothing worth inlining (`size`/`grown`/`cap` bound the fixpoint).
        for _round in 0..32 {
            let mut sites = Vec::new();
            let mut loops: Option<FxHashSet<Ptr<BasicBlock>>> = None;
            for b in blocks(ctx, caller) {
                for op in ops(ctx, b) {
                    if !seen.insert(op) {
                        continue;
                    }
                    if !bottom_up && st.invokes.contains_key(&op) {
                        continue;
                    }
                    if only.is_some_and(|o| !o.contains(&op)) {
                        continue;
                    }
                    let Some(cs) = direct_callee(ctx, st, op) else {
                        continue;
                    };
                    let Some(ci) = infos.get(cs) else {
                        if verbose {
                            eprintln!("inline {}: {sym} -> {cs}: reject (ineligible)", st.cgu);
                        }
                        continue;
                    };
                    let f = &st.funcs[cs];
                    // Free inlines: the out-of-line copy disappears
                    // (single-callsite locals) or rustc demanded it.
                    let free = f.always_inline || (once && single.contains(cs));
                    let mut t = if free {
                        limit.saturating_mul(10)
                    } else {
                        limit
                    };
                    let args = Operation::get_op::<CallOp>(op, ctx)
                        .map(|c| c.args(ctx))
                        .unwrap_or_default();
                    // A constant address feeding an indirect-call param lets
                    // `devirt` make the call direct; a scalar constant feeding a
                    // branch condition folds it. (The old `dv` map's bonuses.)
                    if bonus > 1
                        && ci.dvirt.iter().any(|&i| {
                            args.get(i).is_some_and(|a| {
                                matches!(st.consts.get(a), Some(ConstVal::Sym { .. }))
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
                    let mut cost = ci.n;
                    if model {
                        cost = ci.n.saturating_add(ci.ncalls.saturating_mul(CALL_OP_W));
                        let mut save = 0usize;
                        for (i, a) in args.iter().enumerate() {
                            if const_arg(st, a) {
                                save = save.saturating_add(
                                    FOLD_USE * ci.fold.get(i).copied().unwrap_or(0) as usize,
                                );
                            }
                            if const_arg(st, a) || st.allocas.contains_key(a) {
                                save = save.saturating_add(
                                    MEM_USE * ci.mem.get(i).copied().unwrap_or(0) as usize,
                                );
                            }
                        }
                        t = t.saturating_add(save);
                        if f.cold {
                            cost = cost.saturating_mul(COLD_W);
                        }
                        if loops
                            .get_or_insert_with(|| loop_blocks(ctx, st, caller))
                            .contains(&b)
                        {
                            // `PLIRON_INLINE_LOOP` = bonus in limit/4 units
                            // (default 2 = limit/2, the historical bonus).
                            let lb: usize = std::env::var("PLIRON_INLINE_LOOP")
                                .ok()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(2);
                            t = t.saturating_add(limit * lb / 4);
                        }
                        t = t.min(limit.saturating_mul(MAX_T));
                        if free {
                            // Net global growth is ~0 — the callee's out-of-line
                            // copy dies — so nested calls and coldness aren't
                            // charged; `cap` still bounds the caller's size.
                            cost = ci.n;
                        }
                    }
                    // A callee with landing pads may go into an invoke whose pad is a
                    // cleanup: its `_Unwind_Resume` becomes an invoke of that pad. Not
                    // into a catch pad: unwinder phase 1 would never see that catch.
                    let eh_block = ci.eh
                        && st
                            .invokes
                            .get(&op)
                            .is_some_and(|&(_, catch)| catch || !eh_invoke);
                    let why = if cost > t {
                        "reject (score)"
                    } else if eh_block {
                        "reject (eh)"
                    } else if ci.op == caller || st.calls[&op].fn_ty != f.ty {
                        "reject (self/fnty)"
                    } else if size + ci.n > cap {
                        "reject (cap)"
                    } else if model && !free && grown + ci.n > growth {
                        "reject (growth)"
                    } else {
                        if model && !free {
                            grown += ci.n;
                        }
                        size += ci.n;
                        sites.push((op, ci.op));
                        "accept"
                    };
                    if verbose {
                        eprintln!(
                            "inline {}: {sym} -> {cs}: n={} calls={} cost={cost} t={t}: {why}",
                            st.cgu, ci.n, ci.ncalls
                        );
                    }
                }
            }
            if sites.is_empty() {
                break;
            }
            for (call, callee) in sites {
                inline_call(ctx, st, &mut rw, call, callee);
            }
        }
        if bottom_up {
            add_info!(&sym);
        }
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        left_stats(ctx, st, &infos);
    }
}

/// Indirect calls whose callee is a constant function address (a vtable slot
/// folded by `constload`, e.g. hashbrown's `&mut dyn FnMut` probe callback)
/// become direct calls. Returns the rewritten calls, for a second inline round.
pub fn devirt(ctx: &mut Context, st: &mut State<'_>) -> FxHashSet<Ptr<Operation>> {
    let mut out = FxHashSet::default();
    let fns: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    for f in fns {
        for b in blocks(ctx, f) {
            for c in ops(ctx, b) {
                let Some(call) = Operation::get_op::<CallOp>(c, ctx) else {
                    continue;
                };
                let CallOpCallable::Indirect(p) = call.callee(ctx) else {
                    continue;
                };
                let Some(ConstVal::Sym { sym, off: 0 }) = st.consts.get(&p) else {
                    continue;
                };
                let (Some(fi), Some(info), Some(id)) = (
                    st.funcs.get(sym),
                    st.calls.get(&c),
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
                    let (old, new) = (c.deref(ctx).get_result(0), nc.deref(ctx).get_result(0));
                    old.replace_all_uses_with(ctx, &new);
                }
                let info = st.calls.remove(&c).unwrap();
                st.calls.insert(nc, info);
                if let Some(u) = st.invokes.remove(&c) {
                    st.invokes.insert(nc, u);
                }
                if let Some(e) = st.expect.remove(&c) {
                    st.expect.insert(nc, e);
                }
                if st.last_call == Some(c) {
                    st.last_call = Some(nc);
                }
                Operation::erase(c, ctx);
                out.insert(nc);
                if std::env::var_os("PLIRON_STATS_DEVIRT").is_some() {
                    let caller = st
                        .funcs
                        .iter()
                        .find(|(_, x)| x.op == f)
                        .map(|(n, _)| n.clone());
                    eprintln!("devirt {caller:?} -> {sym}");
                }
            }
        }
    }
    if std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("devirt {}: {} calls", st.cgu, out.len());
    }
    out
}

/// Why direct calls to defined functions were left in place (`PLIRON_STATS`).
fn left_stats(ctx: &Context, st: &State<'_>, infos: &FxHashMap<String, CInfo>) {
    let mut why: FxHashMap<&str, usize> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
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
                } else if let Some(ci) = infos.get(cs) {
                    if ci.eh && st.invokes.contains_key(&op) {
                        "eh callee at invoke (catch pad / EH_INVOKE=0)"
                    } else if st.calls[&op].fn_ty != g.ty {
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
    eprintln!("inline left {}: {why:?}", st.cgu);
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
    let scan = std::env::var_os("PLIRON_FROZEN_SCAN").is_some();
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
                    eprintln!("extract_leaf: no idx for {v:?} in {b:?}");
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
                        eprintln!("extract_leaf: non-branch pred {t:?} of {b:?}");
                    }
                    return None;
                };
                let Some(&w) = ops.get(i) else {
                    if scan {
                        eprintln!("extract_leaf: arg {i} missing in {t:?} ops for {b:?}");
                    }
                    return None;
                };
                if w == v {
                    continue;
                }
                let l = extract_leaf(ctx, preds, w, sub, depth + 1)?;
                if uniq.is_some_and(|u| u != l) {
                    if scan {
                        eprintln!(
                            "extract_leaf: arg {v:?} of {b:?} merges {uniq:?} with {l:?}"
                        );
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
            let (s, n, _) =
                leaf_range(ctx, op.deref(ctx).get_operand(0).get_type(ctx), &idx);
            if sub >= s && sub < s + n {
                sub -= s;
                v = op.deref(ctx).get_operand(1);
            } else {
                v = op.deref(ctx).get_operand(0);
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
    st: &State<'_>,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    load: Ptr<Operation>,
    mem: &mut FxHashSet<Ptr<Operation>>,
) -> Option<Value> {
    let scan = std::env::var_os("PLIRON_FROZEN_SCAN").is_some();
    let fail = |why: &str| -> Option<Value> {
        if scan {
            eprintln!("store_src: {why} for {load:?}");
        }
        None
    };
    if st.volatile.contains(&load) {
        return fail("volatile");
    }
    let addr = load.deref(ctx).get_operand(0);
    let Some((lb, ll, loff)) = ptr_base(ctx, st, preds, addr, mem) else {
        return fail("ptr_base");
    };
    let Some(loff) = loff else {
        return fail("dynoff");
    };
    let lsz = size_align(ctx, load.deref(ctx).get_result(0).get_type(ctx)).0 as i64;
    let Some(b) = load.deref(ctx).get_parent_block() else {
        return fail("noblock");
    };
    let pos = ops(ctx, b).iter().position(|o| *o == load);
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
    st: &State<'_>,
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
        for (oi, &o) in block_ops[..start].iter().enumerate().rev().take(400) {
            // `llvm.memcpy/memmove(dst, src, n)`: a load fully inside the
            // copied range reads the same bytes the *source* held just
            // before the copy — resolve the store to that source slot.
            if let Some(name) = st.intrinsics.get(&o) {
                if name != "llvm.memcpy" && name != "llvm.memmove" {
                    if name.starts_with("llvm.mem") {
                        return None;
                    }
                    continue;
                }
                let od = o.deref(ctx);
                let Some(n) = crate::sroa::const_int(ctx, st, od.get_operand(2)) else {
                    return None;
                };
                let Some((db, dl, doff)) =
                    ptr_base(ctx, st, preds, od.get_operand(0), mem)
                else {
                    return None;
                };
                let Some(doff) = doff else { return None };
                if db == lb && dl == ll {
                    let (lo, hi) = (doff, doff + n as i64);
                    if loff >= lo && loff + lsz <= hi {
                        let Some((sb2, sl2, soff2)) =
                            ptr_base(ctx, st, preds, od.get_operand(1), mem)
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
            if Operation::is_op::<StoreOp>(o, ctx) && !st.volatile.contains(&o) {
                let od = o.deref(ctx);
                let (sb, sl, soff) = ptr_base(ctx, st, preds, od.get_operand(1), mem)?;
                let soff = soff?;
                let ssz = size_align(ctx, od.get_operand(0).get_type(ctx)).0 as i64;
                if sb == lb && sl == ll {
                    if soff == loff && ssz == lsz {
                        return Some(od.get_operand(0));
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
                    eprintln!(
                        "store_src: writer {} for {load:?}",
                        OpDbg { op: o, ctx }
                    );
                }
                return None;
            }
        }
        // No decisive store here — follow the predecessors.
        let mut pr: Vec<Ptr<BasicBlock>> = Vec::new();
        let Some(pr_ops) = preds.get(&b) else {
            if scan {
                eprintln!("store_src: no preds {b:?} for {load:?}");
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
                        eprintln!("store_src: merge diff {} vs {}", d(ctx, w), d(ctx, v));
                    }
                    eq
                })
            {
                if scan {
                    eprintln!(
                        "store_src: merge {b:?} pred {p:?} -> {pv:?} vs {v:?} for {load:?}"
                    );
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
    st: &State<'_>,
    preds: &FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>>,
    v1: Value,
    v2: Value,
    mem: &mut FxHashSet<Ptr<Operation>>,
) -> bool {
    fn norm(
        ctx: &Context,
        st: &State<'_>,
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
                v = op.deref(ctx).get_operand(0);
            } else if Operation::is_op::<BitcastOp>(op, ctx)
                || Operation::is_op::<FreezeOp>(op, ctx)
                || Operation::is_op::<AddrSpaceCastOp>(op, ctx)
            {
                v = op.deref(ctx).get_operand(0);
            } else if Operation::is_op::<ExtractValueOp>(op, ctx) {
                let idx = Operation::get_op::<ExtractValueOp>(op, ctx)
                    .unwrap()
                    .indices(ctx);
                let agg = op.deref(ctx).get_operand(0);
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
    if n1 != n2 && std::env::var_os("PLIRON_FROZEN_SCAN").is_some() {
        let d = |v: Value| {
            v.defining_op()
                .map(|o| OpDbg { op: o, ctx }.to_string())
                .unwrap_or_else(|| format!("{v:?}"))
        };
        eprintln!("same_ptr: {n1:?} ({}) vs {n2:?} ({})", d(n1.0), d(n2.0));
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
    st: &State<'_>,
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
            && !st.region_frozen.contains_key(&v)
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
            off = off.and_then(|o| {
                crate::sroa::gep_offset(ctx, st, op).and_then(|d| o.checked_add(d))
            });
            v = op.deref(ctx).get_operand(0);
        } else if Operation::is_op::<BitcastOp>(op, ctx)
            || Operation::is_op::<FreezeOp>(op, ctx)
            || Operation::is_op::<AddrSpaceCastOp>(op, ctx)
        {
            v = op.deref(ctx).get_operand(0);
        } else if Operation::is_op::<ExtractValueOp>(op, ctx) {
            // `extractvalue agg, idx` of a pointer leaf: decomposing `agg`
            // (`insertvalue` chains) keeps provenance; an opaque aggregate
            // returns the leaf position so callers can match a recorded
            // frozen leaf of the aggregate.
            let idx = Operation::get_op::<ExtractValueOp>(op, ctx)
                .unwrap()
                .indices(ctx);
            let agg = op.deref(ctx).get_operand(0);
            let (s, n, _) = leaf_range(ctx, agg.get_type(ctx), &idx);
            if n != 1 {
                return Some((v, None, off));
            }
            // Same rule as the arg case: don't decompose a recorded
            // aggregate arg past its own leaf record.
            if st
                .region_frozen
                .get(&agg)
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
fn frozen_of(
    ctx: &Context,
    st: &State<'_>,
    caller_frz: &[(usize, u64)],
    fentry: Option<Ptr<BasicBlock>>,
    base: Value,
    bl: Option<usize>,
) -> Option<(u64, Ptr<BasicBlock>)> {
    if let Some(&(_, bytes, r)) = st
        .region_frozen
        .get(&base)
        .and_then(|rs| rs.iter().find(|&&(s, _, _)| bl.unwrap_or(0) == s as usize))
    {
        return Some((bytes, r));
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
    st: &mut State<'_>,
    callee: Ptr<Operation>,
    src: &[Ptr<BasicBlock>],
    map: &IrMapping,
    call_args: &[Value],
    call_block: Ptr<BasicBlock>,
) {
    if !crate::pass_enabled("PLIRON_FROZEN_INLINE") {
        return;
    }
    let dbg = std::env::var_os("PLIRON_FROZEN_DEBUG").is_some();
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
        .region_blocks
        .get(&call_block)
        .copied()
        .or(fentry)
        .unwrap_or(entry);
    st.region_parent.insert(entry, parent);
    for &b in src {
        if let Some(nb) = map.lookup_block(b) {
            // Nested clones inside `src` keep their own (remapped) region;
            // only unclaimed blocks belong to this clone's region.
            st.region_blocks.entry(nb).or_insert(entry);
        }
    }
    let Some(f) = st.funcs.values().find(|f| f.op == callee) else {
        if dbg {
            eprintln!("frozen_args: callee {callee:?} not in funcs");
        }
        return;
    };
    let fname = st
        .funcs
        .iter()
        .find(|(_, x)| x.op == callee)
        .map(|(n, _)| n.clone());
    if dbg {
        eprintln!(
            "frozen_args: na={:?} frz={:?} callee={fname:?} entry={entry:?}",
            f.noalias.len(),
            f.frozen.len()
        );
    }
    // CFG edges of the whole containing function, for `ptr_base`'s
    // block-arg resolution: a load's pointer (or a caller operand bound to
    // a frozen arg) is often phi-forwarded through args of *enclosing*
    // clone blocks — `partition`'s pivot reaches `lt` callsites that way.
    let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>> =
        FxHashMap::default();
    if !f.frozen.is_empty()
        && let Some(region) = call_block.deref(ctx).get_parent_region()
    {
        for b in region.deref(ctx).iter(ctx) {
            for op in ops(ctx, b) {
                for (si, s) in op.deref(ctx).successors().enumerate() {
                    preds.entry(s).or_default().push((op, si));
                }
            }
        }
    }
    // `(bytes, region)` for a base resolved at leaf `bl`: an enclosing
    // clone-arg record first; else a frozen param of the containing
    // function itself — the outermost region, anchored at its entry.
    let caller_frz: Vec<(usize, u64)> = fop
        .and_then(|o| st.funcs.values().find(|x| x.op == o))
        .map(|x| x.frozen.clone())
        .unwrap_or_default();
    // Caller-side operands bound to each entry arg, for region inheritance.
    // `inline_call` branches to `entry` with `args` positionally, so the
    // entry arg at index k is bound to `call_args[k]`.
    let eargs: Vec<Value> = entry.deref(ctx).arguments().collect();
    for &(leaf, bytes) in &f.frozen {
        let Some((a, sub)) = leaf_arg(ctx, entry, leaf) else {
            if dbg {
                eprintln!("frozen_args: leaf {leaf} unresolved");
            }
            continue;
        };
        let cv = eargs
            .iter()
            .position(|&x| x == a)
            .and_then(|k| call_args.get(k).copied());
        // `a` may cover several leaves (comparator tuples pack both refs
        // into one operand) — resolve the `sub`-th leaf of the operand.
        let rb = cv
            .and_then(|v| extract_leaf(ctx, &preds, v, sub, 0))
            .and_then(|v| ptr_base(ctx, st, &preds, v, &mut FxHashSet::default()));
        // Inherit the enclosing region only when the operand's pointee
        // provably sits inside the outer frozen range — `a = pivot + 8`
        // resolves to pivot's base but points *past* its 8 frozen bytes.
        // The clone-entry region is always valid: `a`'s own `readonly`
        // contract holds for this call regardless.
        let inh = rb.and_then(|(b, l, off)| {
            frozen_of(ctx, st, &caller_frz, fentry, b, l)
                .filter(|&(rb_, _)| {
                    off.is_some_and(|o| o >= 0 && o + bytes as i64 <= rb_ as i64)
                })
                .map(|(_, r)| r)
        });
        let region = inh.unwrap_or(entry);
        if dbg && std::env::var_os("PLIRON_FROZEN_SCAN").is_some() {
            eprintln!(
                "frozen_args: leaf {leaf} sub {sub} entry {entry:?} inh={inh:?} rb={rb:?} cv={cv:?}"
            );
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
            eprintln!(
                "frozen_args: record leaf {leaf} sub {sub} bytes {bytes} region={region:?} callee={fname:?} cv={cv:?} rb={rb:?} {rbs} | {cvs} || {cas}"
            );
        }
        st.region_frozen
            .entry(a)
            .or_default()
            .push((sub as u32, bytes, region));
    }
    // Loads inside the clone that provably read inside a frozen pointee:
    // they may hoist within the region without a clobber scan.
    for &b in src {
        let Some(nb) = map.lookup_block(b) else {
            continue;
        };
        for op in ops(ctx, nb) {
            if !Operation::is_op::<LoadOp>(op, ctx) {
                continue;
            }
            let ptr = op.deref(ctx).get_operand(0);
            let Some((base, bl, Some(off))) =
                ptr_base(ctx, st, &preds, ptr, &mut FxHashSet::default())
            else {
                continue;
            };
            let Some((bytes, region)) = frozen_of(ctx, st, &caller_frz, fentry, base, bl)
            else {
                if dbg && std::env::var_os("PLIRON_FROZEN_SCAN").is_some() {
                    let bs = base
                        .defining_op()
                        .map(|o| OpDbg { op: o, ctx }.to_string())
                        .unwrap_or_default();
                    eprintln!(
                        "frozen_args: miss {op:?} base {base:?} leaf {bl:?} off {off} {bs}"
                    );
                }
                continue;
            };
            let sz = size_align(ctx, op.deref(ctx).get_result(0).get_type(ctx)).0 as i64;
            if off < 0 || off + sz > bytes as i64 {
                continue;
            }
            st.frozen_loads.insert(op, region);
            if dbg {
                eprintln!("frozen_args: load {op:?} base {base:?} off {off} region {region:?}");
            }
        }
    }
    for &(leaf, _, _) in &f.noalias {
        let Some((a, sub)) = leaf_arg(ctx, entry, leaf) else {
            continue;
        };
        if dbg {
            eprintln!("frozen_args: na record leaf {leaf} a={a:?} entry={entry:?}");
        }
        st.region_noalias.entry(a).or_default().push(sub as u32);
    }
}

fn inline_call(
    ctx: &mut Context,
    st: &mut State<'_>,
    rw: &mut IRRewriter<DummyListener>,
    call: Ptr<Operation>,
    callee: Ptr<Operation>,
) {
    let block = call.deref(ctx).get_parent_block().unwrap();
    let region = block.deref(ctx).get_parent_region().unwrap();
    let args = Operation::get_op::<CallOp>(call, ctx).unwrap().args(ctx);
    let res = (call.deref(ctx).get_num_results() > 0).then(|| call.deref(ctx).get_result(0));
    let unwind = st.invokes.remove(&call);
    let cont = rw.split_block(ctx, block, OpInsertionPoint::AfterOperation(call), None);

    let src = blocks(ctx, callee);
    let mut map = IrMapping::new();
    clone_blocks_into(&src, region, ctx, rw, &mut map);
    // Region ids are `Ptr<BasicBlock>`s in the source's block space: every
    // record copied below must remap through `map` so a tag anchored at
    // the callee's own entry (or a nested clone's) resolves inside the
    // clone — verbatim copies point at blocks the caller's `region_blocks`
    // never maps, and licm's ancestry walk dead-ends on them.
    let remap = |r: Ptr<BasicBlock>| map.lookup_block(r).unwrap_or(r);
    for &b in &src {
        let nb = map.lookup_block(b).unwrap();
        if let Some(&pr) = st.region_blocks.get(&b) {
            st.region_blocks.insert(nb, remap(pr));
        }
        if let Some(&pp) = st.region_parent.get(&b) {
            st.region_parent.insert(nb, remap(pp));
        }
        for (a, na) in b
            .deref(ctx)
            .arguments()
            .zip(nb.deref(ctx).arguments())
        {
            if let Some(f) = st.region_frozen.get(&a).cloned() {
                st.region_frozen.insert(
                    na,
                    f.into_iter()
                        .map(|(s, b, r)| (s, b, remap(r)))
                        .collect(),
                );
            }
            if let Some(f) = st.region_noalias.get(&a).cloned() {
                st.region_noalias.insert(na, f);
            }
        }
        for (op, new) in ops(ctx, b).into_iter().zip(ops(ctx, nb)) {
            if let Some(c) = st.calls.get(&op).cloned() {
                st.calls.insert(new, c);
                if let Some(&(lp, catch)) = st.invokes.get(&op) {
                    st.invokes
                        .insert(new, (map.lookup_block(lp).unwrap(), catch));
                } else if let Some(u) = unwind {
                    st.invokes.insert(new, u);
                }
            }
            if let Some(s) = st.intrinsics.get(&op).cloned() {
                st.intrinsics.insert(new, s);
            }
            if let Some(r) = st.rmw.get(&op).copied() {
                st.rmw.insert(new, r);
            }
            if let Some(e) = st.expect.get(&op).copied() {
                st.expect.insert(new, e);
            }
            if st.volatile.contains(&op) {
                st.volatile.insert(new);
            }
            if st.nonnull.contains(&op) {
                st.nonnull.insert(new);
            }
            if st.inbounds.contains(&op) {
                st.inbounds.insert(new);
            }
            if st.bool01.contains(&op) {
                st.bool01.insert(new);
            }
            if let Some(r) = st.frozen_loads.get(&op).copied() {
                st.frozen_loads.insert(new, remap(r));
            }
            let n = op.deref(ctx).get_num_results();
            for i in 0..n {
                let (v, nv) = (op.deref(ctx).get_result(i), new.deref(ctx).get_result(i));
                if let Some(a) = st.allocas.get(&v).copied() {
                    st.allocas.insert(nv, a);
                }
                if let Some(t) = st.promoted.get(&v).copied() {
                    st.promoted.insert(nv, t);
                }
                if let Some(c) = st.consts.get(&v).cloned() {
                    st.consts.insert(nv, c);
                }
            }
            if let Some(r) = Operation::get_op::<ReturnOp>(new, ctx) {
                let rv: Vec<_> = r.retval(ctx).into_iter().collect();
                let br = BrOp::new(ctx, cont, rv).get_operation();
                br.insert_before(ctx, new);
                Operation::erase(new, ctx);
            }
        }
    }
    if let Some(res) = res {
        let ty = res.get_type(ctx);
        BasicBlock::push_argument(cont, ctx, ty);
        let arg = cont.deref(ctx).get_argument(0);
        res.replace_all_uses_with(ctx, &arg);
    }
    // Create the branch to the clone before `frozen_args` so its
    // predecessor map sees the callsite edge — the clone entry's sole pred
    // is what lets `store_src` walk back to the args-tuple stores.
    let entry = map.lookup_block(src[0]).unwrap();
    let br = BrOp::new(ctx, entry, args.clone()).get_operation();
    br.insert_before(ctx, call);
    frozen_args(ctx, st, callee, &src, &map, &args, block);
    st.calls.remove(&call);
    Operation::erase(call, ctx);
}

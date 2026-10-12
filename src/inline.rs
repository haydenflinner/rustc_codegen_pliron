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
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::irbuild::cloning::{IrMapping, clone_blocks_into};
use pliron::irbuild::inserter::OpInsertionPoint;
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::{IRRewriter, Rewriter};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::OpDbg;
use pliron::operation::Operation;
use pliron::r#type::{Typed, TypedHandle};
use pliron::value::Value;
use pliron_llvm::ops::{
    AtomicLoadOp, AtomicStoreOp, BitcastOp, BrOp, CallOp, CondBrOp, GetElementPtrOp, IndirectBrOp,
    LoadOp, ReturnOp, StoreOp, SwitchOp, UnreachableOp,
};
use pliron_llvm::types::FuncType;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::context::{ConstVal, State};
use crate::lower::has_body;

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
        let mut sites = Vec::new();
        let mut size: usize = blocks(ctx, caller).iter().map(|&b| ops(ctx, b).len()).sum();
        let mut grown = 0usize;
        let mut loops: Option<FxHashSet<Ptr<BasicBlock>>> = None;
        for b in blocks(ctx, caller) {
            for op in ops(ctx, b) {
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
                        args.get(i)
                            .is_some_and(|a| matches!(st.consts.get(a), Some(ConstVal::Sym { .. })))
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
                        t = t.saturating_add(limit / 2);
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
        for (call, callee) in sites {
            inline_call(ctx, st, &mut rw, call, callee);
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
    for &b in &src {
        let nb = map.lookup_block(b).unwrap();
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
    let entry = map.lookup_block(src[0]).unwrap();
    let br = BrOp::new(ctx, entry, args).get_operation();
    br.insert_before(ctx, call);
    st.calls.remove(&call);
    Operation::erase(call, ctx);
}

//! Drop block arguments whose incoming values are all one value (or the
//! argument itself) and use that value instead. Inlined returns of a pointer
//! derived from an argument otherwise route alloca addresses through block
//! arguments, which blocks SROA.

use pliron::{
    basic_block::BasicBlock,
    builtin::op_interfaces::BranchOpInterface,
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    op::op_cast,
    operation::Operation,
    value::Value,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt_body;
use crate::inline::blocks;
use crate::state::State;

/// Operand `i` that terminator `t` forwards to successor `si`, if `t` is a
/// branch forwarding exactly `na` operands there. NOT memoizable:
/// `replace_all_uses_with` rewrites operand lists on edges belonging to
/// other blocks' pred lists, so a cached Vec goes stale.
fn succ_operand(ctx: &Context, t: Ptr<Operation>, si: usize, i: usize, na: usize) -> Option<Value> {
    let o = Operation::get_op_dyn(t, ctx);
    let v = op_cast::<dyn BranchOpInterface>(&*o)?.successor_operands(ctx, si);
    (v.len() == na).then(|| v[i])
}

fn defined_in(ctx: &Context, v: Value, b: Ptr<BasicBlock>) -> bool {
    match v.defining_op() {
        Some(op) => op.deref(ctx).get_parent_block() == Some(b),
        None => b.deref(ctx).arguments(ctx).any(|a| a == v),
    }
}

/// Is the clone-entry arg `arg`, fed only `v`, worth keeping as a name of
/// its own for the scoped `noalias` fact (`PLIRON_SCOPED_NOALIAS`)? A held
/// arg hides `v` from every IR pass that follows: `constload` no longer
/// folds a load from a constant (a derived `PartialEq::eq(&CONST, &x)`
/// became a byte-by-byte compare against the constant's memory:
/// pulldown-cmark `Tree::append_text`, 90 -> 173 instructions), SROA sees
/// an alloca's field address leave through a branch operand. So it is
/// held only where `loadfwd::ScopedNa` can use it:
///
/// - the param is written through. A `readonly` one (every leaf in
///   `region_frozen`) is not a candidate there; it folds as it does
///   without the knob. `PLIRON_SCOPED_PHI_RO=1` holds those too (they
///   then reach lowering as `noalias` roots with a frozen region: exact
///   counts pulldown-cmark -0.12%, rust-protobuf +0.92% against folding);
/// - `v` is not an address inside an alloca or a constant (rule 1 there
///   refuses stack slots; nothing writes a constant);
/// - `v` is not the function's own `noalias` param at a constant offset
///   (`scoped_seed` drops those: the root is already sheltered
///   function-wide); for a held `readonly` arg, not the own frozen param.
///
/// What holding the other classes did on rust-html-rewriter was not the
/// fact: an alloca field kept from SROA (`exec_without_attrs` -326k
/// instructions, `memcpy` +201k), a constant kept from `constload` (the
/// benchmark's two closures -166k). `PLIRON_SCOPED_PHI_KEEP` reproduces
/// each.
fn scoped_hold(ctx: &Context, st: &State, arg: Value, v: Value, hold_ro: bool) -> bool {
    use pliron_llvm::ops::{AddrSpaceCastOp, BitcastOp, FreezeOp, GetElementPtrOp};
    let Some(subs) = st.region_noalias.get(arg) else {
        return false;
    };
    let fz = st.region_frozen.get(arg);
    let ro = subs.iter().all(|&s| fz.is_some_and(|rs| rs.iter().any(|&(f, _, _)| f == s)));
    if ro && !hold_ro {
        return false;
    }
    let (mut b, mut konst) = (v, true);
    for _ in 0..16 {
        let Some(op) = b.defining_op() else { break };
        if Operation::is_op::<GetElementPtrOp>(op, ctx) {
            konst &= crate::sroa::gep_offset(ctx, st, op).is_some();
        } else if !(Operation::is_op::<BitcastOp>(op, ctx)
            || Operation::is_op::<FreezeOp>(op, ctx)
            || Operation::is_op::<AddrSpaceCastOp>(op, ctx))
        {
            break;
        }
        b = op.deref(ctx).get_operand(ctx, 0);
    }
    // `PLIRON_SCOPED_PHI_KEEP=<bits>` holds a class anyway (ablation): 1 an
    // alloca's field, 2 a constant, 4 the own `noalias` param under a
    // written-through arg, 8 the own frozen param under a `readonly` one.
    static KEEP: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
        pliron_cfg::env_get("PLIRON_SCOPED_PHI_KEEP").and_then(|v| v.parse().ok()).unwrap_or(0)
    });
    let keep = *KEEP;
    if (st.allocas.contains(b) && keep & 1 == 0) || (st.consts.contains(b) && keep & 2 == 0) {
        return false;
    }
    !(konst && if ro { st.frozen.contains(b) && keep & 8 == 0 } else { st.noalias.contains(b) && keep & 4 == 0 })
}

/// Blocks reachable from the entry, counting invoke landing-pad edges.
fn reachable(ctx: &Context, st: &State, bs: &[Ptr<BasicBlock>]) -> FxHashSet<Ptr<BasicBlock>> {
    let mut seen = FxHashSet::default();
    let mut work = vec![bs[0]];
    while let Some(b) = work.pop() {
        if !seen.insert(b) {
            continue;
        }
        // Invokes are non-terminator CallOps (see `domcheck::succs`).
        for op in b.deref(ctx).iter(ctx) {
            work.extend(st.invokes.get(op).map(|&(l, _)| l));
        }
        if let Some(t) = b.deref(ctx).iter(ctx).last() {
            work.extend(t.deref(ctx).successors(ctx));
        }
    }
    seen
}

// r[impl iropt.phisimp]
/// One function's phisimp; returns the number of block args removed.
pub fn run_fn(ctx: &mut Context, st: &State, f: Ptr<Operation>) -> usize {
    let landing: FxHashSet<Ptr<BasicBlock>> = st.invokes.values().map(|&(b, _)| b).collect();
    run_fn_with(ctx, st, f, &landing)
}

fn run_fn_with(ctx: &mut Context, st: &State, f: Ptr<Operation>, landing: &FxHashSet<Ptr<BasicBlock>>) -> usize {
    let pin_nn = pliron_cfg::env_is("PLIRON_RETNN", "1");
    let pin_na = pliron_cfg::env_is("PLIRON_SCOPED_NOALIAS", "1") && !pliron_cfg::env_is("PLIRON_SCOPED_PHI", "0");
    let pin_all = pin_na && pliron_cfg::env_is("PLIRON_SCOPED_PHI_ALL", "1");
    let pin_ro = pliron_cfg::env_is("PLIRON_SCOPED_PHI_RO", "1");
    let mut n = 0;
    // `reach`/`preds` are loop-invariant: `remove_successor_operand`
    // only shrinks an edge's operand list, never retargets edges, so
    // block adjacency can't change between fixpoint rounds.
    let bs = blocks(ctx, f);
    // Only non-entry block args are ever removed — no args, no work.
    if bs[1..]
        .iter()
        .all(|&b| b.deref(ctx).get_num_arguments() == 0)
    {
        return 0;
    }
    let reach = reachable(ctx, st, &bs);
    let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>> =
        FxHashMap::default();
    for &b in &bs {
        let Some(t) = b.deref(ctx).iter(ctx).last() else {
            continue;
        };
        let succs: Vec<_> = t.deref(ctx).successors(ctx).collect();
        for (si, s) in succs.into_iter().enumerate() {
            preds.entry(s).or_default().push((t, si));
        }
    }
    loop {
        let mut changed = false;
        for &b in &bs[1..] {
            let Some(ps) = preds.get(&b).filter(|_| !landing.contains(&b)) else {
                continue;
            };
            let na = b.deref(ctx).get_num_arguments();
            for i in (0..na).rev() {
                let arg = b.deref(ctx).get_argument(ctx, i);
                let mut uniq = None;
                let ok = ps
                    .iter()
                    .all(|&(t, si)| match succ_operand(ctx, t, si, i, na) {
                        Some(v) if v == arg => true,
                        Some(v) => *uniq.get_or_insert(v) == v,
                        None => false,
                    });
                // `b` is reachable, so some reachable predecessor passes `v`
                // and `v` dominates `b`.
                let Some(v) = uniq.filter(|&v| ok && !defined_in(ctx, v, b)) else {
                    continue;
                };
                // An orphaned pad region (`nounwind` dropped its invoke) is
                // unreachable to `reach`, but folding still can't reference a
                // non-dominating value. An entry-block alloca dominates
                // everything (incl. dead code), so it folds there too —
                // otherwise the dead region's br operands keep the alloca
                // escaping and SROA can never promote it (bench/sieve's
                // `Vec` fields reloaded every inner iteration). Allocas
                // cloned mid-function by the inliner don't dominate the
                // dead region and stay folded-out.
                //
                // The same when `b` has one incoming edge and no path back
                // to the block that edge leaves: that block is then on
                // every path to `b` and ahead of it, reachable or not, and
                // `v` is available at its end. An inlined callee's entry in
                // such a region otherwise keeps its arguments as block
                // params, and one of them can be a caller's local or a
                // field address of one (`&self.alloc` of the `Vec` whose
                // inlined `RawVec` drop sits in the dead unwind path):
                // regex's `cached_state` then kept its scratch `Vec` whole
                // in memory and stored `len` on every push
                // (`tests/parity/statekey.rs`, `PLIRON_PHISIMP_DEADPRED=0`
                // off). With a path back (a loop that lost its entry edge)
                // `v` may be computed from the param itself.
                let dead_single_pred = || {
                    if pliron_cfg::env_is("PLIRON_PHISIMP_DEADPRED", "0") || ps.len() != 1 {
                        return false;
                    }
                    let Some(pb) = ps[0].0.deref(ctx).get_parent_block() else { return false };
                    let mut seen: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
                    let mut work = vec![b];
                    while let Some(x) = work.pop() {
                        if x == pb || seen.len() > 64 {
                            return false;
                        }
                        if !seen.insert(x) {
                            continue;
                        }
                        for op in x.deref(ctx).iter(ctx) {
                            work.extend(op.deref(ctx).successors(ctx));
                            work.extend(st.invokes.get(op).map(|&(l, _)| l));
                        }
                    }
                    true
                };
                if !reach.contains(&b)
                    && !(st.allocas.contains(v)
                        && v.defining_op().is_some_and(|op| {
                            op.deref(ctx).get_parent_block() == Some(bs[0])
                        }))
                    && !dead_single_pred()
                {
                    continue;
                }
                // A region-scoped `nnargs` fact on `arg` (an inlined
                // call's result, `PLIRON_RETNN`) holds only at the
                // continuation; `v` would lose it. An alloca's address
                // needs no fact (and must not block SROA).
                if pin_nn && st.nnargs.contains(arg) && !st.allocas.contains(v) {
                    continue;
                }
                // `PLIRON_SCOPED_NOALIAS`: a clone-entry arg bound to a
                // `noalias` callee param is the name the scoped fact hangs
                // on (`State::region_noalias`); folded into the caller's
                // value, accesses through it are indistinguishable from the
                // caller's own. Allocas fold as above, and so does every
                // arg the fact could not serve (`scoped_hold`;
                // `PLIRON_SCOPED_PHI_ALL=1` holds those too).
                // `PLIRON_SCOPED_PHI=0` folds them all (then few candidates
                // reach lowering).
                if pin_na && st.region_noalias.contains(arg) && !st.allocas.contains(v) && (pin_all || scoped_hold(ctx, st, arg, v, pin_ro)) {
                    continue;
                }
                arg.replace_all_uses_with(ctx, &v);
                for &(t, si) in ps {
                    let o = Operation::get_op_dyn(t, ctx);
                    op_cast::<dyn BranchOpInterface>(&*o)
                        .unwrap()
                        .remove_successor_operand(ctx, si, i);
                }
                BasicBlock::remove_argument(b, ctx, i);
                n += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    n
}

pub fn run(ctx: &mut Context, st: &State) {
    let fns: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| opt_body(ctx, st, f))
        .collect();
    let landing: FxHashSet<Ptr<BasicBlock>> = st.invokes.values().map(|&(b, _)| b).collect();
    let mut n = 0;
    for f in fns {
        n += run_fn_with(ctx, st, f, &landing);
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats","phisimp {}: {n} block args removed", st.cgu)
        };
    }
}

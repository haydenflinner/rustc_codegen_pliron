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
                if !reach.contains(&b)
                    && !(st.allocas.contains(v)
                        && v.defining_op().is_some_and(|op| {
                            op.deref(ctx).get_parent_block() == Some(bs[0])
                        }))
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
                // caller's own. Allocas fold as above. `PLIRON_SCOPED_PHI=0`
                // folds them anyway (then few candidates reach lowering).
                if pin_na && st.region_noalias.contains(arg) && !st.allocas.contains(v) {
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

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
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::context::State;
use crate::inline::blocks;
use crate::lower::has_body;

/// Operand `i` that terminator `t` forwards to successor `si`, if `t` is a
/// branch forwarding exactly `na` operands there.
fn succ_operand(ctx: &Context, t: Ptr<Operation>, si: usize, i: usize, na: usize) -> Option<Value> {
    let o = Operation::get_op_dyn(t, ctx);
    let v = op_cast::<dyn BranchOpInterface>(&*o)?.successor_operands(ctx, si);
    (v.len() == na).then(|| v[i])
}

fn defined_in(ctx: &Context, v: Value, b: Ptr<BasicBlock>) -> bool {
    match v.defining_op() {
        Some(op) => op.deref(ctx).get_parent_block() == Some(b),
        None => b.deref(ctx).arguments().any(|a| a == v),
    }
}

/// Blocks reachable from the entry, counting invoke landing-pad edges.
fn reachable(ctx: &Context, st: &State<'_>, bs: &[Ptr<BasicBlock>]) -> FxHashSet<Ptr<BasicBlock>> {
    let mut seen = FxHashSet::default();
    let mut work = vec![bs[0]];
    while let Some(b) = work.pop() {
        if !seen.insert(b) {
            continue;
        }
        for op in b.deref(ctx).iter(ctx) {
            work.extend(op.deref(ctx).successors());
            work.extend(st.invokes.get(&op).map(|&(l, _)| l));
        }
    }
    seen
}

pub fn run(ctx: &mut Context, st: &State<'_>) {
    let landing: FxHashSet<Ptr<BasicBlock>> = st.invokes.values().map(|&(b, _)| b).collect();
    let fns: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    let mut n = 0;
    for f in fns {
        loop {
            let bs = blocks(ctx, f);
            // Unreachable code has no dominance order, and lowering emits it in
            // layout order, so only touch reachable blocks and values.
            let reach = reachable(ctx, st, &bs);
            let mut preds: FxHashMap<Ptr<BasicBlock>, Vec<(Ptr<Operation>, usize)>> =
                FxHashMap::default();
            for &b in &bs {
                let Some(t) = b.deref(ctx).iter(ctx).last() else {
                    continue;
                };
                let succs: Vec<_> = t.deref(ctx).successors().collect();
                for (si, s) in succs.into_iter().enumerate() {
                    preds.entry(s).or_default().push((t, si));
                }
            }
            let mut changed = false;
            for &b in &bs[1..] {
                let Some(ps) = preds
                    .get(&b)
                    .filter(|_| !landing.contains(&b) && reach.contains(&b))
                else {
                    continue;
                };
                let na = b.deref(ctx).get_num_arguments();
                for i in (0..na).rev() {
                    let arg = b.deref(ctx).get_argument(i);
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
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!("phisimp {}: {n} block args removed", st.cgu);
    }
}

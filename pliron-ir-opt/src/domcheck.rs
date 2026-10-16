//! Debug check (`PLIRON_CHECK_DOM=1`): after a pass, every operand's
//! definition must dominate its use. Reports the first violation per function.

use crate::has_body;
use crate::inline::{blocks, ops};
use crate::state::State;
use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::operation::Operation;
use pliron::value::DefiningEntity;
use rustc_hash::FxHashMap;

pub fn run(ctx: &Context, st: &State, pass: &str) {
    if !crate::env_set("PLIRON_CHECK_DOM") {
        return;
    }
    for (name, f) in &st.funcs {
        if has_body(ctx, f.op)
            && let Some(e) = check(ctx, st, f.op)
        {
            if pliron_cfg::tracing_on() {
                tracing::error!(target: "pliron::domcheck", "domcheck after {pass}: {name}: {e}")
            };
        }
    }
}

fn succs(ctx: &Context, st: &State, b: Ptr<BasicBlock>) -> Vec<Ptr<BasicBlock>> {
    let mut v = Vec::new();
    // Only the terminator carries successor edges (invokes included).
    if let Some(t) = b.deref(ctx).iter(ctx).last() {
        v.extend(t.deref(ctx).successors());
        v.extend(st.invokes.get(&t).map(|&(l, _)| l));
    }
    v
}

fn check(ctx: &Context, st: &State, f: Ptr<Operation>) -> Option<String> {
    let bs = blocks(ctx, f);
    let dom = Dom::new(ctx, st, f)?;
    let (post, idx) = (&dom.order, &dom.idx);
    let dominates = |a: usize, b: usize| dom.dom_idx(a, b);
    let all: FxHashMap<Ptr<BasicBlock>, usize> =
        bs.iter().enumerate().map(|(i, &b)| (b, i)).collect();
    for (bi, &b) in post.iter().enumerate() {
        let bops = ops(ctx, b);
        let pos: FxHashMap<Ptr<Operation>, usize> =
            bops.iter().enumerate().map(|(i, &o)| (o, i)).collect();
        for (oi, &op) in bops.iter().enumerate() {
            for v in op.deref(ctx).operands() {
                let (db, before) = match v.defining_entity() {
                    DefiningEntity::Op(d) => {
                        let Some(db) = d.deref(ctx).get_parent_block() else {
                            continue;
                        };
                        (db, pos.get(&d).is_some_and(|&p| p < oi))
                    }
                    DefiningEntity::Block(db) => (db, true),
                };
                let ok = if db == b {
                    before
                } else {
                    idx.get(&db).is_some_and(|&d| dominates(d, bi))
                };
                if !ok {
                    let what = match v.defining_entity() {
                        DefiningEntity::Op(d) => Operation::get_opid(d, ctx).to_string(),
                        DefiningEntity::Block(_) => "block arg".to_string(),
                    };
                    return Some(format!(
                        "`{}` in block #{} uses {what} from block #{:?} (reachable: {})",
                        Operation::get_opid(op, ctx),
                        all.get(&b).copied().unwrap_or(usize::MAX),
                        all.get(&db),
                        idx.contains_key(&db)
                    ));
                }
            }
        }
    }
    None
}

/// Dominator tree over the blocks reachable from the entry (counting invoke
/// landing-pad edges).
pub(crate) struct Dom {
    /// Reachable blocks in reverse post-order.
    pub order: Vec<Ptr<BasicBlock>>,
    pub idx: FxHashMap<Ptr<BasicBlock>, usize>,
    idom: Vec<usize>,
}

impl Dom {
    pub fn new(ctx: &Context, st: &State, f: Ptr<Operation>) -> Option<Dom> {
        let bs = blocks(ctx, f);
        let entry = *bs.first()?;
        // Reverse post-order over reachable blocks. Successor lists are
        // memoized: the DFS needs each block's once, and the `preds`
        // build below needs them all again — `succs` walks the block's
        // linked-list ops, so a second call doubles the pass's scan cost.
        let mut seen = FxHashMap::default();
        let mut post = Vec::new();
        let mut smap: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
        smap.insert(entry, succs(ctx, st, entry));
        let mut stack = vec![(entry, 0usize)];
        seen.insert(entry, ());
        while let Some((b, i)) = stack.last_mut() {
            let next = smap.get(b).and_then(|ss| ss.get(*i).copied());
            match next {
                Some(n) => {
                    *i += 1;
                    if seen.insert(n, ()).is_none() {
                        smap.insert(n, succs(ctx, st, n));
                        stack.push((n, 0));
                    }
                }
                None => {
                    post.push(*b);
                    stack.pop();
                }
            }
        }
        post.reverse();
        let idx: FxHashMap<Ptr<BasicBlock>, usize> =
            post.iter().enumerate().map(|(i, &b)| (b, i)).collect();
        let mut preds = vec![Vec::new(); post.len()];
        for (i, &b) in post.iter().enumerate() {
            for &s in smap.get(&b).into_iter().flatten() {
                preds[idx[&s]].push(i);
            }
        }
        let mut idom = vec![usize::MAX; post.len()];
        idom[0] = 0;
        let mut changed = true;
        while changed {
            changed = false;
            for b in 1..post.len() {
                let mut new = usize::MAX;
                for &p in &preds[b] {
                    if idom[p] == usize::MAX {
                        continue;
                    }
                    new = if new == usize::MAX {
                        p
                    } else {
                        let (mut x, mut y) = (new, p);
                        while x != y {
                            while x > y {
                                x = idom[x];
                            }
                            while y > x {
                                y = idom[y];
                            }
                        }
                        x
                    };
                }
                if new != idom[b] {
                    idom[b] = new;
                    changed = true;
                }
            }
        }
        Some(Dom {
            order: post,
            idx,
            idom,
        })
    }

    fn dom_idx(&self, a: usize, mut b: usize) -> bool {
        while b > a {
            b = self.idom[b];
        }
        b == a
    }

    /// Whether reachable block `a` dominates reachable block `b`.
    pub fn dominates(&self, a: Ptr<BasicBlock>, b: Ptr<BasicBlock>) -> bool {
        match (self.idx.get(&a), self.idx.get(&b)) {
            (Some(&a), Some(&b)) => self.dom_idx(a, b),
            _ => false,
        }
    }

    /// Immediate dominator of a reachable block (None for the entry).
    pub fn idom(&self, b: Ptr<BasicBlock>) -> Option<Ptr<BasicBlock>> {
        let &i = self.idx.get(&b)?;
        (self.idom[i] != i).then(|| self.order[self.idom[i]])
    }
}

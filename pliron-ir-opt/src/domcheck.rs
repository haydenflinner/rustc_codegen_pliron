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
        v.extend(t.deref(ctx).successors(ctx));
        v.extend(st.invokes.get(t).map(|&(l, _)| l));
    }
    v
}

fn check(ctx: &Context, st: &State, f: Ptr<Operation>) -> Option<String> {
    let bs = blocks(ctx, f);
    let dom = Dom::new(ctx, st, f)?;
    let post = &dom.order;
    let dominates = |a: usize, b: usize| dom.dom_idx(a, b);
    let all: FxHashMap<Ptr<BasicBlock>, usize> =
        bs.iter().enumerate().map(|(i, &b)| (b, i)).collect();
    for (bi, &b) in post.iter().enumerate() {
        let bops = ops(ctx, b);
        let pos: FxHashMap<Ptr<Operation>, usize> =
            bops.iter().enumerate().map(|(i, &o)| (o, i)).collect();
        for (oi, &op) in bops.iter().enumerate() {
            for v in op.deref(ctx).operands(ctx) {
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
                    dom.rpo_idx(db).is_some_and(|d| dominates(d, bi))
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
                        dom.rpo_idx(db),
                        dom.rpo_idx(db).is_some()
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
    /// block → local ordinal (covers every `blocks` entry plus any
    /// successor outside region 0 the DFS can still reach).
    ord: FxHashMap<Ptr<BasicBlock>, usize>,
    /// ordinal → position in `order` (usize::MAX = unreachable).
    rpo_of: Vec<usize>,
    idom: Vec<usize>,
}

impl Dom {
    pub fn new(ctx: &Context, st: &State, f: Ptr<Operation>) -> Option<Dom> {
        let mut bs = blocks(ctx, f);
        let entry = *bs.first()?;
        // Local ordinals keep the DFS state (seen/smap) dense — one
        // ptr→ord map replaces a `seen` set + a `smap` hash map. A
        // successor `blocks` didn't list (terminator pointing outside
        // region 0) still gets a fresh ordinal, matching the old
        // open-addressing behavior.
        let mut ord: FxHashMap<Ptr<BasicBlock>, usize> =
            bs.iter().enumerate().map(|(i, &b)| (b, i)).collect();
        let mut seen: Vec<bool> = vec![false; bs.len()];
        let mut smap: Vec<Vec<Ptr<BasicBlock>>> = vec![Vec::new(); bs.len()];
        let get_ord = |ord: &mut FxHashMap<Ptr<BasicBlock>, usize>,
                           bs: &mut Vec<Ptr<BasicBlock>>,
                           seen: &mut Vec<bool>,
                           smap: &mut Vec<Vec<Ptr<BasicBlock>>>,
                           b: Ptr<BasicBlock>| {
            match ord.get(&b) {
                Some(&o) => o,
                None => {
                    let o = bs.len();
                    bs.push(b);
                    ord.insert(b, o);
                    seen.push(false);
                    smap.push(Vec::new());
                    o
                }
            }
        };
        // Reverse post-order over reachable blocks. Successor lists are
        // memoized: the DFS needs each block's once, and the `preds`
        // build below needs them all again — `succs` walks the block's
        // linked-list ops, so a second call doubles the pass's scan cost.
        let mut post: Vec<usize> = Vec::new();
        let eord = get_ord(&mut ord, &mut bs, &mut seen, &mut smap, entry);
        smap[eord] = succs(ctx, st, entry);
        seen[eord] = true;
        let mut stack = vec![(eord, 0usize)];
        while let Some(&(bo, i)) = stack.last() {
            match smap[bo].get(i).copied() {
                Some(n) => {
                    stack.last_mut().unwrap().1 += 1;
                    let no = get_ord(&mut ord, &mut bs, &mut seen, &mut smap, n);
                    if !seen[no] {
                        seen[no] = true;
                        smap[no] = succs(ctx, st, n);
                        stack.push((no, 0));
                    }
                }
                None => {
                    post.push(bo);
                    stack.pop();
                }
            }
        }
        post.reverse();
        let mut rpo_of = vec![usize::MAX; bs.len()];
        for (i, &o) in post.iter().enumerate() {
            rpo_of[o] = i;
        }
        let mut preds = vec![Vec::new(); post.len()];
        for (i, &o) in post.iter().enumerate() {
            for &s in &smap[o] {
                preds[rpo_of[ord[&s]]].push(i);
            }
        }
        let order: Vec<Ptr<BasicBlock>> = post.iter().map(|&o| bs[o]).collect();
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
            order,
            ord,
            rpo_of,
            idom,
        })
    }

    /// `b`'s position in `order` — None when unreachable or unknown.
    fn rpo_idx(&self, b: Ptr<BasicBlock>) -> Option<usize> {
        let &o = self.ord.get(&b)?;
        (self.rpo_of[o] != usize::MAX).then_some(self.rpo_of[o])
    }

    /// Whether `b` is reachable from the entry (was `idx.contains_key`).
    pub fn contains(&self, b: Ptr<BasicBlock>) -> bool {
        self.rpo_idx(b).is_some()
    }

    fn dom_idx(&self, a: usize, mut b: usize) -> bool {
        while b > a {
            b = self.idom[b];
        }
        b == a
    }

    /// Whether reachable block `a` dominates reachable block `b`.
    pub fn dominates(&self, a: Ptr<BasicBlock>, b: Ptr<BasicBlock>) -> bool {
        match (self.rpo_idx(a), self.rpo_idx(b)) {
            (Some(a), Some(b)) => self.dom_idx(a, b),
            _ => false,
        }
    }

    /// Immediate dominator of a reachable block (None for the entry).
    pub fn idom(&self, b: Ptr<BasicBlock>) -> Option<Ptr<BasicBlock>> {
        let i = self.rpo_idx(b)?;
        (self.idom[i] != i).then(|| self.order[self.idom[i]])
    }
}

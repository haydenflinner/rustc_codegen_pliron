//! Profile pruning of guarded-devirtualization chains (`PLIRON_DEVIRT`
//! with `PLIRON_PGO_USE`). The front end emits the same full chain in the
//! instrumented and the profile-use build (so their CFG fingerprints
//! agree) and records each site in `State::devirt_sites`; every hit block
//! then carries the count of calls that reached its target. After
//! `pgo::load`, [`prune`] keeps the arms that earned their guard and
//! relinks them hottest first.

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::operation::Operation;
use pliron_llvm::ops::CondBrOp;
use rustc_hash::FxHashMap;

use crate::inline::blocks;
use crate::state::State;

fn knob(name: &str, default: u64) -> u64 {
    crate::env_get(name).and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// Keep, per site, at most `PLIRON_DEVIRT_MAX` (default 2) arms whose hit
/// count is at least `PLIRON_DEVIRT_MIN_SHARE` percent (default 10) of
/// the site's calls, hottest first; a site that never ran (or whose
/// function has no matching profile) goes back to the plain indirect
/// call. Dropped arms' counts move to the fallback block. Returns the
/// number of sites rewritten.
pub fn prune(ctx: &mut Context, st: &mut State) -> usize {
    let sites = std::mem::take(&mut st.devirt_sites);
    if sites.is_empty() {
        return 0;
    }
    let max = knob("PLIRON_DEVIRT_MAX", 2) as usize;
    let share = knob("PLIRON_DEVIRT_MIN_SHARE", 10) as u128;
    let mut touched: Vec<Ptr<Operation>> = Vec::new();
    for s in sites {
        if !s.fallback.is_live(ctx) || s.arms.iter().any(|(g, h)| !g.is_live(ctx) || !h.is_live(ctx)) {
            continue;
        }
        // The recorded shape must still hold: guard i ends in
        // `cond_br _, hit_i, guard_{i+1} | fallback`.
        let n = s.arms.len();
        let terms: Vec<Option<Ptr<Operation>>> = (0..n)
            .map(|i| {
                let (g, h) = s.arms[i];
                let next = s.arms.get(i + 1).map_or(s.fallback, |a| a.0);
                let t = g.deref(ctx).iter(ctx).last()?;
                let o = t.deref(ctx);
                (Operation::is_op::<CondBrOp>(t, ctx)
                    && o.get_num_successors() == 2
                    && o.get_successor(ctx, 0) == h
                    && o.get_successor(ctx, 1) == next)
                    .then_some(t)
            })
            .collect();
        if n == 0 || terms.iter().any(|t| t.is_none()) {
            pliron_cfg::remark!("devirt", s.caller, s.what; "profile: chain shape changed, kept as built");
            continue;
        }
        let terms: Vec<Ptr<Operation>> = terms.into_iter().flatten().collect();
        let Some(func) = s.fallback.deref(ctx).get_parent_op(ctx) else {
            continue;
        };
        let profiled = st.prof.get(func).is_some();
        let counts: Vec<u64> = s
            .arms
            .iter()
            .map(|(_, h)| st.prof_bcount.get(*h).copied().unwrap_or(0))
            .collect();
        let fb = st.prof_bcount.get(s.fallback).copied().unwrap_or(0);
        let total: u64 = counts.iter().fold(fb, |a, c| a.saturating_add(*c));
        let mut order: Vec<usize> = (0..n)
            .filter(|&i| counts[i] > 0 && counts[i] as u128 * 100 >= share * total as u128)
            .collect();
        order.sort_by(|&a, &b| counts[b].cmp(&counts[a]).then(a.cmp(&b)));
        order.truncate(max);
        if pliron_cfg::remarks_on() {
            let pct = |c: u64| if total == 0 { 0 } else { (c as u128 * 100 / total as u128) as u64 };
            let shares: Vec<String> = counts.iter().map(|&c| format!("{}%", pct(c))).collect();
            pliron_cfg::remark!("devirt", s.caller, s.what;
                "profile: kept {} of {} arms (shares {} fallback {}% calls {}{})",
                order.len(), n, shares.join(" "), pct(fb), total,
                if profiled { "" } else { ", no profile for function" });
        }
        // Relink: predecessor -> kept guards in order -> fallback.
        let chain: Vec<Ptr<BasicBlock>> =
            order.iter().map(|&i| s.arms[i].0).chain([s.fallback]).collect();
        let first = s.arms[0].0;
        if chain[0] != first {
            first.retarget_all_preds_to(ctx, chain[0]);
        }
        let mut remaining = total;
        for (j, &i) in order.iter().enumerate() {
            let t = terms[i];
            if t.deref(ctx).get_successor(ctx, 1) != chain[j + 1] {
                Operation::replace_successor(t, ctx, 1, chain[j + 1]);
            }
            let c = counts[i];
            let rest = remaining - c;
            st.prof_bcount.insert(s.arms[i].0, remaining);
            st.prof_edges.insert(t, vec![c, rest]);
            st.expect.remove(t);
            if remaining >= 8 && (c >= 4 * rest || rest >= 4 * c) && !crate::env_is("PLIRON_PGO_EXPECT", "0") {
                st.expect.insert(t, c >= rest);
            }
            remaining = rest;
        }
        if profiled {
            st.prof_bcount.insert(s.fallback, remaining);
        }
        for i in (0..n).filter(|i| !order.contains(i)) {
            let (g, h) = s.arms[i];
            st.prof_bcount.remove(g);
            st.prof_bcount.remove(h);
            st.prof_edges.remove(terms[i]);
            st.expect.remove(terms[i]);
            if let Some(t) = h.deref(ctx).iter(ctx).last() {
                st.prof_edges.remove(t);
            }
        }
        if order.len() < n && !touched.contains(&func) {
            touched.push(func);
        }
    }
    let n = touched.len();
    for f in touched {
        let bs = blocks(ctx, f);
        let om: FxHashMap<_, _> =
            bs.iter().map(|&b| (b, b.deref(ctx).iter(ctx).collect::<Vec<_>>())).collect();
        crate::helpers::erase_unreachable(ctx, st, &bs, &|b| om[&b].clone());
    }
    if n > 0 {
        st.purge_dead(ctx);
    }
    n
}

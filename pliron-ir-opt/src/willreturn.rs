//! Bottom-up `willreturn` inference (LLVM FunctionAttrs' `willreturn`,
//! `Instruction::willReturn` applied per-op): every execution of the
//! function either returns, unwinds out, tail-calls out, or is UB — control
//! can't stay inside forever *and* can't observe the outside differently on
//! a re-execution. Rust has no forward-progress rule: `loop {}` is defined,
//! and rustc emits no `mustprogress`, so a loop or a recursion with no
//! observable interaction may legitimately never come back. The executions
//! that may never come back are the ones seeded below, propagated over the
//! call graph as a least fixpoint.
//!
//! Seeds (mirroring LLVM's per-instruction check for a function without
//! `mustprogress`):
//! - a cycle in the CFG reachable from the entry — any loop may run forever (LLVM would accept a
//!   loop with a SCEV-bounded trip count; we have no trip-count proof);
//! - a cycle in the call graph — recursion may not bottom out. Every cycle
//!   holds a DFS back edge; seeding the back edge's caller reaches each
//!   member through the `call` propagation rule;
//! - a volatile op — LLVM's `willReturn` is false on volatile accesses
//!   (they're legal fuel for an infinite loop *and* nondeterministic, so a
//!   dedup'd or speculated call could produce a different value);
//! - an atomic op or fence — conservatively (unordered atomics could be
//!   allowed later);
//! - inline asm — may contain an unbounded loop;
//! - a `trap` intrinsic — never comes back;
//! - an indirect call, or a direct call to a callee with no table entry —
//!   it may do anything;
//! - a call to a `diverge` callee — propagated.
//!
//! Noreturn callsites (a call directly followed by `llvm.unreachable` —
//! the panic/abort paths rustc emits after `panic_*` calls) don't seed
//! from `!known(c)`: they exit the frame by unwinding or are unreachable
//! code. A proven-diverging bodied callee still propagates through them.
//! The escape can mark `f(){ abort(); unreachable }` willreturn where LLVM
//! would refuse, but such an `f` is never `nowrite` — every consumer here
//! requires both.
//!
//! Stored as `st.willreturn`: a set rather than a map — members provably
//! come back *and* behave deterministically; everyone else may loop
//! forever or observe differently.

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::operation::Operation;
use pliron_llvm::ops::{
    AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp, FenceOp,
    InlineAsmOp, UnreachableOp,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::inline::{blocks_iter, direct_callee};
use crate::state::State;
use crate::{Syms, bodied};

/// libc functions that return and read deterministically (the same set
/// `nowrite` trusts).
const LIBC: &[&str] = &["memcmp", "bcmp", "strlen"];

ascent::ascent! {
    /// Divergence as a least fixpoint (the complement of "comes back
    /// deterministically until proven otherwise"). Function ids index `Syms`.
    struct WillRet;
    /// A direct call in `f` to `c`; bool = noreturn callsite (`unreachable`
    /// follows the call — panic/abort paths).
    relation call(u32, u32, bool);
    /// An indirect call in `f`; bool = noreturn callsite.
    relation icall(u32, bool);
    /// Functions with a table entry: bodied ones and `LIBC`.
    relation known(u32);
    /// `f`'s body contains an op that can interact with the world in a way
    /// a re-execution may observe differently (or that can fuel a legal
    /// infinite loop): volatile, atomic, asm, trap.
    relation side_fx(u32);
    /// `f` has a loop in its CFG, or makes the back-edge call of a cycle in
    /// the call graph: it may keep control forever without side effects.
    relation cyclic(u32);

    /// `f` may keep control forever or behave nondeterministically.
    relation diverge(u32);
    diverge(f) <-- side_fx(f);
    diverge(f) <-- cyclic(f);
    diverge(f) <-- icall(f, false);
    diverge(f) <-- call(f, c, false), !known(c);
    diverge(f) <-- call(f, c, _), diverge(c);
}

// r[impl iropt.ipo.willreturn]
pub fn run(ctx: &Context, st: &mut State) {
    let bodied = bodied(ctx, st);
    let mut ids = Syms::default();
    let mut p = WillRet::default();
    for s in LIBC
        .iter()
        .copied()
        .chain(bodied.iter().map(|s| s.as_str()))
    {
        p.known.push((ids.id(s),));
    }
    let mut os = Vec::new();
    // The CFG is gathered during the same op walk: block ordinals, and
    // (block, successor) edges from terminators and invoke landing pads.
    let mut ord: FxHashMap<Ptr<BasicBlock>, u32> = FxHashMap::default();
    let mut raw: Vec<(u32, Ptr<BasicBlock>)> = Vec::new();
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for s in &bodied {
        let f = ids.id(s);
        // Already diverging from an op seed: the cycle check can't add to it.
        let mut seeded = false;
        ord.clear();
        raw.clear();
        for (i, b) in blocks_iter(ctx, st.funcs[s].op).enumerate() {
            let i = i as u32;
            ord.insert(b, i);
            crate::inline::ops_into(ctx, b, &mut os);
            for (k, &op) in os.iter().enumerate() {
                macro_rules! is {
                    ($t:ty) => {
                        Operation::is_op::<$t>(op, ctx)
                    };
                }
                if is!(CallOp) {
                    // Invokes are CallOps: their landing pad is a CFG edge.
                    if let Some(&(pad, _)) = st.invokes.get(op) {
                        raw.push((i, pad));
                    }
                    // A tail call *is* the caller's exit: `unreachable`
                    // after it can't let an unknown callee off the hook.
                    let tail = st.calls.get(op).is_some_and(|ci| ci.tail);
                    let unr = !tail
                        && os
                            .get(k + 1)
                            .is_some_and(|&n| Operation::is_op::<UnreachableOp>(n, ctx));
                    match direct_callee(ctx, st, op) {
                        Some(c) => p.call.push((f, ids.id(c), unr)),
                        None => {
                            seeded |= !unr;
                            p.icall.push((f, unr))
                        }
                    }
                } else if st.volatile.contains(op)
                    || is!(AtomicRmwOp)
                    || is!(AtomicCmpxchgOp)
                    || is!(AtomicLoadOp)
                    || is!(AtomicStoreOp)
                    || is!(FenceOp)
                    || is!(InlineAsmOp)
                    || (is!(CallIntrinsicOp)
                        && st.intrinsics.get(op).is_some_and(|n| n.contains("trap")))
                {
                    seeded = true;
                    p.side_fx.push((f,));
                }
            }
            if let Some(&t) = os.last() {
                for d in t.deref(ctx).successors(ctx) {
                    raw.push((i, d));
                }
            }
        }
        if !seeded {
            edges.clear();
            edges.extend(raw.iter().filter_map(|&(a, d)| ord.get(&d).map(|&j| (a, j))));
            if cfg_has_cycle(ord.len(), &mut edges) {
                p.cyclic.push((f,));
            }
        }
    }
    for f in back_edge_callers(&p.call) {
        p.cyclic.push((f,));
    }
    p.run();
    let diverge: FxHashSet<u32> = p.diverge.iter().map(|&(f,)| f).collect();
    let wr: FxHashSet<String> = LIBC
        .iter()
        .map(|s| s.to_string())
        .chain(
            bodied
                .iter()
                .filter(|s| !diverge.contains(&ids.id(s)))
                .cloned(),
        )
        .collect();
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats",
                "willreturn {}: {} of {} bodies come back",
                st.cgu,
                wr.len() - LIBC.len(),
                bodied.len()
            )
        };
    }
    for s in &bodied {
        crate::dump_fact("willreturn", s, &wr.contains(s));
    }
    st.willreturn = wr.into_iter().collect();
}

/// Whether the CFG reachable from the entry block (ordinal 0) has a cycle:
/// an iterative DFS that stops at the first back edge. `edges` are
/// (from, to) ordinals, sorted in place into per-block runs.
fn cfg_has_cycle(n: usize, edges: &mut [(u32, u32)]) -> bool {
    if n == 0 {
        return false;
    }
    edges.sort_unstable();
    let mut start = vec![0u32; n + 1];
    for &(a, _) in edges.iter() {
        start[a as usize + 1] += 1;
    }
    for i in 0..n {
        start[i + 1] += start[i];
    }
    // 0 = unseen, 1 = on the DFS stack, 2 = done
    let mut state = vec![0u8; n];
    state[0] = 1;
    let mut dfs = vec![(0usize, start[0])];
    while let Some(&mut (v, ref mut k)) = dfs.last_mut() {
        if *k < start[v + 1] {
            let w = edges[*k as usize].1 as usize;
            *k += 1;
            match state[w] {
                0 => {
                    state[w] = 1;
                    dfs.push((w, start[w]));
                }
                1 => return true,
                _ => {}
            }
            continue;
        }
        state[v] = 2;
        dfs.pop();
    }
    false
}

/// Callers of the back edges of an iterative DFS over the direct-call
/// graph: at least one per cycle (self-calls included), in one linear walk.
/// Roots are taken in ascending id order, so the result is deterministic.
fn back_edge_callers(calls: &[(u32, u32, bool)]) -> Vec<u32> {
    let n = calls.iter().map(|&(f, c, _)| f.max(c) as usize + 1).max().unwrap_or(0);
    let mut succ: Vec<Vec<u32>> = vec![Vec::new(); n];
    for &(f, c, _) in calls {
        succ[f as usize].push(c);
    }
    // 0 = unseen, 1 = on the DFS stack, 2 = done
    let mut state = vec![0u8; n];
    let mut out = Vec::new();
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        let mut dfs = vec![(root, 0usize)];
        while let Some(&mut (v, ref mut k)) = dfs.last_mut() {
            if let Some(&w) = succ[v].get(*k) {
                *k += 1;
                match state[w as usize] {
                    0 => {
                        state[w as usize] = 1;
                        dfs.push((w as usize, 0));
                    }
                    1 => out.push(v as u32),
                    _ => {}
                }
                continue;
            }
            state[v] = 2;
            dfs.pop();
        }
    }
    out
}

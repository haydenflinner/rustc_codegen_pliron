//! Bottom-up `willreturn` inference (LLVM FunctionAttrs' `willreturn`,
//! `Instruction::willReturn` applied per-op): every execution of the
//! function either returns, unwinds out, tail-calls out, or is UB — control
//! can't stay inside forever *and* can't observe the outside differently on
//! a re-execution. rustc marks Rust fns `mustprogress`: an infinite
//! loop/recursion with no observable interaction is UB, so the only
//! executions that may legitimately never come back are the ones seeded
//! below, propagated over the call graph as a least fixpoint.
//!
//! Seeds (mirroring LLVM's per-instruction check):
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

use pliron::context::Context;
use pliron::operation::Operation;
use pliron_llvm::ops::{
    AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp, FenceOp,
    InlineAsmOp, UnreachableOp,
};
use rustc_hash::FxHashSet;

use crate::inline::{blocks, direct_callee};
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

    /// `f` may keep control forever or behave nondeterministically.
    relation diverge(u32);
    diverge(f) <-- side_fx(f);
    diverge(f) <-- icall(f, false);
    diverge(f) <-- call(f, c, false), !known(c);
    diverge(f) <-- call(f, c, _), diverge(c);
}

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
    for s in &bodied {
        let f = ids.id(s);
        for b in blocks(ctx, st.funcs[s].op) {
            crate::inline::ops_into(ctx, b, &mut os);
            for (k, &op) in os.iter().enumerate() {
                macro_rules! is {
                    ($t:ty) => {
                        Operation::is_op::<$t>(op, ctx)
                    };
                }
                if is!(CallOp) {
                    // A tail call *is* the caller's exit: `unreachable`
                    // after it can't let an unknown callee off the hook.
                    let tail = st.calls.get(&op).is_some_and(|ci| ci.tail);
                    let unr = !tail
                        && os
                            .get(k + 1)
                            .is_some_and(|&n| Operation::is_op::<UnreachableOp>(n, ctx));
                    match direct_callee(ctx, st, op) {
                        Some(c) => p.call.push((f, ids.id(c), unr)),
                        None => p.icall.push((f, unr)),
                    }
                } else if st.volatile.contains(&op)
                    || is!(AtomicRmwOp)
                    || is!(AtomicCmpxchgOp)
                    || is!(AtomicLoadOp)
                    || is!(AtomicStoreOp)
                    || is!(FenceOp)
                    || is!(InlineAsmOp)
                    || (is!(CallIntrinsicOp)
                        && st.intrinsics.get(&op).is_some_and(|n| n.contains("trap")))
                {
                    p.side_fx.push((f,));
                }
            }
        }
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
    st.willreturn = wr;
}

//! Bottom-up nounwind inference: a body that makes no plain call to a
//! function that may unwind cannot unwind itself (invoked calls are caught
//! by its own landing pads, and `resume` is a plain call to
//! `_Unwind_Resume`). Invokes of nounwind callees become plain calls, so
//! their landing pads become dead and Cranelift drops them.

use pliron::context::Context;
use pliron::operation::Operation;
use pliron_llvm::ops::CallOp;
use rustc_hash::FxHashSet;

use crate::inline::{blocks, direct_callee, ops};
use crate::state::State;
use crate::{Syms, bodied};

/// Unwind-outs invisible to the attribute scan. The emulated-EH entries
/// (`llvm.wasm.*`, and `_CxxThrowException`/`_Unwind_Resume`/`_Unwind_RaiseException`
/// when lowering intercepts them) are `extern "C"` decls rustc marks
/// `!can_unwind`, but wasm.rs/lower.rs lower calls to them into a flag set
/// on `__pliron_eh` plus an early return that the caller's post-call check
/// unwinds through. Native `_Unwind_*` calls unwind out for real, so they
/// belong here under any EH model.
const THROWS: &[&str] = &[
    "llvm.wasm.throw",
    "llvm.wasm.rethrow",
    "__pliron_llvm_wasm_throw",
    "__pliron_llvm_wasm_rethrow",
    "_CxxThrowException",
    "_Unwind_Resume",
    "_Unwind_RaiseException",
];

ascent::ascent! {
    struct Unwind;
    /// A plain (non-invoke) call in `f` to `c`.
    relation call(u32, u32);
    /// `f` makes a plain indirect call.
    relation icall(u32);
    /// Bodies the inference covers, and functions already known nounwind.
    relation body(u32);
    relation nounwind(u32);

    relation may_unwind(u32);
    may_unwind(f) <-- icall(f);
    may_unwind(f) <-- call(f, c), !body(c), !nounwind(c);
    may_unwind(f) <-- call(f, c), may_unwind(c);
}

pub fn run(ctx: &Context, st: &mut State) {
    let bodied: Vec<String> = bodied(ctx, st)
        .into_iter()
        .filter(|s| !st.funcs[s].nounwind)
        .collect();
    let mut ids = Syms::default();
    let mut p = Unwind::default();
    for (s, f) in &st.funcs {
        if f.nounwind && !THROWS.contains(&s.as_str()) {
            p.nounwind.push((ids.id(s),));
        }
    }
    for s in &bodied {
        let f = ids.id(s);
        p.body.push((f,));
        for b in blocks(ctx, st.funcs[s].op) {
            for op in ops(ctx, b) {
                // `pliron.eh.rethrow` unwinds out of `f` through a
                // CallIntrinsicOp — the `call` relation only sees CallOps.
                if st
                    .intrinsics
                    .get(&op)
                    .is_some_and(|n| n == "pliron.eh.rethrow")
                {
                    p.may_unwind.push((f,));
                }
                if Operation::is_op::<CallOp>(op, ctx) && !st.invokes.contains_key(&op) {
                    match direct_callee(ctx, st, op) {
                        Some(c) => p.call.push((f, ids.id(c))),
                        None => p.icall.push((f,)),
                    }
                }
            }
        }
    }
    p.run();
    let unw: FxHashSet<u32> = p.may_unwind.iter().map(|&(f,)| f).collect();
    let nw: FxHashSet<String> = bodied
        .iter()
        .filter(|s| !unw.contains(&ids.id(s)))
        .cloned()
        .collect();
    for s in &bodied {
        crate::dump_fact("nounwind", s, &nw.contains(s));
    }
    let dead: Vec<_> = st
        .invokes
        .keys()
        .copied()
        .filter(|&op| {
            direct_callee(ctx, st, op).is_some_and(|c| {
                !THROWS.contains(&c.as_str())
                    && (nw.contains(c) || st.funcs.get(c).is_some_and(|f| f.nounwind))
            })
        })
        .collect();
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats",
                "nounwind {}: {} of {} invokes -> calls",
                st.cgu,
                dead.len(),
                st.invokes.len()
            )
        };
    }
    for op in dead {
        st.invokes.remove(&op);
    }
    // Persist the inference: the emulated-EH flag check after calls on COFF
    // is skipped for callees that can't unwind.
    for s in &nw {
        if let Some(f) = st.funcs.get_mut(s) {
            f.nounwind = true;
        }
    }
}

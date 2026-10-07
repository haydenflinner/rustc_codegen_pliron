//! Bottom-up nounwind inference: a body that makes no plain call to a
//! function that may unwind cannot unwind itself (invoked calls are caught
//! by its own landing pads, and `resume` is a plain call to
//! `_Unwind_Resume`). Invokes of nounwind callees become plain calls, so
//! their landing pads become dead and Cranelift drops them.

use cranelift_module::Linkage;
use pliron::context::{Context, Ptr};
use pliron::operation::Operation;
use pliron_llvm::ops::CallOp;
use rustc_data_structures::fx::FxHashSet;

use crate::context::State;
use crate::inline::{blocks, direct_callee, ops};
use crate::lower::has_body;

fn may_unwind(ctx: &Context, st: &State<'_>, f: Ptr<Operation>, nw: &FxHashSet<String>) -> bool {
    blocks(ctx, f).into_iter().any(|b| {
        ops(ctx, b).into_iter().any(|op| {
            Operation::is_op::<CallOp>(op, ctx)
                && !st.invokes.contains_key(&op)
                && !direct_callee(ctx, st, op).is_some_and(|c| nw.contains(c))
        })
    })
}

pub fn run(ctx: &Context, st: &mut State<'_>) {
    let bodied: Vec<String> = st
        .funcs
        .iter()
        .filter(|(_, f)| !f.nounwind && f.linkage != Linkage::Preemptible && has_body(ctx, f.op))
        .map(|(s, _)| s.clone())
        .collect();
    let mut nw: FxHashSet<String> = st
        .funcs
        .iter()
        .filter(|(_, f)| f.nounwind)
        .map(|(s, _)| s.clone())
        .collect();
    // Optimistic start, then drop functions until a fixpoint: handles recursion.
    nw.extend(bodied.iter().cloned());
    loop {
        let mut changed = false;
        for s in &bodied {
            if nw.contains(s) && may_unwind(ctx, st, st.funcs[s].op, &nw) {
                nw.remove(s);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let dead: Vec<_> = st
        .invokes
        .keys()
        .copied()
        .filter(|&op| direct_callee(ctx, st, op).is_some_and(|c| nw.contains(c)))
        .collect();
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!(
            "nounwind {}: {} of {} invokes -> calls",
            st.cgu,
            dead.len(),
            st.invokes.len()
        );
    }
    for op in dead {
        st.invokes.remove(&op);
    }
}

//! Bottom-up write-free inference (LLVM FunctionAttrs' `memory(read)`): a
//! body with no store, atomic, fence, inline asm, va_arg, memory-writing
//! intrinsic, or call to anything but a write-free function leaves memory as
//! it found it (stores into its own frame aside), so loadfwd keeps loads available across calls to it.

use cranelift_module::Linkage;
use pliron::context::{Context, Ptr};
use pliron::operation::Operation;
use pliron::value::Value;
use pliron_llvm::ops::{
    AllocaOp, AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp,
    FenceOp, GetElementPtrOp, InlineAsmOp, StoreOp, UnreachableOp, VAArgOp,
};
use rustc_data_structures::fx::FxHashMap;

use crate::context::State;
use crate::inline::{blocks, direct_callee, ops};
use crate::lower::has_body;

/// libc functions that only read memory.
const LIBC: &[&str] = &["memcmp", "bcmp", "strlen"];

/// Intrinsic name prefixes that neither write memory nor synchronize.
const PURE: &[&str] = &[
    "llvm.ctlz.",
    "llvm.cttz.",
    "llvm.ctpop.",
    "llvm.bswap.",
    "llvm.bitreverse.",
    "llvm.fshl.",
    "llvm.fshr.",
    "llvm.umin.",
    "llvm.umax.",
    "llvm.smin.",
    "llvm.smax.",
    "llvm.abs.",
    "llvm.assume",
    "llvm.expect.",
    "llvm.lifetime.",
    "llvm.uadd.",
    "llvm.sadd.",
    "llvm.usub.",
    "llvm.ssub.",
    "llvm.umul.",
    "llvm.smul.",
    "llvm.fabs.",
    "llvm.copysign.",
    "llvm.sqrt.",
    "llvm.fma.",
    "llvm.floor.",
    "llvm.ceil.",
    "llvm.trunc.",
    "llvm.round.",
];

/// An address into the function's own frame (an alloca through any GEP
/// chain; leaving the alloca is UB): stores there die with the frame.
fn frame_local(ctx: &Context, mut p: Value) -> bool {
    loop {
        match p.defining_op() {
            Some(d) if Operation::is_op::<AllocaOp>(d, ctx) => return true,
            Some(d) if Operation::is_op::<GetElementPtrOp>(d, ctx) => {
                p = d.deref(ctx).get_operand(0)
            }
            _ => return false,
        }
    }
}

/// Whether `f` may write memory. Strict: never writes. Otherwise: writes
/// only on paths that end in a plain noreturn call (a panic), which leave
/// `f` by unwinding straight through it, so a normal return saw no write.
/// Invokes may land in `f`'s own pads and return, so they need a strict callee.
fn writes(
    ctx: &Context,
    st: &State<'_>,
    f: Ptr<Operation>,
    nw: &FxHashMap<String, bool>,
    strict: bool,
) -> bool {
    blocks(ctx, f).into_iter().any(|b| {
        let os = ops(ctx, b);
        os.iter().enumerate().any(|(k, &op)| {
            macro_rules! is {
                ($t:ty) => {
                    Operation::is_op::<$t>(op, ctx)
                };
            }
            (is!(StoreOp) && !frame_local(ctx, op.deref(ctx).get_operand(1)))
                || is!(AtomicRmwOp)
                || is!(AtomicCmpxchgOp)
                || is!(FenceOp)
                || is!(AtomicLoadOp)
                || is!(AtomicStoreOp)
                || is!(InlineAsmOp)
                || is!(VAArgOp)
                || (is!(CallIntrinsicOp)
                    && !st
                        .intrinsics
                        .get(&op)
                        .is_some_and(|n| PURE.iter().any(|p| n.starts_with(p))))
                || (is!(CallOp) && {
                    let callee = direct_callee(ctx, st, op).and_then(|c| nw.get(c).copied());
                    if strict || st.invokes.contains_key(&op) {
                        callee != Some(true)
                    } else {
                        callee.is_none()
                            && !os
                                .get(k + 1)
                                .is_some_and(|&n| Operation::is_op::<UnreachableOp>(n, ctx))
                    }
                })
        })
    })
}

/// Drop optimistic `strict` entries until no listed body writes.
fn fixpoint(
    ctx: &Context,
    st: &State<'_>,
    bodied: &[String],
    nw: &mut FxHashMap<String, bool>,
    strict: bool,
) {
    loop {
        let mut changed = false;
        for s in bodied {
            if nw.get(s) == Some(&strict) && writes(ctx, st, st.funcs[s].op, nw, strict) {
                nw.remove(s);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

pub fn run(ctx: &Context, st: &mut State<'_>) {
    let bodied: Vec<String> = st
        .funcs
        .iter()
        .filter(|(_, f)| f.linkage != Linkage::Preemptible && has_body(ctx, f.op))
        .map(|(s, _)| s.clone())
        .collect();
    // Optimistic start, then drop functions until a fixpoint: handles recursion.
    let mut nw: FxHashMap<String, bool> = LIBC
        .iter()
        .map(|s| s.to_string())
        .chain(bodied.iter().cloned())
        .map(|s| (s, true))
        .collect();
    fixpoint(ctx, st, &bodied, &mut nw, true);
    let strict = nw.len();
    for s in &bodied {
        nw.entry(s.clone()).or_insert(false);
    }
    fixpoint(ctx, st, &bodied, &mut nw, false);
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!(
            "nowrite {}: {} of {} bodies write-free, {} more on return",
            st.cgu,
            bodied.iter().filter(|s| nw.get(*s) == Some(&true)).count(),
            bodied.len(),
            nw.len() - strict
        );
    }
    st.nowrite = nw;
}

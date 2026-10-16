//! Bottom-up write-free inference (LLVM FunctionAttrs' `memory(read)`): a
//! body with no store, atomic, fence, inline asm, va_arg, memory-writing
//! intrinsic, or call to anything but a write-free function leaves memory as
//! it found it (stores into its own frame aside), so loadfwd keeps loads available across calls to it.

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::operation::Operation;
use pliron::value::Value;
use pliron_llvm::ops::{
    AllocaOp, AtomicCmpxchgOp, AtomicLoadOp, AtomicRmwOp, AtomicStoreOp, CallIntrinsicOp, CallOp,
    FenceOp, GetElementPtrOp, InlineAsmOp, ReturnOp, StoreOp, UnreachableOp, VAArgOp,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::inline::{blocks, direct_callee};
use crate::state::State;
use crate::{Syms, bodied};

/// libc functions that only read memory.
const LIBC: &[&str] = &["memcmp", "bcmp", "strlen"];

/// Intrinsic name prefixes that neither write memory nor synchronize.
pub(crate) const PURE: &[&str] = &[
    "llvm.ctlz.",
    "llvm.cttz.",
    "llvm.cttz_nz",
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
pub(crate) fn frame_local(ctx: &Context, mut p: Value) -> bool {
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

ascent::ascent! {
    /// Write effects as a least fixpoint (the complement of the optimistic
    /// "write-free until proven otherwise" table). Function ids index `Syms`.
    struct WriteFx;
    /// `f` has a write that isn't a call (store outside its frame, atomic,
    /// fence, asm, va_arg, memory-writing intrinsic) in a block that can
    /// still reach a normal return.
    relation ret_write(u32);
    /// The same but in a block that can't reach a return — unwind pads
    /// ending in `panic_cannot_unwind`, abort paths: the write is only
    /// seen by a path that never comes back.
    relation dead_write(u32);
    /// A direct call in `f` to `c`, whether it is an invoke, and whether a
    /// plain `unreachable` follows it (a panic: the write is only seen by
    /// unwinding).
    relation call(u32, u32, bool, bool);
    /// The same, restricted to calls in return-reaching blocks.
    relation callr(u32, u32, bool, bool);
    /// The same for an indirect call (callee unknown).
    relation icall(u32, bool, bool);
    /// The same, restricted to calls in return-reaching blocks.
    relation icallr(u32, bool, bool);
    /// Functions with a table entry: bodied ones and `LIBC`.
    relation known(u32);

    /// A call that may do anything.
    relation any(u32, bool, bool);
    any(f, inv, unr) <-- icall(f, inv, unr);
    any(f, inv, unr) <-- call(f, c, inv, unr), !known(c);

    /// May write at all.
    relation strict(u32);
    strict(f) <-- ret_write(f);
    strict(f) <-- dead_write(f);
    strict(f) <-- any(f, _, _);
    strict(f) <-- call(f, c, _, _), strict(c);

    /// May write on a path that returns normally. An invoke may land in
    /// `f`'s own pad and return, so it needs a strictly write-free callee.
    /// Calls and writes in dead blocks can't be observed by a return.
    relation on_return(u32);
    on_return(f) <-- ret_write(f);
    on_return(f) <-- icallr(f, inv, unr), if *inv || !*unr;
    on_return(f) <-- callr(f, c, inv, unr), !known(c), if *inv || !*unr;
    on_return(f) <-- callr(f, c, true, _), strict(c);
    on_return(f) <-- callr(f, c, false, false), on_return(c);
}

pub fn run(ctx: &Context, st: &mut State) {
    let bodied = bodied(ctx, st);
    let mut ids = Syms::default();
    let mut p = WriteFx::default();
    for s in LIBC
        .iter()
        .copied()
        .chain(bodied.iter().map(|s| s.as_str()))
    {
        p.known.push((ids.id(s),));
    }
    for s in &bodied {
        let f = ids.id(s);
        let bs = blocks(ctx, st.funcs[s].op);
        // Blocks that can still reach a normal return: backward reach from
        // `return`/`become` blocks over CFG+invoke edges. Pads that only
        // reach `panic_cannot_unwind` or `unreachable` stay out — writes
        // there (e.g. `pliron.eh.exn` clearing the flag) die with the path.
        let mut can_ret = FxHashSet::default();
        let mut wl = Vec::new();
        let mut rp: FxHashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = FxHashMap::default();
        for &b in &bs {
            for op in b.deref(ctx).iter(ctx) {
                for s in op.deref(ctx).successors() {
                    rp.entry(s).or_default().push(b);
                }
                if let Some(&(l, _)) = st.invokes.get(op) {
                    rp.entry(l).or_default().push(b);
                }
                if Operation::is_op::<ReturnOp>(op, ctx)
                    || st.calls.get(op).is_some_and(|ci| ci.tail)
                {
                    if can_ret.insert(b) {
                        wl.push(b);
                    }
                }
            }
        }
        while let Some(b) = wl.pop() {
            for &p in rp.get(&b).into_iter().flatten() {
                if can_ret.insert(p) {
                    wl.push(p);
                }
            }
        }
        let mut os = Vec::new();
        for b in bs {
            let cr = can_ret.contains(&b);
            crate::inline::ops_into(ctx, b, &mut os);
            for (k, &op) in os.iter().enumerate() {
                macro_rules! is {
                    ($t:ty) => {
                        Operation::is_op::<$t>(op, ctx)
                    };
                }
                if is!(CallOp) {
                    let inv = st.invokes.contains(op);
                    let unr = os
                        .get(k + 1)
                        .is_some_and(|&n| Operation::is_op::<UnreachableOp>(n, ctx));
                    match direct_callee(ctx, st, op) {
                        Some(c) => {
                            p.call.push((f, ids.id(c), inv, unr));
                            if cr {
                                p.callr.push((f, ids.id(c), inv, unr));
                            }
                        }
                        None => {
                            p.icall.push((f, inv, unr));
                            if cr {
                                p.icallr.push((f, inv, unr));
                            }
                        }
                    }
                } else if (is!(StoreOp) && !frame_local(ctx, op.deref(ctx).get_operand(1)))
                    || is!(AtomicRmwOp)
                    || is!(AtomicCmpxchgOp)
                    || is!(FenceOp)
                    || is!(AtomicLoadOp)
                    || is!(AtomicStoreOp)
                    || is!(InlineAsmOp)
                    || is!(VAArgOp)
                    || (is!(CallIntrinsicOp)
                        && !st
                            .intrinsics.get(op)
                            .is_some_and(|n| PURE.iter().any(|p| n.starts_with(p))))
                {
                    if cr {
                        p.ret_write.push((f,));
                    } else {
                        p.dead_write.push((f,));
                    }
                }
            }
        }
    }
    p.run();
    let strict: FxHashSet<u32> = p.strict.iter().map(|&(f,)| f).collect();
    let on_return: FxHashSet<u32> = p.on_return.iter().map(|&(f,)| f).collect();
    // true: never writes; false: writes only on paths that end in a plain
    // noreturn call (a panic), so a normal return saw no write.
    let nw: FxHashMap<String, bool> = LIBC
        .iter()
        .map(|s| (s.to_string(), true))
        .chain(bodied.iter().filter_map(|s| {
            let f = ids.id(s);
            (!on_return.contains(&f)).then(|| (s.clone(), !strict.contains(&f)))
        }))
        .collect();
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        let strict = nw.values().filter(|&&v| v).count() - LIBC.len();
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats",
                "nowrite {}: {} of {} bodies write-free, {} more on return",
                st.cgu,
                strict,
                bodied.len(),
                nw.len() - LIBC.len() - strict
            )
        };
    }
    for s in &bodied {
        crate::dump_fact("nowrite", s, &nw.get(s));
    }
    st.nowrite = nw.into_iter().collect();
}

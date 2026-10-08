//! memcpyopt call-slot forwarding: `call f(sret %tmp); memcpy(%dst, %tmp, size(tmp))`
//! becomes `call f(sret %dst)` when `%tmp` is an alloca used only by the two ops and
//! `%dst` is a non-escaping alloca or the caller's own `sret` argument, untouched
//! between the call and the copy. `PLIRON_MEMCPYOPT=0` disables it.

use pliron::{
    context::{Context, Ptr},
    linked_list::{ContainsLinkedList, LinkedList},
    operation::Operation,
    value::Value,
};
use pliron_llvm::ops::{AllocaOp, GetElementPtrOp, LoadOp, StoreOp};

use crate::context::{ArgExt, State};
use crate::lower::has_body;
use crate::sroa::const_int;

/// Pointer `p` only flows into address operands of loads, stores, mem intrinsics and GEPs.
fn non_escaping(ctx: &Context, st: &State<'_>, p: Value) -> bool {
    p.uses(ctx).iter().all(|u| {
        let o = u.user_op();
        let i = u.find_index(ctx);
        if Operation::is_op::<LoadOp>(o, ctx) {
            true
        } else if Operation::is_op::<StoreOp>(o, ctx) {
            i == 1
        } else if Operation::is_op::<GetElementPtrOp>(o, ctx) {
            i == 0 && non_escaping(ctx, st, o.deref(ctx).get_result(0))
        } else {
            matches!(
                st.intrinsics.get(&o).map(|s| s.as_str()),
                Some("llvm.memcpy" | "llvm.memmove" | "llvm.memset")
            ) && i <= 1
        }
    })
}

/// `p` or a GEP derived from it is an operand of `op`.
fn touches(ctx: &Context, op: Ptr<Operation>, p: Value) -> bool {
    p.uses(ctx).iter().any(|u| {
        let o = u.user_op();
        o == op
            || (Operation::is_op::<GetElementPtrOp>(o, ctx)
                && touches(ctx, op, o.deref(ctx).get_result(0)))
    })
}

fn try_forward(
    ctx: &mut Context,
    st: &mut State<'_>,
    sret_args: &[Value],
    m: Ptr<Operation>,
) -> bool {
    if st.volatile.contains(&m) || st.intrinsics.get(&m).map(|s| s.as_str()) != Some("llvm.memcpy")
    {
        return false;
    }
    let o: Vec<Value> = m.deref(ctx).operands().collect();
    let (dst, src) = (o[0], o[1]);
    let Some(n) = const_int(ctx, st, o[2]) else {
        return false;
    };
    let Some(a) = src.defining_op() else {
        return false;
    };
    if !Operation::is_op::<AllocaOp>(a, ctx) || st.allocas.get(&src).map(|x| x.0 as i128) != Some(n)
    {
        return false;
    }
    let uses: Vec<_> = src
        .uses(ctx)
        .iter()
        .map(|u| (u.user_op(), u.find_index(ctx)))
        .collect();
    if uses.len() != 2 {
        return false;
    }
    let Some(&(call, idx)) = uses.iter().find(|(u, _)| *u != m) else {
        return false;
    };
    let Some(ci) = st.calls.get(&call) else {
        return false;
    };
    if st.invokes.contains_key(&call) {
        return false;
    }
    let nopd = call.deref(ctx).get_num_operands();
    let first_arg = nopd - ci.exts.params.len().min(nopd);
    if idx < first_arg || ci.exts.params.get(idx - first_arg) != Some(&ArgExt::SRet) {
        return false;
    }
    let block = m.deref(ctx).get_parent_block().unwrap();
    if call.deref(ctx).get_parent_block() != Some(block) {
        return false;
    }
    // dst must exist before the call and nothing may observe it until the copy.
    let dst_ok = if sret_args.contains(&dst) {
        non_escaping(ctx, st, dst)
    } else if let Some(d) = dst.defining_op() {
        Operation::is_op::<AllocaOp>(d, ctx)
            && st.allocas.get(&dst).is_some_and(|x| x.0 as i128 >= n)
            && non_escaping(ctx, st, dst)
            && dominates_in_block(ctx, d, call)
    } else {
        false
    };
    if !dst_ok {
        return false;
    }
    let mut cur = call.deref(ctx).get_next();
    while let Some(op) = cur {
        if op == m {
            break;
        }
        if touches(ctx, op, dst) {
            return false;
        }
        cur = op.deref(ctx).get_next();
    }
    if cur.is_none() {
        return false;
    }
    Operation::replace_operand(call, ctx, idx, dst);
    st.intrinsics.remove(&m);
    Operation::erase(m, ctx);
    st.allocas.remove(&src);
    st.promoted.remove(&src);
    Operation::erase(a, ctx);
    true
}

/// `d` is in the entry block, or precedes `user` in `user`'s block.
fn dominates_in_block(ctx: &Context, d: Ptr<Operation>, user: Ptr<Operation>) -> bool {
    let db = d.deref(ctx).get_parent_block().unwrap();
    let region = db.deref(ctx).get_parent_region().unwrap();
    if region.deref(ctx).get_head() == Some(db) {
        return true;
    }
    if user.deref(ctx).get_parent_block() != Some(db) {
        return false;
    }
    let mut cur = d.deref(ctx).get_next();
    while let Some(op) = cur {
        if op == user {
            return true;
        }
        cur = op.deref(ctx).get_next();
    }
    false
}

pub fn run(ctx: &mut Context, st: &mut State<'_>) {
    let mut n = 0;
    let funcs: Vec<_> = st
        .funcs
        .values()
        .map(|f| (f.op, f.exts.params.clone()))
        .collect();
    for (f, params) in funcs {
        if !has_body(ctx, f) {
            continue;
        }
        let Some(entry) = f.deref(ctx).get_region(0).deref(ctx).get_head() else {
            continue;
        };
        let sret_args: Vec<Value> = params
            .iter()
            .enumerate()
            .filter(|(i, e)| **e == ArgExt::SRet && *i < entry.deref(ctx).get_num_arguments())
            .map(|(i, _)| entry.deref(ctx).get_argument(i))
            .collect();
        let blocks: Vec<_> = f.deref(ctx).get_region(0).deref(ctx).iter(ctx).collect();
        for b in blocks {
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            for op in ops {
                n += try_forward(ctx, st, &sret_args, op) as usize;
            }
        }
    }
    if std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("memcpyopt {}: {n} call slots forwarded", st.cgu);
    }
}

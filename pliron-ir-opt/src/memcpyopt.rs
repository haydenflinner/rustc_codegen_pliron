//! memcpyopt call-slot forwarding: `call f(sret %tmp); memcpy(%dst, %tmp, size(tmp))`
//! becomes `call f(sret %dst)` when `%tmp` is an alloca used only by the two ops and
//! `%dst` is a non-escaping alloca or the caller's own `sret` argument, untouched
//! between the call and the copy. `PLIRON_MEMCPYOPT=0` disables it.

use pliron::{
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    operation::Operation,
    value::Value,
};
use pliron::r#type::Typed;
use pliron_llvm::ops::{AllocaOp, CallOp, GetElementPtrOp, LoadOp, StoreOp};
use rustc_hash::{FxHashMap, FxHashSet};
use pliron::basic_block::BasicBlock;

use crate::opt_body;
use crate::sroa::{const_int, gep_offset};
use crate::state::{ArgExt, State};
use crate::types::size_align;

/// Pointer `p` only flows into address operands of loads, stores, mem intrinsics and GEPs.
fn non_escaping(ctx: &Context, st: &State, p: Value) -> bool {
    p.uses_iter(ctx).all(|u| {
        let o = u.user_op();
        let i = u.find_index(ctx);
        if Operation::is_op::<LoadOp>(o, ctx) {
            true
        } else if Operation::is_op::<StoreOp>(o, ctx) {
            i == 1
        } else if Operation::is_op::<GetElementPtrOp>(o, ctx) {
            i == 0 && non_escaping(ctx, st, o.deref(ctx).get_result(ctx, 0))
        } else {
            matches!(
                st.intrinsics.get(o).map(|s| s.as_str()),
                Some("llvm.memcpy" | "llvm.memmove" | "llvm.memset")
            ) && i <= 1
        }
    })
}

/// `p` or a GEP derived from it is an operand of `op`.
fn touches(ctx: &Context, op: Ptr<Operation>, p: Value) -> bool {
    p.uses_iter(ctx).any(|u| {
        let o = u.user_op();
        o == op
            || (Operation::is_op::<GetElementPtrOp>(o, ctx)
                && touches(ctx, op, o.deref(ctx).get_result(ctx, 0)))
    })
}

fn try_forward(ctx: &mut Context, st: &mut State, sret_args: &[Value], m: Ptr<Operation>) -> bool {
    if st.volatile.contains(m) || st.intrinsics.get(m).map(|s| s.as_str()) != Some("llvm.memcpy")
    {
        return false;
    }
    let o: Vec<Value> = m.deref(ctx).operands(ctx).collect();
    let (dst, src) = (o[0], o[1]);
    let Some(n) = const_int(ctx, st, o[2]) else {
        return false;
    };
    let Some(a) = src.defining_op() else {
        return false;
    };
    if !Operation::is_op::<AllocaOp>(a, ctx) || st.allocas.get(src).map(|x| x.0 as i128) != Some(n)
    {
        return false;
    }
    let uses: Vec<_> = src
        .uses_iter(ctx)
        .map(|u| (u.user_op(), u.find_index(ctx)))
        .collect();
    if uses.len() != 2 {
        return false;
    }
    let Some(&(call, idx)) = uses.iter().find(|(u, _)| *u != m) else {
        return false;
    };
    let Some(ci) = st.calls.get(call) else {
        return false;
    };
    if st.invokes.contains(call) {
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
            && st.allocas.get(dst).is_some_and(|x| x.0 as i128 >= n)
            && non_escaping(ctx, st, dst)
            && dominates_in_block(ctx, d, call)
    } else {
        false
    };
    if !dst_ok {
        return false;
    }
    let mut cur = call.get_next(ctx);
    while let Some(op) = cur {
        if op == m {
            break;
        }
        if touches(ctx, op, dst) {
            return false;
        }
        cur = op.get_next(ctx);
    }
    if cur.is_none() {
        return false;
    }
    Operation::replace_operand(call, ctx, idx, dst);
    st.intrinsics.remove(m);
    Operation::erase(m, ctx);
    st.allocas.remove(src);
    st.promoted.remove(src);
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
    let mut cur = d.get_next(ctx);
    while let Some(op) = cur {
        if op == user {
            return true;
        }
        cur = op.get_next(ctx);
    }
    false
}

/// The alloca `v` points into and the constant byte offset, through a chain
/// of constant GEPs.
fn alloca_base(ctx: &Context, st: &State, mut v: Value) -> Option<(Value, u64)> {
    let mut off: i64 = 0;
    for _ in 0..16 {
        let d = v.defining_op()?;
        if Operation::is_op::<AllocaOp>(d, ctx) {
            return (st.allocas.contains(v) && off >= 0).then_some((v, off as u64));
        }
        if !Operation::is_op::<GetElementPtrOp>(d, ctx) {
            return None;
        }
        off = off.checked_add(gep_offset(ctx, st, d)?)?;
        v = d.deref(ctx).get_operand(ctx, 0);
    }
    None
}

// `PLIRON_TRACE=pliron::memcpyopt=debug`: why a by-value copy was not
// forwarded (`nope!` inside `home_users`, `bail!` in `try_byval`).
macro_rules! nope {
    ($($t:tt)*) => {{
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::memcpyopt", "byval: {}", format!($($t)*));
        }
        return None;
    }};
}
macro_rules! bail {
    ($($t:tt)*) => {{
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::memcpyopt", "byval: {}", format!($($t)*));
        }
        return false;
    }};
}

/// One op's touch of a home alloca: the byte range it may read and/or
/// write, or a lifetime marker on the whole object (`kill`: the bytes are
/// undefined past it).
#[derive(Clone, Copy)]
struct Touch {
    read: bool,
    write: bool,
    lo: u64,
    hi: u64,
    kill: bool,
}

/// Every op that touches alloca `a` (through constant GEPs) with the byte
/// ranges it may read or write; `None` when `a`'s address can leave the
/// function's view: stored as a value, turned into an integer, compared,
/// carried by a block argument, or passed to a call that may retain it
/// (neither rustc's `captures(none)` nor our own `nocapture` inference
/// clears the operand) or whose reach is unknown (a raw pointer).
fn home_users(
    ctx: &Context,
    st: &State,
    a: Value,
    size: u64,
) -> Option<FxHashMap<Ptr<Operation>, Vec<Touch>>> {
    let mut out: FxHashMap<Ptr<Operation>, Vec<Touch>> = FxHashMap::default();
    let mut work = vec![(a, 0u64)];
    let mut seen = 0usize;
    let touch = |read, write, lo: u64, n: u64| Touch {
        read,
        write,
        lo,
        hi: lo.saturating_add(n),
        kill: false,
    };
    while let Some((p, base)) = work.pop() {
        for u in p.uses_iter(ctx) {
            seen += 1;
            if seen > 512 {
                nope!("home: gep as a non-base operand");
            }
            let op = u.user_op();
            let i = u.find_index(ctx);
            if Operation::is_op::<GetElementPtrOp>(op, ctx) {
                if i != 0 {
                    nope!("home: gep past the object");
                }
                let o = gep_offset(ctx, st, op)?;
                let nb = (base as i64).checked_add(o)?;
                if nb < 0 || nb as u64 > size {
                    nope!("home: address stored as a value");
                }
                work.push((op.deref(ctx).get_result(ctx, 0), nb as u64));
            } else if Operation::is_op::<LoadOp>(op, ctx) {
                let w = size_align(ctx, op.deref(ctx).get_result(ctx, 0).get_type(ctx)).0;
                out.entry(op).or_default().push(touch(true, false, base, w));
            } else if Operation::is_op::<StoreOp>(op, ctx) {
                if i != 1 {
                    nope!("home: memset byte operand");
                }
                let w = size_align(ctx, op.deref(ctx).get_operand(ctx, 0).get_type(ctx)).0;
                out.entry(op).or_default().push(touch(false, true, base, w));
            } else if let Some(name) = st.intrinsics.get(op).map(|s| s.as_str()) {
                let opnds: Vec<Value> = op.deref(ctx).operands(ctx).collect();
                match name {
                    "llvm.lifetime.start" | "llvm.lifetime.end" => {
                        // A marker on the object itself with its full size
                        // (or no size) ends every byte's value; one on a
                        // field says nothing about the rest.
                        let whole = i == 1
                            && base == 0
                            && const_int(ctx, st, opnds[0]).is_none_or(|n| n < 0 || n as u64 >= size);
                        out.entry(op).or_default().push(Touch {
                            read: false,
                            write: false,
                            lo: 0,
                            hi: 0,
                            kill: whole,
                        });
                    }
                    "llvm.memcpy" | "llvm.memmove" | "llvm.memset" if i <= 1 => {
                        // memset's operand 1 is the byte, not a pointer.
                        if i == 1 && name == "llvm.memset" {
                            nope!("home: unknown intrinsic");
                        }
                        let n = const_int(ctx, st, opnds[2])
                            .and_then(|n| u64::try_from(n).ok())
                            .unwrap_or(size);
                        out.entry(op).or_default().push(touch(i == 1, i == 0, base, n));
                    }
                    _ => return None,
                }
            } else if let Some(c) = Operation::get_op::<CallOp>(op, ctx) {
                let direct = c.get_attr_llvm_call_callee(ctx).is_some();
                if !direct && i == 0 {
                    nope!("home: indirect callee operand");
                }
                let ai = i - usize::from(!direct);
                let info = st.calls.get(op)?;
                // Drop glue on the home (`drop_in_place`, `drop_glue<T>`):
                // a drop retains nothing, and it never reads the bytes a
                // by-value callee wrote. The local was moved into that
                // call (its drop flag is false from there until a new
                // value is stored) or it is `Copy` and has no drop glue; a
                // partially moved struct drops its remaining fields one by
                // one. An inlined drop shows up as plain loads and counts.
                let drop = info
                    .callee
                    .as_deref()
                    .is_some_and(|s| s.contains("4core3ptr9drop_glue") || s.contains("13drop_in_place"));
                let nocap = drop
                    || info.nocap.as_ref().is_some_and(|n| n.get(ai) == Some(&true))
                    || c
                        .get_attr_llvm_call_callee(ctx)
                        .and_then(|id| st.ident_to_sym.get(AsRef::<str>::as_ref(id.as_ref())))
                        .and_then(|sym| st.nocapture.get(sym))
                        .is_some_and(|s| s.contains(&(ai as u32)));
                let (n, _) = info.arg_extent.as_ref().and_then(|e| e.get(ai)).copied()?;
                if !nocap || n == 0 {
                    nope!("home: call operand may retain or has no extent ({:?} #{ai})", info.callee);
                }
                out.entry(op).or_default().push(touch(!drop, !drop, base, n));
            } else {
                nope!("home: address escapes into {}", Operation::get_opid(op, ctx));
            }
        }
    }
    Some(out)
}

/// Bytes `[lo, hi)` of a home alloca are dead after `call`: on every path
/// from it they are fully overwritten, or the object's lifetime ends, or
/// the function leaves, before any read of them. Reads and writes are the
/// alloca's own `users` — nothing else can reach its bytes. A budget bounds
/// the walk; running out means "not shown dead".
fn dead_after(
    ctx: &Context,
    st: &State,
    users: &FxHashMap<Ptr<Operation>, Vec<Touch>>,
    call: Ptr<Operation>,
    lo: u64,
    hi: u64,
) -> Option<bool> {
    const MAX_BLOCKS: usize = 64;
    let mut rewritten = false;
    const MAX_OPS: usize = 4096;
    let mut ops_seen = 0usize;
    let mut visited: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
    // (block, first op to scan; `None` = from the block's top)
    let mut work: Vec<(Ptr<BasicBlock>, Option<Ptr<Operation>>)> = Vec::new();
    let Some(cb) = call.deref(ctx).get_parent_block() else {
        return None;
    };
    work.push((cb, call.get_next(ctx)));
    if let Some(&(pad, _)) = st.invokes.get(call) {
        work.push((pad, None));
    }
    while let Some((b, from)) = work.pop() {
        if from.is_none() && !visited.insert(b) {
            continue;
        }
        if visited.len() > MAX_BLOCKS {
            return None;
        }
        let mut cur = match from {
            Some(op) => Some(op),
            None => b.deref(ctx).get_head(),
        };
        let mut stopped = false;
        let mut last = None;
        while let Some(op) = cur {
            ops_seen += 1;
            if ops_seen > MAX_OPS {
                return None;
            }
            last = Some(op);
            if let Some(ts) = users.get(&op) {
                if ts.iter().any(|t| t.read && t.lo < hi && lo < t.hi) {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::memcpyopt", "byval: home read after the call by {} {:?}",
                            Operation::get_opid(op, ctx), st.calls.get(op).and_then(|c| c.callee.clone()));
                    }
                    return None;
                }
                if ts.iter().any(|t| t.kill || (t.write && t.lo <= lo && hi <= t.hi)) {
                    rewritten |= ts.iter().any(|t| t.write && t.lo <= lo && hi <= t.hi);
                    stopped = true;
                    break;
                }
            }
            if let Some(&(pad, _)) = st.invokes.get(op) {
                work.push((pad, None));
            }
            cur = op.get_next(ctx);
        }
        if stopped {
            continue;
        }
        // Fell off the block's end: its terminator's successors.
        if let Some(t) = last {
            for s in t.deref(ctx).successors(ctx) {
                work.push((s, None));
            }
        }
    }
    Some(rewritten)
}

/// `PLIRON_BYVALHOME=1`: `memcpy(%t, %s, n); call f(.., %t, ..)`, where `%t`
/// is a fresh `n`-byte alloca the callee takes by value and never writes
/// (`CallInfo::arg_byval_ro`, rustc's deduced `readonly`), becomes
/// `call f(.., %s, ..)`. `%s` points `off` bytes into an alloca `%a` whose
/// address never leaves the function's view (`home_users`), and no write
/// reaches `%a`'s bytes between the copy and the call, so the callee reads
/// exactly the bytes the copy would have carried and leaves them as they
/// were. `%a` goes into `State::homes` so SROA keeps it one stack object:
/// split, it would scalarize every other access and copy the scalars into
/// a fresh temp at this very call — nine loop-carried SSA values for
/// regex-syntax's `concat = self.push_group(concat)?`.
fn try_byval(ctx: &mut Context, st: &mut State, m: Ptr<Operation>) -> bool {
    if st.volatile.contains(m) || st.intrinsics.get(m).map(|s| s.as_str()) != Some("llvm.memcpy")
    {
        return false;
    }
    let o: Vec<Value> = m.deref(ctx).operands(ctx).collect();
    let (dst, src) = (o[0], o[1]);
    let Some(n) = const_int(ctx, st, o[2]).and_then(|n| u64::try_from(n).ok()) else {
        bail!("copy length not constant");
    };
    let Some(t_op) = dst.defining_op() else {
        return false;
    };
    if n == 0 || !Operation::is_op::<AllocaOp>(t_op, ctx) {
        return false;
    }
    let Some(&(tsize, talign)) = st.allocas.get(dst) else {
        return false;
    };
    if tsize != n || talign == 0 {
        bail!("temp size {tsize} != copy length {n}");
    }
    // `%t`'s uses: this copy as destination, lifetime markers, one call
    // operand.
    let mut call = None;
    let mut markers = Vec::new();
    for u in dst.uses_iter(ctx) {
        let op = u.user_op();
        let i = u.find_index(ctx);
        if op == m {
            if i != 0 {
                bail!("temp is the copy source");
            }
            continue;
        }
        if matches!(
            st.intrinsics.get(op).map(|s| s.as_str()),
            Some("llvm.lifetime.start" | "llvm.lifetime.end")
        ) {
            markers.push(op);
            continue;
        }
        if call.is_some() || !Operation::is_op::<CallOp>(op, ctx) {
            bail!("temp has another use: {}", Operation::get_opid(op, ctx));
        }
        call = Some((op, i));
    }
    let Some((call, idx)) = call else {
        return false;
    };
    let Some(c) = Operation::get_op::<CallOp>(call, ctx) else {
        bail!("temp use is not a call");
    };
    let direct = c.get_attr_llvm_call_callee(ctx).is_some();
    if !direct && idx == 0 {
        bail!("temp is the callee");
    }
    let ai = idx - usize::from(!direct);
    let Some(ci) = st.calls.get(call) else {
        bail!("call without CallInfo");
    };
    let ro = ci.arg_byval_ro.as_ref().is_some_and(|v| v.get(ai) == Some(&true));
    let ext = ci.arg_extent.as_ref().and_then(|e| e.get(ai)).copied();
    if ext.map(|e| e.0) != Some(n) {
        bail!("operand extent {ext:?} is not the copy length {n}");
    }
    let block = m.deref(ctx).get_parent_block().unwrap();
    if call.deref(ctx).get_parent_block() != Some(block) {
        bail!("call in another block");
    }
    let Some((a, off)) = alloca_base(ctx, st, src) else {
        bail!("source is not a constant offset into an alloca");
    };
    let Some(&(asize, aalign)) = st.allocas.get(a) else {
        return false;
    };
    if a == dst
        || st.promoted.contains(a)
        || off.checked_add(n).is_none_or(|e| e > asize)
        || aalign < talign
        || off % talign != 0
    {
        bail!("home range/alignment/promotion");
    }
    let Some(users) = home_users(ctx, st, a, asize) else {
        bail!("home escapes");
    };
    // Another operand of the same call reaching into `%a`: the callee
    // could write the bytes it reads.
    if users.contains_key(&call) {
        bail!("another operand of the call reaches the home");
    }
    // No write into `[off, off+n)` between the copy and the call. Ops that
    // don't touch `%a` can't reach its bytes — its address never escaped.
    let mut cur = m.get_next(ctx);
    loop {
        let Some(op) = cur else {
            bail!("block ends before the call");
        };
        if op == call {
            break;
        }
        if let Some(ts) = users.get(&op)
            && ts.iter().any(|t| t.write && t.lo < off + n && off < t.hi)
        {
            bail!("home written between the copy and the call");
        }
        cur = op.get_next(ctx);
    }
    // The callee may write its by-value copy — now `%a`'s bytes — unless
    // rustc deduced the param `readonly`; otherwise those bytes must be
    // dead after the call (MIR moved the local out: nothing reads it
    // before a whole new value is stored).
    // The callee may write its by-value copy — now `%a`'s bytes — unless
    // rustc deduced the param `readonly`; otherwise those bytes must be
    // dead after the call (MIR moved the local out: nothing reads it
    // before a whole new value is stored). Either way the local must get
    // a new value on some path after the call: that is the loop-carried
    // shape this pays for. A temp that is consumed and never rewritten
    // (`let c = make(); consume(c)`) promotes better: its scalars feed
    // the copy directly and the call's is the only copy there is.
    let dead = dead_after(ctx, st, &users, call, off, off + n);
    match dead {
        Some(true) => {}
        Some(false) if n >= 64 => {}
        Some(false) => bail!("home not rewritten after the call"),
        None if ro => bail!("readonly callee, home not shown rewritten"),
        None => bail!("home read after the call before a new value"),
    }
    if pliron_cfg::tracing_on() {
        tracing::debug!(target: "pliron::memcpyopt", "byval: homed n {n} off {off} asize {asize} ro {ro} callee {:?}", ci.callee);
    }
    Operation::replace_operand(call, ctx, idx, src);
    for mk in markers {
        st.intrinsics.remove(mk);
        Operation::erase(mk, ctx);
    }
    st.intrinsics.remove(m);
    Operation::erase(m, ctx);
    st.allocas.remove(dst);
    st.promoted.remove(dst);
    Operation::erase(t_op, ctx);
    st.homes.insert(a);
    true
}

pub fn run(ctx: &mut Context, st: &mut State) {
    let mut n = 0;
    let funcs: Vec<_> = st
        .funcs
        .values()
        .map(|f| (f.op, f.exts.params.clone()))
        .collect();
    for (f, params) in funcs {
        if !opt_body(ctx, st, f) {
            continue;
        }
        let Some(entry) = f.deref(ctx).get_region(ctx, 0).deref(ctx).get_head() else {
            continue;
        };
        let sret_args: Vec<Value> = params
            .iter()
            .enumerate()
            .filter(|(i, e)| **e == ArgExt::SRet && *i < entry.deref(ctx).get_num_arguments())
            .map(|(i, _)| entry.deref(ctx).get_argument(ctx, i))
            .collect();
        let blocks: Vec<_> = f.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx).collect();
        for b in blocks {
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            for op in ops {
                n += try_forward(ctx, st, &sret_args, op) as usize;
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats","memcpyopt {}: {n} call slots forwarded", st.cgu)
        };
    }
}

/// `PLIRON_BYVALHOME=1`: [`try_byval`] over every function. Runs right
/// before SROA — after `phisimp` has removed the block arguments the
/// inliner bound callee params through (a home alloca's address must not
/// flow through one), and after the `nocapture` inference `home_users`
/// consults for the home's other call operands.
pub fn run_byval(ctx: &mut Context, st: &mut State) {
    let mut n = 0;
    let funcs: Vec<_> = st.funcs.values().map(|f| f.op).collect();
    for f in funcs {
        if !opt_body(ctx, st, f) {
            continue;
        }
        let blocks: Vec<_> = f.deref(ctx).get_region(ctx, 0).deref(ctx).iter(ctx).collect();
        for b in blocks {
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            for op in ops {
                n += try_byval(ctx, st, op) as usize;
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats","byvalhome {}: {n} by-value args homed", st.cgu)
        };
    }
}

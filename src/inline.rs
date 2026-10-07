//! Pliron-level inliner. Cranelift does no inlining, so small callees defined
//! in the same CGU are copied into their (non-invoke) call sites before lowering.
//! `PLIRON_INLINE=<max ops>` tunes the size limit; `0` disables it.

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::context::{Context, Ptr};
use pliron::irbuild::cloning::{IrMapping, clone_blocks_into};
use pliron::irbuild::inserter::OpInsertionPoint;
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::{IRRewriter, Rewriter};
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron_llvm::ops::{BrOp, CallOp, ReturnOp};
use rustc_data_structures::fx::FxHashMap;

use crate::context::State;
use crate::lower::has_body;

const DEFAULT_LIMIT: usize = 40;

fn blocks(ctx: &Context, f: Ptr<Operation>) -> Vec<Ptr<BasicBlock>> {
    f.deref(ctx).get_region(0).deref(ctx).iter(ctx).collect()
}

fn ops(ctx: &Context, b: Ptr<BasicBlock>) -> Vec<Ptr<Operation>> {
    b.deref(ctx).iter(ctx).collect()
}

fn direct_callee<'a>(ctx: &Context, st: &'a State<'_>, op: Ptr<Operation>) -> Option<&'a String> {
    let c = Operation::get_op::<CallOp>(op, ctx)?;
    let CallOpCallable::Direct(id) = c.callee(ctx) else { return None };
    st.ident_to_sym.get(&id.to_string())
}

pub fn run(ctx: &mut Context, st: &mut State<'_>) {
    let limit = std::env::var("PLIRON_INLINE").ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_LIMIT);
    if limit == 0 {
        return;
    }
    let mut eligible: FxHashMap<String, Ptr<Operation>> = FxHashMap::default();
    for (sym, f) in &st.funcs {
        if f.no_inline || f.linkage == Linkage::Preemptible || !has_body(ctx, f.op) {
            continue;
        }
        let (mut n, mut ok) = (0, true);
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
                n += 1;
                let eh_or_va =
                    st.intrinsics.get(&op).is_some_and(|s| s.starts_with("pliron.eh") || s.starts_with("llvm.va_"));
                if eh_or_va || st.invokes.contains_key(&op) || direct_callee(ctx, st, op) == Some(sym) {
                    ok = false;
                }
            }
        }
        if ok && n <= if f.always_inline { limit * 10 } else { limit } {
            eligible.insert(sym.clone(), f.op);
        }
    }
    if eligible.is_empty() {
        return;
    }
    let callers: Vec<Ptr<Operation>> = st.funcs.values().map(|f| f.op).filter(|&f| has_body(ctx, f)).collect();
    let mut rw = IRRewriter::<DummyListener>::default();
    for caller in callers {
        let mut sites = Vec::new();
        for b in blocks(ctx, caller) {
            for op in ops(ctx, b) {
                if st.invokes.contains_key(&op) {
                    continue;
                }
                let Some(sym) = direct_callee(ctx, st, op) else { continue };
                let Some(&callee) = eligible.get(sym) else { continue };
                if callee != caller && st.calls[&op].fn_ty == st.funcs[sym].ty {
                    sites.push((op, callee));
                }
            }
        }
        for (call, callee) in sites {
            inline_call(ctx, st, &mut rw, call, callee);
        }
    }
}

fn inline_call(
    ctx: &mut Context,
    st: &mut State<'_>,
    rw: &mut IRRewriter<DummyListener>,
    call: Ptr<Operation>,
    callee: Ptr<Operation>,
) {
    let block = call.deref(ctx).get_parent_block().unwrap();
    let region = block.deref(ctx).get_parent_region().unwrap();
    let args = Operation::get_op::<CallOp>(call, ctx).unwrap().args(ctx);
    let res = (call.deref(ctx).get_num_results() > 0).then(|| call.deref(ctx).get_result(0));
    let cont = rw.split_block(ctx, block, OpInsertionPoint::AfterOperation(call), None);

    let src = blocks(ctx, callee);
    let mut map = IrMapping::new();
    clone_blocks_into(&src, region, ctx, rw, &mut map);
    for &b in &src {
        let nb = map.lookup_block(b).unwrap();
        for (op, new) in ops(ctx, b).into_iter().zip(ops(ctx, nb)) {
            if let Some(c) = st.calls.get(&op).cloned() {
                st.calls.insert(new, c);
            }
            if let Some(s) = st.intrinsics.get(&op).cloned() {
                st.intrinsics.insert(new, s);
            }
            if let Some(r) = st.rmw.get(&op).copied() {
                st.rmw.insert(new, r);
            }
            let n = op.deref(ctx).get_num_results();
            for i in 0..n {
                let (v, nv) = (op.deref(ctx).get_result(i), new.deref(ctx).get_result(i));
                if let Some(a) = st.allocas.get(&v).copied() {
                    st.allocas.insert(nv, a);
                }
            }
            if let Some(r) = Operation::get_op::<ReturnOp>(new, ctx) {
                let rv: Vec<_> = r.retval(ctx).into_iter().collect();
                let br = BrOp::new(ctx, cont, rv).get_operation();
                br.insert_before(ctx, new);
                Operation::erase(new, ctx);
            }
        }
    }
    if let Some(res) = res {
        let ty = res.get_type(ctx);
        BasicBlock::push_argument(cont, ctx, ty);
        let arg = cont.deref(ctx).get_argument(0);
        res.replace_all_uses_with(ctx, &arg);
    }
    let entry = map.lookup_block(src[0]).unwrap();
    let br = BrOp::new(ctx, entry, args).get_operation();
    br.insert_before(ctx, call);
    st.calls.remove(&call);
    Operation::erase(call, ctx);
}

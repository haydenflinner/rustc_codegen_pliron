//! Argument specialization: LLVM IPSCCP-lite.
//!
//! When every direct callsite of a `Local` function passes the same constant
//! for a parameter — and the function's address never escapes — the shared
//! body can adopt that constant. Each such param's uses are replaced by the
//! callsite `Value`: constants are orphan `UndefOp` results that materialize
//! wherever they're used (`get()`/`mat()`), so nothing else has to move. The
//! signature and the calls stay; the win is the constant propagation every
//! downstream pass already performs on `st.consts` values.

use cranelift_module::Linkage;
use pliron::builtin::op_interfaces::CallOpInterface;
use pliron::context::{Context, Ptr};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::ops::{CallOp, UndefOp};
use rustc_data_structures::fx::FxHashMap;

use crate::context::State;
use crate::inline::{blocks, direct_callee, ops};
use crate::lower::has_body;

pub fn run(ctx: &mut Context, st: &mut State<'_>) {
    let dbg = std::env::var_os("PLIRON_SPEC_DEBUG").is_some();
    // Direct callsites per callee.
    let mut sites: FxHashMap<String, Vec<Ptr<Operation>>> = FxHashMap::default();
    for f in st.funcs.values() {
        if !has_body(ctx, f.op) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
                if let Some(sym) = direct_callee(ctx, st, op) {
                    sites.entry(sym.clone()).or_default().push(op);
                }
            }
        }
    }
    // Symbols whose address escapes: an unseen caller could pass different
    // arguments.
    let taken = crate::lower::address_taken(ctx, st);
    let mut done = 0usize;
    for (sym, f) in &st.funcs {
        if dbg {
            eprintln!(
                "spec {sym}: linkage={:?} body={} taken={} sites={:?}",
                f.linkage,
                has_body(ctx, f.op),
                taken.contains(sym.as_str()),
                sites.get(sym).map(|v| v.len())
            );
        }
        if f.linkage != Linkage::Local
            || !has_body(ctx, f.op)
            || taken.contains(sym.as_str())
            || st.dead_fns.contains(sym.as_str())
        {
            continue;
        }
        let Some(calls) = sites.get(sym) else {
            continue;
        };
        let params: Vec<Value> = blocks(ctx, f.op)[0]
            .deref(ctx)
            .arguments()
            .collect();
        let args: Vec<Vec<Value>> = calls
            .iter()
            .map(|&c| Operation::get_op::<CallOp>(c, ctx).unwrap().args(ctx))
            .collect();
        for (i, p) in params.iter().enumerate() {
            let Some(a0) = args[0].get(i) else { break };
            let Some(c0) = st.consts.get(a0).cloned() else {
                continue;
            };
            if a0.get_type(ctx) != p.get_type(ctx) {
                continue;
            }
            if !args
                .iter()
                .all(|a| a.get(i).is_some_and(|&v| st.consts.get(&v) == Some(&c0)))
            {
                continue;
            }
            // Fresh orphan const: `st.consts` also maps some in-block
            // results (inline clones), whose defs can't migrate across
            // functions; an unattached `UndefOp` can be used anywhere.
            let nv = UndefOp::new(ctx, p.get_type(ctx))
                .get_operation()
                .deref(ctx)
                .get_result(0);
            st.consts.insert(nv, c0);
            p.replace_all_uses_with(ctx, &nv);
            done += 1;
        }
    }
    if done > 0 && std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("spec {}: {done} params", st.cgu);
    }
}

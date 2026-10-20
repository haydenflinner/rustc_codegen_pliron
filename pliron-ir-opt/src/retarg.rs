//! Bottom-up `returned`-argument inference (LLVM FunctionAttrs' `returned`):
//! a body whose every `ret` returns the same parameter — literally, or
//! transitively through a call to a `returned(k)` callee whose k-th argument
//! is that parameter (a select of two arms mapping to the same index also
//! qualifies) — returns its k-th argument unchanged. Call results then fold
//! to the passed operand, unblocking the clif-level value passes on
//! wrapper/`#[inline(never)]` shim patterns that survive the inliner.
//!
//! Values start unmapped and only gain a parameter index, so the table grows
//! monotonically to a (least) fixpoint; unknown/conflicting returns simply
//! keep a function out of it. Foreign, bodiless and preemptible functions are
//! never inferred.

use pliron::builtin::op_interfaces::CallOpInterface;
use pliron::context::Context;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::ops::{CallOp, ReturnOp, SelectOp};
use rustc_hash::FxHashMap;

use crate::inline::{blocks, direct_callee};
use crate::state::State;
use crate::{Syms, bodied, has_body};

ascent::ascent! {
    struct Returned;
    /// Entry param `i` is `v`.
    relation param(Value, u32);
    /// `r` is the result of a direct call to `c` whose argument `j` is `a`.
    relation call(u32, Value, u32, Value);
    /// `r` selects between `a` and `b`.
    relation select(Value, Value, Value);
    /// The `i`-th `ret` of `f` returns `v`; `f` has `n` `ret`s.
    relation ret(u32, u32, Value);
    relation nrets(u32, u32);

    /// `v` provably equals param `k` of its function.
    relation is(Value, u32);
    is(v, i) <-- param(v, i);
    is(r, k) <-- call(c, r, j, a), returned(c, j), is(a, k);
    is(r, k) <-- select(r, a, b), is(a, k), is(b, k);

    /// `f`'s `ret`s up to the `i`-th all return param `k`.
    relation upto(u32, u32, u32);
    upto(f, 0, k) <-- ret(f, 0, v), is(v, k);
    upto(f, i + 1, k) <-- upto(f, i, k), ret(f, i + 1, v), is(v, k);

    /// Every `ret` of `f` returns its param `k`.
    relation returned(u32, u32);
    returned(f, k) <-- nrets(f, n), upto(f, n - 1, k);
}

pub fn run(ctx: &Context, st: &State) {
    let bodied = bodied(ctx, st);
    let mut ids = Syms::default();
    let mut p = Returned::default();
    let mut scratch = Vec::new();
    for s in &bodied {
        let f = ids.id(s);
        let bs = blocks(ctx, st.funcs[s].op);
        for (i, a) in bs[0].deref(ctx).arguments(ctx).enumerate() {
            p.param.push((a, i as u32));
        }
        let mut n = 0;
        for b in bs {
            crate::inline::ops_into(ctx, b, &mut scratch);
            for &op in &scratch {
                let d = op.deref(ctx);
                if let Some(r) = Operation::get_op::<ReturnOp>(op, ctx) {
                    // A valueless `ret` breaks the chain.
                    if let Some(v) = r.retval(ctx) {
                        p.ret.push((f, n, v));
                    }
                    n += 1;
                } else if Operation::is_op::<SelectOp>(op, ctx) {
                    p.select
                        .push((d.get_result(ctx, 0), d.get_operand(ctx, 1), d.get_operand(ctx, 2)));
                } else if Operation::is_op::<CallOp>(op, ctx)
                    && let Some(r) = d.results(ctx).next()
                    && let Some(c) = direct_callee(ctx, st, op)
                {
                    let c = ids.id(c);
                    let args = Operation::get_op::<CallOp>(op, ctx).unwrap().args(ctx);
                    for (j, a) in args.into_iter().enumerate() {
                        p.call.push((c, r, j as u32, a));
                    }
                }
            }
        }
        if n > 0 {
            p.nrets.push((f, n));
        }
    }
    p.run();
    let by_id: FxHashMap<u32, u32> = p.returned.iter().copied().collect();
    let ra: FxHashMap<String, u32> = bodied
        .iter()
        .filter_map(|s| by_id.get(&ids.id(s)).map(|&k| (s.clone(), k)))
        .collect();
    for s in &bodied {
        crate::dump_fact("retarg", s, &ra.get(s));
    }
    // Consume: a `returned(k)` call's result is the k-th operand.
    let mut folded = 0usize;
    for f in st.funcs.values() {
        // `dead_unopt` copies are emit-only — folding calls inside them
        // is pure waste.
        if !has_body(ctx, f.op) || st.dead_unopt_ops.contains(&f.op) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            crate::inline::ops_into(ctx, b, &mut scratch);
            for &op in &scratch {
                if !Operation::is_op::<CallOp>(op, ctx) || op.deref(ctx).get_num_results() == 0 {
                    continue;
                }
                let Some(&k) = direct_callee(ctx, st, op).and_then(|c| ra.get(c)) else {
                    continue;
                };
                let call = Operation::get_op::<CallOp>(op, ctx).unwrap();
                let Some(&a) = call.args(ctx).get(k as usize) else {
                    continue;
                };
                let r = op.deref(ctx).get_result(ctx, 0);
                // Guard: a type mismatch would mean the inference lied.
                if r.get_type(ctx) == a.get_type(ctx) {
                    r.replace_all_uses_with(ctx, &a);
                    folded += 1;
                }
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats",
                "retarg {}: {} returned-arg bodies, {folded} call results folded",
                st.cgu,
                ra.len()
            )
        };
    }
}

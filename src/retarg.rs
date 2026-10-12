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

use cranelift_module::Linkage;
use pliron::builtin::op_interfaces::CallOpInterface;
use pliron::context::{Context, Ptr};
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::ops::{CallOp, ReturnOp, SelectOp};
use rustc_hash::FxHashMap;

use crate::context::State;
use crate::inline::{blocks, direct_callee, ops};
use crate::lower::has_body;

/// `f`'s values mapped to the parameter index they provably equal: the
/// params themselves, results of calls to `returned(k)` callees passing a
/// mapped value at k, and selects whose arms agree. Grows to a fixpoint
/// (op order isn't def-first across blocks).
fn param_map(
    ctx: &Context,
    st: &State<'_>,
    f: Ptr<Operation>,
    ra: &FxHashMap<String, u32>,
) -> FxHashMap<Value, u32> {
    let bs = blocks(ctx, f);
    let mut m: FxHashMap<Value, u32> = FxHashMap::default();
    for (i, a) in bs[0].deref(ctx).arguments().enumerate() {
        m.insert(a, i as u32);
    }
    loop {
        let mut changed = false;
        for &b in &bs {
            for op in ops(ctx, b) {
                let add = if Operation::is_op::<CallOp>(op, ctx)
                    && let Some(&k) = direct_callee(ctx, st, op).and_then(|c| ra.get(c))
                {
                    let call = Operation::get_op::<CallOp>(op, ctx).unwrap();
                    call.args(ctx)
                        .get(k as usize)
                        .and_then(|a| m.get(a))
                        .copied()
                } else if Operation::is_op::<SelectOp>(op, ctx) {
                    let d = op.deref(ctx);
                    match (m.get(&d.get_operand(1)), m.get(&d.get_operand(2))) {
                        (Some(&a), Some(&b)) if a == b => Some(a),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(i) = add
                    && let Some(r) = op.deref(ctx).results().next()
                    && m.get(&r) != Some(&i)
                {
                    m.insert(r, i);
                    changed = true;
                }
            }
        }
        if !changed {
            return m;
        }
    }
}

/// `Some(k)` when every `ret` in `f` returns the value mapped to param `k`
/// under table `ra`. `None` covers both "not provable yet" and a genuine
/// conflict — either way the function stays out of the table.
fn ret_idx(
    ctx: &Context,
    st: &State<'_>,
    f: Ptr<Operation>,
    ra: &FxHashMap<String, u32>,
) -> Option<u32> {
    let m = param_map(ctx, st, f, ra);
    let mut acc = None;
    for b in blocks(ctx, f) {
        for op in ops(ctx, b) {
            let Some(r) = Operation::get_op::<ReturnOp>(op, ctx) else {
                continue;
            };
            let k = *m.get(&r.retval(ctx)?)?;
            if acc.is_some_and(|a| a != k) {
                return None;
            }
            acc = Some(k);
        }
    }
    acc
}

pub fn run(ctx: &Context, st: &State<'_>) {
    let bodied: Vec<String> = st
        .funcs
        .iter()
        .filter(|(_, f)| f.linkage != Linkage::Preemptible && has_body(ctx, f.op))
        .map(|(s, _)| s.clone())
        .collect();
    let mut ra: FxHashMap<String, u32> = FxHashMap::default();
    loop {
        let mut changed = false;
        for s in &bodied {
            if ra.contains_key(s) {
                continue;
            }
            if let Some(k) = ret_idx(ctx, st, st.funcs[s].op, &ra) {
                ra.insert(s.clone(), k);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Consume: a `returned(k)` call's result is the k-th operand.
    let mut folded = 0usize;
    for f in st.funcs.values() {
        if !has_body(ctx, f.op) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
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
                let r = op.deref(ctx).get_result(0);
                // Guard: a type mismatch would mean the inference lied.
                if r.get_type(ctx) == a.get_type(ctx) {
                    r.replace_all_uses_with(ctx, &a);
                    folded += 1;
                }
            }
        }
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!(
            "retarg {}: {} returned-arg bodies, {folded} call results folded",
            st.cgu,
            ra.len()
        );
    }
}

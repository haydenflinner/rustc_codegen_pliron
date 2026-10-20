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
use rustc_hash::{FxHashMap, FxHashSet};

use crate::has_body;
use crate::inline::{blocks_iter, direct_callee};
use crate::state::State;

pub fn run(ctx: &mut Context, st: &mut State) {
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::spec", tracing::Level::DEBUG);
    // Direct callsites per callee. `dead_unopt` bodies can't have their
    // callsites rewritten (their calls are emitted as-is), so a callee they
    // reach is vetoed outright — specializing it on the surviving callsites
    // would change what the emitted call observes.
    let mut sites: FxHashMap<String, Vec<Ptr<Operation>>> = FxHashMap::default();
    let mut dead_called = FxHashSet::default();
    let mut scratch = Vec::new();
    for (n, f) in st.funcs.iter() {
        if !has_body(ctx, f.op) {
            continue;
        }
        let dead = st.dead_unopt.contains(n.as_str()) || st.dead_fns.contains(n.as_str());
        for b in blocks_iter(ctx, f.op) {
            crate::inline::ops_into(ctx, b, &mut scratch);
            for &op in &scratch {
                if let Some(sym) = direct_callee(ctx, st, op) {
                    if dead {
                        dead_called.insert(sym.clone());
                    } else {
                        sites.entry(sym.clone()).or_default().push(op);
                    }
                }
            }
        }
    }
    // Symbols whose address escapes: an unseen caller could pass different
    // arguments.
    let taken = crate::address_taken(ctx, st);
    let mut done = 0usize;
    for (sym, f) in &st.funcs {
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::spec",
                    "spec {sym}: linkage={:?} body={} taken={} sites={:?}",
                    f.linkage,
                    has_body(ctx, f.op),
                    taken.contains(sym.as_str()),
                    sites.get(sym).map(|v| v.len())
                )
            };
        }
        if f.linkage != Linkage::Local
            || !has_body(ctx, f.op)
            || taken.contains(sym.as_str())
            || st.dead_fns.contains(sym.as_str())
            || st.dead_unopt.contains(sym.as_str())
            || dead_called.contains(sym.as_str())
        {
            continue;
        }
        let Some(calls) = sites.get(sym) else {
            continue;
        };
        let params: Vec<Value> = blocks_iter(ctx, f.op)
            .next()
            .unwrap()
            .deref(ctx)
            .arguments(ctx)
            .collect();
        let args: Vec<Vec<Value>> = calls
            .iter()
            .map(|&c| Operation::get_op::<CallOp>(c, ctx).unwrap().args(ctx))
            .collect();
        for (i, p) in params.iter().enumerate() {
            let Some(a0) = args[0].get(i) else { break };
            let Some(c0) = st.consts.get(*a0).cloned() else {
                continue;
            };
            if a0.get_type(ctx) != p.get_type(ctx) {
                continue;
            }
            if !args
                .iter()
                .all(|a| a.get(i).is_some_and(|&v| st.consts.get(v) == Some(&c0)))
            {
                continue;
            }
            // Fresh orphan const: `st.consts` also maps some in-block
            // results (inline clones), whose defs can't migrate across
            // functions; an unattached `UndefOp` can be used anywhere.
            let nv = UndefOp::new(ctx, p.get_type(ctx))
                .get_operation()
                .deref(ctx)
                .get_result(ctx, 0);
            st.consts.insert(nv, c0);
            p.replace_all_uses_with(ctx, &nv);
            done += 1;
        }
    }
    if done > 0
        && pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "spec {}: {done} params", st.cgu)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::blocks;
    use crate::state::{ConstVal, Exts, FuncInfo};
    use cranelift_module::Linkage;
    use pliron::builtin::op_interfaces::SymbolOpInterface;
    use pliron::builtin::ops::ModuleOp;
    use pliron::irfmt::parsers::spaced;
    use pliron::parsable::parse_from_str;
    use pliron_llvm::ops::FuncOp;
    use rustc_hash::FxHashSet;

    /// A `dead_unopt` body still emits its calls verbatim, so its callsites
    /// must veto specialization: specializing the callee on the surviving
    /// (live) callsites alone would change what the emitted call observes.
    #[test]
    fn dead_body_callsite_vetoes_specialization() {
        let input = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @callee: llvm.func <builtin.integer i32(builtin.integer i32) variadic = false> [] {
          ^entry_block_1_0(x: builtin.integer i32):
            llvm.return x
          };
          llvm.func @live: llvm.func <llvm.void () variadic = false> [] {
          ^entry_block_2_0():
            five = llvm.constant <builtin.integer <5: i32>> : builtin.integer i32;
            r = llvm.call @callee (five) : llvm.func <builtin.integer i32(builtin.integer i32) variadic = false>;
            llvm.return
          };
          llvm.func @dead: llvm.func <llvm.void () variadic = false> [] {
          ^entry_block_3_0():
            seven = llvm.constant <builtin.integer <7: i32>> : builtin.integer i32;
            r = llvm.call @callee (seven) : llvm.func <builtin.integer i32(builtin.integer i32) variadic = false>;
            llvm.return
          }
        }
        "#;
        let ctx = &mut Context::new();
        let op = parse_from_str(spaced(Operation::top_level_parser()), ctx, input).expect("parse");
        let mop = Operation::get_op::<ModuleOp>(op, ctx).unwrap();
        let st = &mut State::default();
        for fop in crate::inline::blocks(ctx, mop.get_operation())
            .into_iter()
            .flat_map(|b| crate::inline::ops(ctx, b))
        {
            let Some(f) = Operation::get_op::<FuncOp>(fop, ctx) else {
                continue;
            };
            let sym = f.get_symbol_name(ctx).to_string();
            st.ident_to_sym.insert(sym.clone(), sym.clone());
            st.funcs.insert(
                sym.clone(),
                FuncInfo {
                    op: fop,
                    ty: f.get_type(ctx).into(),
                    linkage: Linkage::Local,
                    exts: Exts::default(),
                    no_inline: false,
                    always_inline: false,
                    inline_hint: false,
                    cold: false,
                    nounwind: false,
                    odr: false,
                    sret_ty: None,
                    frozen: Vec::new(),
                    noalias: Vec::new(),
                    nonnull: Vec::new(),
                    nparams: 1,
                },
            );
            // `llvm.constant` args become `st.consts` entries; `live` and
            // `dead` pass the callee different values.
            let bits = if sym == "live" { 5u128 } else { 7u128 };
            for b in blocks(ctx, fop) {
                for op in crate::inline::ops(ctx, b) {
                    if let Some(c) = Operation::get_op::<CallOp>(op, ctx)
                        && let Some(&a) = c.args(ctx).first()
                    {
                        st.consts.insert(a, ConstVal::Bits(bits));
                    }
                }
            }
        }
        st.dead_unopt.insert("dead".to_string());
        st.xcgu_copies.insert("dead".to_string());
        run(ctx, st);
        let callee = st.funcs["callee"].op;
        let p = blocks(ctx, callee)[0]
            .deref(ctx)
            .arguments(ctx)
            .next()
            .unwrap();
        assert!(
            p.is_used(ctx),
            "spec specialized a callee a dead copy calls differently"
        );
    }
}

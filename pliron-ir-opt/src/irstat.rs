//! `PLIRON_IR_HIST=<file>`: an op histogram of the pliron IR at a pipeline
//! point, appended to `<file>` (one record per CGU and phase). Diagnostic
//! only; nothing here mutates the IR.
//!
//! Each record lists ops by kind, the biggest functions, and two kinds of
//! redundancy a cleanup pass could remove: pure ops with no uses, and pure
//! ops that repeat an earlier op of the same block with the same operands
//! and attributes.

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::operation::Operation;
use pliron::printable::Printable;
use pliron::r#type::Typed;
use pliron::value::Value;
use rustc_hash::FxHashMap;
use std::io::Write;

use crate::has_body;
use crate::state::State;

/// Ops with no side effects: erasable when unused, mergeable when repeated.
pub fn is_pure(name: &str) -> bool {
    matches!(
        name,
        "llvm.add"
            | "llvm.sub"
            | "llvm.mul"
            | "llvm.shl"
            | "llvm.lshr"
            | "llvm.ashr"
            | "llvm.and"
            | "llvm.or"
            | "llvm.xor"
            | "llvm.icmp"
            | "llvm.fcmp"
            | "llvm.select"
            | "llvm.bitcast"
            | "llvm.zext"
            | "llvm.sext"
            | "llvm.trunc"
            | "llvm.ptrtoint"
            | "llvm.inttoptr"
            | "llvm.addrspacecast"
            | "llvm.getelementptr"
            | "llvm.extractvalue"
            | "llvm.insertvalue"
            | "llvm.freeze"
            | "llvm.fadd"
            | "llvm.fsub"
            | "llvm.fmul"
            | "llvm.fdiv"
            | "llvm.fneg"
            | "llvm.fpext"
            | "llvm.fptrunc"
            | "llvm.sitofp"
            | "llvm.uitofp"
            | "llvm.fptosi"
            | "llvm.fptoui"
            | "llvm.undef"
            | "llvm.poison"
            | "llvm.mlir.constant"
            | "llvm.constant"
            | "llvm.zero"
            | "llvm.insertelement"
            | "llvm.extractelement"
            | "llvm.shufflevector"
    )
}

fn category(name: &str) -> &'static str {
    match name {
        "llvm.undef" | "llvm.poison" | "llvm.mlir.constant" | "llvm.constant" | "llvm.zero" => "const",
        "llvm.bitcast" | "llvm.zext" | "llvm.sext" | "llvm.trunc" | "llvm.ptrtoint"
        | "llvm.inttoptr" | "llvm.addrspacecast" | "llvm.fpext" | "llvm.fptrunc"
        | "llvm.sitofp" | "llvm.uitofp" | "llvm.fptosi" | "llvm.fptoui" => "cast",
        "llvm.getelementptr" => "gep",
        "llvm.load" => "load",
        "llvm.store" => "store",
        "llvm.call" => "call",
        "llvm.call_intrinsic" => "intrinsic",
        "llvm.br" | "llvm.cond_br" | "llvm.switch" | "llvm.return" | "llvm.unreachable"
        | "llvm.indirectbr" => "control",
        "llvm.alloca" => "alloca",
        "llvm.extractvalue" | "llvm.insertvalue" => "aggregate",
        "llvm.icmp" | "llvm.fcmp" | "llvm.select" => "cmp/select",
        _ if is_pure(name) => "arith",
        _ => "other",
    }
}

#[derive(Default)]
struct Hist {
    kinds: FxHashMap<String, u64>,
    cats: FxHashMap<&'static str, u64>,
    fns: Vec<(usize, String)>,
    ops: u64,
    blocks: u64,
    dead: u64,
    dup: u64,
    dead_kinds: FxHashMap<String, u64>,
    dup_kinds: FxHashMap<String, u64>,
}

/// Append one record for `phase` to `PLIRON_IR_HIST`'s file.
pub fn report(ctx: &Context, st: &State, phase: &str) {
    let Some(path) = crate::env_get("PLIRON_IR_HIST") else {
        return;
    };
    let mut h = Hist::default();
    let mut names: FxHashMap<pliron::op::OpId, String> = FxHashMap::default();
    let mut seen: FxHashMap<(String, Vec<String>, String), u32> = FxHashMap::default();
    // Landing pads: entered by unwinding, not by a visible edge.
    let pads: rustc_hash::FxHashSet<Ptr<BasicBlock>> = st.invokes.iter().map(|(_, &(lp, _))| lp).collect();
    let (mut merge1, mut bronly, mut intr) = (0u64, 0u64, FxHashMap::<String, u64>::default());
    for (sym, f) in st.funcs.iter() {
        if !has_body(ctx, f.op) || st.dead_fns.contains(sym.as_str()) {
            continue;
        }
        let mut n = 0usize;
        // Predecessor counts and the unconditional-br preds per block.
        let mut npred: FxHashMap<Ptr<BasicBlock>, (u32, bool)> = FxHashMap::default();
        for b in crate::inline::blocks_iter(ctx, f.op) {
            if let Some(t) = b.deref(ctx).get_tail() {
                let to = t.deref(ctx);
                let is_br = Operation::get_opid(t, ctx).to_string() == "llvm.br";
                for s in to.successors(ctx) {
                    let e = npred.entry(s).or_insert((0, true));
                    e.0 += 1;
                    e.1 &= is_br;
                }
            }
        }
        for b in crate::inline::blocks_iter(ctx, f.op) {
            h.blocks += 1;
            seen.clear();
            let bops = block_ops(ctx, b);
            if let Some(&(1, true)) = npred.get(&b)
                && !pads.contains(&b)
            {
                merge1 += 1;
            }
            if bops.len() == 1 && Operation::get_opid(bops[0], ctx).to_string() == "llvm.br" {
                bronly += 1;
            }
            for op in bops {
                n += 1;
                let opid = Operation::get_opid(op, ctx);
                let name = names.entry(opid).or_insert_with_key(|k| k.to_string()).clone();
                *h.kinds.entry(name.clone()).or_default() += 1;
                *h.cats.entry(category(&name)).or_default() += 1;
                if let Some(i) = st.intrinsics.get(op) {
                    *intr.entry(i.clone()).or_default() += 1;
                }
                if !is_pure(&name) {
                    continue;
                }
                let o = op.deref(ctx);
                if o.get_num_results() > 0 && !o.results(ctx).any(|r| r.is_used(ctx)) {
                    h.dead += 1;
                    *h.dead_kinds.entry(name.clone()).or_default() += 1;
                    continue;
                }
                // Constant placeholders are one op per use: key them by value.
                let opk = |v: Value| match st.consts.get(v) {
                    Some(c) => format!("c{:?}", c),
                    None => format!("{:?}", v),
                };
                let key = (
                    name.clone(),
                    o.operands(ctx).map(opk).collect::<Vec<_>>(),
                    format!(
                        "{}:{}",
                        o.attributes().disp(ctx),
                        o.results(ctx).map(|r| r.get_type(ctx).disp(ctx).to_string()).collect::<Vec<_>>().join(",")
                    ),
                );
                let c = seen.entry(key).or_default();
                if *c > 0 {
                    h.dup += 1;
                    *h.dup_kinds.entry(name.clone()).or_default() += 1;
                }
                *c += 1;
            }
        }
        h.ops += n as u64;
        h.fns.push((n, sym.clone()));
    }
    h.fns.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let (arena_ops, arena_blocks, _) = ctx.ir_stats();
    let mut out = String::new();
    out.push_str(&format!(
        "== {} {phase}: fns={} ops={} blocks={} arena_ops={} arena_blocks={} floating={} dead_pure={} dup_pure={} merge1={merge1} bronly={bronly}\n",
        st.cgu,
        h.fns.len(),
        h.ops,
        h.blocks,
        arena_ops,
        arena_blocks,
        (arena_ops as u64).saturating_sub(h.ops),
        h.dead,
        h.dup
    ));
    let mut intr: Vec<_> = intr.into_iter().collect();
    intr.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (k, n) in intr.iter().take(12) {
        out.push_str(&format!("intr {k} {n}\n"));
    }
    let mut cats: Vec<_> = h.cats.into_iter().collect();
    cats.sort_by(|a, b| b.1.cmp(&a.1));
    out.push_str("cats:");
    for (c, n) in cats {
        out.push_str(&format!(" {c}={n}"));
    }
    out.push('\n');
    let mut kinds: Vec<_> = h.kinds.into_iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (k, n) in kinds {
        let d = h.dead_kinds.get(&k).copied().unwrap_or(0);
        let p = h.dup_kinds.get(&k).copied().unwrap_or(0);
        out.push_str(&format!("kind {k} {n} dead={d} dup={p}\n"));
    }
    for (n, sym) in h.fns.iter().take(40) {
        out.push_str(&format!("fn {n} {sym}\n"));
    }
    if crate::env_is("PLIRON_IR_HIST_ALLFNS", "1") {
        for (n, sym) in h.fns.iter() {
            out.push_str(&format!("allfn {n} {sym}\n"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(out.as_bytes());
    }
}

fn block_ops(ctx: &Context, b: Ptr<BasicBlock>) -> Vec<Ptr<Operation>> {
    b.deref(ctx).iter(ctx).collect()
}

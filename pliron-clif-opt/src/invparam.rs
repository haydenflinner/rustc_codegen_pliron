//! Loop-invariant header params move to a preheader (`PLIRON_INVPARAM=1`).
//!
//! A loop header param that every back edge passes back unchanged is
//! invariant in the loop: it only differs between entries. Left on the
//! header, it is a block-param web around the back edge, and regalloc2
//! treats the back edge as a move point: serde_json's escape scan spilled
//! the string base at the loop head and reloaded it on the back edge every
//! byte, though no instruction in the loop touched its register. LLVM's
//! LoopSimplify form never has this shape: loops get a dedicated
//! preheader, and an inner-loop invariant is a plain value defined
//! outside the loop.
//!
//! A header whose back edges disagree (some pass a param back, others a
//! new value: an outer re-entry folded onto the same header) is split
//! into an outer and an inner loop first, as LLVM LoopSimplify does.
//!
//! Per loop whose header has such params: a fresh preheader takes over
//! every entry edge (with all of the header's params, so entry branches
//! keep their args), passes the varying ones on, and its own params stand
//! in for the invariant ones; back edges stop passing them. Loops entered
//! or re-entered through `try_call` edges (non-value args) are skipped.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{BlockArg, BlockCall, Function, Inst, InstBuilder, Value};
use cranelift_codegen::loop_analysis::Loop;

use crate::pipeline::{Analysis, CfgDtLa};

/// Loops rewritten per call; each rewrite recomputes the CFG.
const MAX_LOOPS: usize = 256;

/// (branch inst, destination slot) of every edge into `h`.
fn edges_into(func: &Function, preds: &[Inst], h: cranelift_codegen::ir::Block) -> Vec<(Inst, usize)> {
    let mut out = Vec::new();
    for &i in preds {
        let dests = func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        for (s, bc) in dests.iter().enumerate() {
            if bc.block(&func.dfg.value_lists) == h && !out.contains(&(i, s)) {
                out.push((i, s));
            }
        }
    }
    out
}

fn args_of(func: &Function, i: Inst, s: usize) -> Option<Vec<Value>> {
    let bc = func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[s];
    bc.args(&func.dfg.value_lists)
        .map(|a| match a {
            BlockArg::Value(v) => Some(v),
            _ => None,
        })
        .collect()
}

fn set_dest(func: &mut Function, i: Inst, s: usize, b: cranelift_codegen::ir::Block, args: &[Value]) {
    let dfg = &mut func.dfg;
    let bc = &mut dfg.insts[i].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[s];
    *bc = BlockCall::new(b, args.iter().map(|&v| BlockArg::Value(v)), &mut dfg.value_lists);
}

/// Rewrite one loop; returns whether anything changed.
fn one(func: &mut Function, an: &mut Analysis, done: &mut Vec<cranelift_codegen::ir::Block>) -> bool {
    let CfgDtLa { cfg, la, .. } = an.cfg_dt_la(func);
    let loops: Vec<Loop> = la.loops().collect();
    for lp in loops {
        let h = la.loop_header(lp);
        if done.contains(&h) {
            continue;
        }
        let params: Vec<Value> = func.dfg.block_params(h).to_vec();
        if params.is_empty() {
            continue;
        }
        let preds: Vec<Inst> = cfg.pred_iter(h).map(|p| p.inst).collect();
        let edges = edges_into(func, &preds, h);
        let mut back = Vec::new();
        let mut entry = Vec::new();
        let mut ok = true;
        for &(i, s) in &edges {
            let Some(args) = args_of(func, i, s) else {
                ok = false;
                break;
            };
            let pb = func.layout.inst_block(i).unwrap();
            if la.is_in_loop(pb, lp) {
                back.push((i, s, args));
            } else {
                entry.push((i, s));
            }
        }
        if !ok || back.is_empty() || entry.is_empty() {
            continue;
        }
        // Params each back edge passes back unchanged. The edges passing
        // the largest such set form the inner loop; the other back edges
        // become outer-loop edges into the new header (LLVM LoopSimplify's
        // `separateNestedLoop`), and with no other back edges the new
        // header is a plain preheader.
        let ident = |a: &[Value]| -> Vec<bool> {
            (0..params.len()).map(|k| func.dfg.resolve_aliases(a[k]) == params[k]).collect()
        };
        let best = back
            .iter()
            .map(|(_, _, a)| ident(a))
            .max_by_key(|m| m.iter().filter(|&&x| x).count())
            .unwrap();
        let inv = best;
        if !inv.iter().any(|&x| x) || !pliron_cfg::bisect("invparam") {
            done.push(h);
            continue;
        }
        let covers = |m: &[bool]| m.iter().zip(&inv).all(|(&e, &i)| e || !i);
        let (back, outer): (Vec<_>, Vec<_>) = back.into_iter().partition(|(_, _, a)| covers(&ident(a)));
        let entry: Vec<(Inst, usize)> = entry.into_iter().chain(outer.into_iter().map(|(i, s, _)| (i, s))).collect();
        let tys: Vec<_> = params.iter().map(|&v| func.dfg.value_type(v)).collect();
        // Preheader with all of the header's params.
        let ph = func.dfg.make_block();
        func.layout.insert_block(ph, h);
        let php: Vec<Value> = tys.iter().map(|&t| func.dfg.append_block_param(ph, t)).collect();
        // Header keeps only the varying params.
        let old: Vec<Value> = func.dfg.detach_block_params(h).as_slice(&func.dfg.value_lists).to_vec();
        let mut pass = Vec::new();
        for k in 0..old.len() {
            if inv[k] {
                func.dfg.change_to_alias(old[k], php[k]);
            } else {
                let nv = func.dfg.append_block_param(h, tys[k]);
                func.dfg.change_to_alias(old[k], nv);
                pass.push(php[k]);
            }
        }
        FuncCursor::new(func).at_bottom(ph).ins().jump(h, &pass.iter().map(|&v| BlockArg::Value(v)).collect::<Vec<_>>());
        for (i, s) in entry {
            let a = args_of(func, i, s).unwrap();
            set_dest(func, i, s, ph, &a);
        }
        for (i, s, a) in back {
            let keep: Vec<Value> = a.iter().zip(&inv).filter(|(_, iv)| !**iv).map(|(v, _)| *v).collect();
            set_dest(func, i, s, h, &keep);
        }
        done.push(h);
        an.invalidate(func);
        return true;
    }
    false
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let mut n = 0;
    let mut done = Vec::new();
    while n < MAX_LOOPS && one(func, an, &mut done) {
        n += 1;
    }
    n
}

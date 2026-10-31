//! Index-derived pointers back to pointer increments (`PLIRON_IVPTR=1`).
//!
//! The `TrustedRandomAccess` zip addresses elements as `base + i * s`:
//! once the per-element `Option` test folds (`PLIRON_INLINENN`) and its
//! index update is affine (`PLIRON_BRIFSEL`), the loop carries pointer
//! params rebuilt from the index on every back edge:
//!
//! ```text
//! H(p, i, ..):  ...  brif c, H(B + (i << k), i + 1, ..), EXIT
//! ```
//!
//! With `i' = i + 1` and `p0 = B + s * (i0 - 1)` on every entry edge
//! (compared as linear forms over `iadd`/`isub`/`imul`/`ishl` by
//! constants), `p = B + s * (i - 1)` holds on every trip by induction, so
//! the back-edge value is `p + s`. Rewriting it so takes the index (and a
//! shifted add) off each load's address: the shape LLVM's LSR picks.
//! num-bigint-dig's `add_mul_vvw` zip in blind-sig's montgomery.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Block, BlockArg, Function, InstBuilder, InstructionData, Opcode, Value};
use rustc_hash::FxHashMap;

use crate::pipeline::{Analysis, CfgDtLa};

/// `k + sum(coef * leaf)`, leaves sorted.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Lin {
    k: i64,
    t: Vec<(Value, i64)>,
}

impl Lin {
    fn add(&self, o: &Lin, sign: i64) -> Lin {
        let mut m: FxHashMap<Value, i64> = self.t.iter().copied().collect();
        for &(v, c) in &o.t {
            *m.entry(v).or_insert(0) = m.get(&v).copied().unwrap_or(0).wrapping_add(c.wrapping_mul(sign));
        }
        let mut t: Vec<(Value, i64)> = m.into_iter().filter(|&(_, c)| c != 0).collect();
        t.sort_unstable_by_key(|&(v, _)| v.as_u32());
        Lin { k: self.k.wrapping_add(o.k.wrapping_mul(sign)), t }
    }
    fn scale(&self, s: i64) -> Lin {
        Lin { k: self.k.wrapping_mul(s), t: self.t.iter().map(|&(v, c)| (v, c.wrapping_mul(s))).collect() }
    }
}

fn iconst_of(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

fn lin(func: &Function, v: Value, depth: u32) -> Lin {
    let v = func.dfg.resolve_aliases(v);
    let leaf = Lin { k: 0, t: vec![(v, 1)] };
    if let Some(k) = iconst_of(func, v) {
        return Lin { k, t: vec![] };
    }
    if depth > 8 {
        return leaf;
    }
    let Some(i) = func.dfg.value_def(v).inst() else { return leaf };
    match func.dfg.insts[i] {
        InstructionData::Binary { opcode: Opcode::Iadd, args } => lin(func, args[0], depth + 1).add(&lin(func, args[1], depth + 1), 1),
        InstructionData::Binary { opcode: Opcode::Isub, args } => lin(func, args[0], depth + 1).add(&lin(func, args[1], depth + 1), -1),
        InstructionData::Binary { opcode: Opcode::Imul, args } => match (iconst_of(func, args[0]), iconst_of(func, args[1])) {
            (_, Some(c)) => lin(func, args[0], depth + 1).scale(c),
            (Some(c), _) => lin(func, args[1], depth + 1).scale(c),
            _ => leaf,
        },
        InstructionData::Binary { opcode: Opcode::Ishl, args } => match iconst_of(func, args[1]) {
            Some(c) if (0..63).contains(&c) => lin(func, args[0], depth + 1).scale(1 << c),
            _ => leaf,
        },
        _ => leaf,
    }
}

/// The edge args of every edge into `h` from outside the loop, or `None`
/// when one isn't a plain value list.
fn entry_args(func: &Function, cfg: &cranelift_codegen::flowgraph::ControlFlowGraph, la: &cranelift_codegen::loop_analysis::LoopAnalysis, lp: cranelift_codegen::loop_analysis::Loop, h: Block) -> Option<Vec<Vec<Value>>> {
    let mut out = Vec::new();
    for pred in cfg.pred_iter(h) {
        if la.is_in_loop(pred.block, lp) {
            continue;
        }
        let dests = func.dfg.insts[pred.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        for bc in dests.iter().filter(|bc| bc.block(&func.dfg.value_lists) == h) {
            out.push(bc.args(&func.dfg.value_lists).map(|a| match a {
                BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                _ => None,
            }).collect::<Option<Vec<_>>>()?);
        }
    }
    (!out.is_empty()).then_some(out)
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    // (latch inst, dest index, param slot, pointer param, step)
    let mut edits: Vec<(cranelift_codegen::ir::Inst, usize, usize, Value, i64)> = Vec::new();
    {
        let CfgDtLa { cfg, la, .. } = an.cfg_dt_la(func);
        for lp in la.loops() {
            let h = la.loop_header(lp);
            let params: Vec<Value> = func.dfg.block_params(h).to_vec();
            let Some(entries) = entry_args(func, cfg, la, lp, h) else { continue };
            let latches: Vec<_> = cfg.pred_iter(h).filter(|p| la.is_in_loop(p.block, lp)).collect();
            let [latch] = latches[..] else { continue };
            let dests = func.dfg.insts[latch.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            let Some(di) = dests.iter().position(|bc| bc.block(&func.dfg.value_lists) == h) else { continue };
            if dests.iter().filter(|bc| bc.block(&func.dfg.value_lists) == h).count() != 1 {
                continue;
            }
            let Some(back) = dests[di].args(&func.dfg.value_lists).map(|a| match a {
                BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                _ => None,
            }).collect::<Option<Vec<_>>>() else { continue };
            if back.len() != params.len() {
                continue;
            }
            // Index params: `i' = i + 1`, a constant entry value on every edge.
            let idx: Vec<(usize, i64)> = (0..params.len())
                .filter(|&k| lin(func, back[k], 0) == Lin { k: 1, t: vec![(params[k], 1)] })
                .filter_map(|k| {
                    let e0 = iconst_of(func, entries[0][k])?;
                    entries.iter().all(|e| iconst_of(func, e[k]) == Some(e0)).then_some((k, e0))
                })
                .collect();
            for &(ki, i0) in &idx {
                let i = params[ki];
                for (kp, &p) in params.iter().enumerate() {
                    if kp == ki {
                        continue;
                    }
                    // `p' = B + s * i`: split the back-edge form's `i` term off.
                    let f = lin(func, back[kp], 0);
                    let Some(&(_, s)) = f.t.iter().find(|&&(v, _)| v == i) else { continue };
                    if s == 0 || f.t.iter().any(|&(v, _)| v == p || params.contains(&v) && v != i) {
                        continue;
                    }
                    let base = f.add(&Lin { k: 0, t: vec![(i, s)] }, -1);
                    // Every entry: p0 == B + s * (i0 - 1).
                    let want = base.add(&Lin { k: s.wrapping_mul(i0.wrapping_sub(1)), t: vec![] }, 1);
                    if entries.iter().all(|e| lin(func, e[kp], 0) == want) && pliron_cfg::bisect("ivptr") {
                        edits.push((latch.inst, di, kp, p, s));
                    }
                }
            }
        }
    }
    let n = edits.len();
    for (inst, di, kp, p, s) in edits {
        let mut cur = FuncCursor::new(func).at_inst(inst);
        let np = cur.ins().iadd_imm_s(p, s);
        let func = cur.func;
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di];
        let blk = bc.block(&dfg.value_lists);
        let mut args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
        args[kp] = BlockArg::Value(np);
        *bc = cranelift_codegen::ir::BlockCall::new(blk, args, &mut dfg.value_lists);
    }
    if n > 0 {
        an.invalidate(func);
    }
    n
}

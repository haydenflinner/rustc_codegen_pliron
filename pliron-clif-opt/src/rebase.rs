//! Constant-offset rebase through a dominating increment
//! (`PLIRON_REBASE`, opt-in).
//!
//! ```text
//! w = iadd p, J      x = iadd p, K        →      w = iadd p, J
//! (dominates x)                                 x = iadd w, K - J
//! ```
//!
//! `x` still computes `p + K` — ring arithmetic needs no nowrap — but
//! `p`'s post-increment uses all funnel through `w`. That is the shape
//! aarch64 post-index addressing (`PLIRON_POSTIDX`) needs: the UTF-8
//! scan's `uload8 p; p' = p + 1` fuses only when `p` is dead after the
//! load, and the decode arm's `p + 2` kept `p` live until it was
//! rewritten as `p' + 1`.
//!
//! A rewrite never invalidates another candidate: `x` still means
//! `p + K` after its args change, so the def table built from the
//! original function stays semantically exact. Scoped to `i64` values
//! that feed a scalar load/store base — the only consumers the fusion
//! cares about.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    types, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::pipeline::{Analysis, CfgDtLa};

/// Cap on the `iadd`/`isub` defs of one pointer that a use walks;
/// beyond it the base is left alone (a linear scan stays linear).
const MAX_DEFS: usize = 64;

/// `(base, signed K)` for `iadd base, K` / `isub base, K` (signed so
/// `isub` records `-K`); `isub K, base` and all-const forms do not
/// match. The const operand must materialize as `iconst`.
fn const_add(func: &Function, inst: Inst) -> Option<(Value, i64)> {
    let (args, is_sub) = match func.dfg.insts[inst] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } => (args, false),
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args,
        } => (args, true),
        _ => return None,
    };
    let konst = |a: Value| -> Option<i64> {
        match func.dfg.insts[func.dfg.value_def(func.dfg.resolve_aliases(a)).inst()?] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits()),
            _ => None,
        }
    };
    let (base, k) = match (konst(args[0]), konst(args[1])) {
        (None, Some(k)) => (args[0], k),
        (Some(k), None) if !is_sub => (args[1], k),
        _ => return None,
    };
    Some((base, if is_sub { -k } else { k }))
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let CfgDtLa { dt, .. } = an.cfg_dt_la(func);
    // `iadd p, J`/`isub p, J` defs per base `p` (as `p + Js`), and the
    // values that feed a scalar memory base — the only ones worth
    // rebasing.
    let mut defs: FxHashMap<Value, Vec<(Inst, Value, i64)>> = FxHashMap::default();
    let mut mem_bases: FxHashSet<Value> = FxHashSet::default();
    let i64_out = |func: &Function, i: Inst| {
        func.dfg
            .inst_results(i)
            .first()
            .is_some_and(|&r| func.dfg.value_type(r) == types::I64)
    };
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            match func.dfg.insts[i] {
                InstructionData::LoadNoOffset { arg, .. }
                | InstructionData::Load { arg, .. } => {
                    mem_bases.insert(func.dfg.resolve_aliases(arg));
                }
                InstructionData::StoreNoOffset { args, .. }
                | InstructionData::Store { args, .. } => {
                    mem_bases.insert(func.dfg.resolve_aliases(args[1]));
                }
                _ => {}
            }
            if i64_out(func, i)
                && let Some((base, js)) = const_add(func, i)
            {
                defs.entry(func.dfg.resolve_aliases(base))
                    .or_default()
                    .push((i, func.dfg.inst_results(i)[0], js));
            }
        }
    }
    // For every `x = iadd/isub p, K` on a memory base, the nearest
    // dominating `w = p + Js` (defs of `p` dominating `x` are totally
    // ordered — the deepest one wins).
    let mut edits: Vec<(Inst, Value, i64)> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if !i64_out(func, i) {
                continue;
            }
            let Some((base, ku)) = const_add(func, i) else {
                continue;
            };
            let base = func.dfg.resolve_aliases(base);
            if !mem_bases.contains(&base) {
                continue;
            }
            let Some(cands) = defs.get(&base) else { continue };
            let mut best: Option<(Inst, Value, i64)> = None;
            for &(w, wv, js) in cands.iter().take(MAX_DEFS) {
                if w == i || !dt.dominates(w, i, &func.layout) {
                    continue;
                }
                if best.is_none_or(|(b, _, _)| dt.dominates(b, w, &func.layout)) {
                    best = Some((w, wv, js));
                }
            }
            let Some((_, w, js)) = best else { continue };
            // `x = p + Ku`; `p = w - Js` → `x = w + (Ku - Js)`.
            edits.push((i, w, ku.wrapping_sub(js)));
        }
    }
    let n = edits.len();
    for (i, w, c) in edits {
        if c == 0 {
            let x = func.dfg.inst_results(i)[0];
            func.layout.remove_inst(i);
            func.dfg.detach_inst_results(i);
            func.dfg.change_to_alias(x, w);
            continue;
        }
        let mut pos = FuncCursor::new(func).at_inst(i);
        let k = pos.ins().iconst(types::I64, c);
        pos.func.dfg.insts[i] = InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [w, k],
        };
    }
    if n > 0 {
        an.invalidate(func);
    }
    n
}

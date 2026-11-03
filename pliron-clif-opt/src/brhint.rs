//! Static branch hints for block placement (`PLIRON_BRHINT=1`, read by the
//! vendored `machinst/placement.rs` under `PLIRON_PLACE=1`).
//!
//! Marks the unlikely arm of a `brif` in `Layout` (`set_unlikely_arm`),
//! without profiles, in the spirit of Ball-Larus and LLVM's
//! BranchProbabilityInfo:
//!
//! - **compare cascade**: `brif (icmp eq x, K)` whose other arm leads to
//!   another `icmp eq x, K'` (a `br_table` turned into a chain by
//!   `brchain`, or a `match` on an enum): each arm matches 1/N of the
//!   time, so the match arm is unlikely and the chain stays contiguous;
//! - **zero test**: `icmp eq x, 0` is unlikely and `icmp ne x, 0` likely
//!   (null pointers, empty lengths, error codes).
//!
//! Cold blocks and loop structure are stronger signals and placement
//! applies them first. Runs last in the pipeline so no CFG rewrite has to
//! carry the hints; the hint lives on the block, so a pass that replaces a
//! terminator after this step would have to clear it.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, Function, InstructionData, Opcode, Value};

use crate::pipeline::Analysis;

/// The `brif`'s `icmp` as (cc, x, y) with aliases resolved.
fn test(func: &Function, b: Block) -> Option<(IntCC, Value, Value, [Block; 2])> {
    let t = func.layout.last_inst(b)?;
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[t] else { return None };
    let ci = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst()?;
    let InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args } = func.dfg.insts[ci] else { return None };
    let dests = [blocks[0].block(&func.dfg.value_lists), blocks[1].block(&func.dfg.value_lists)];
    Some((cond, func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]), dests))
}

fn is_const(func: &Function, v: Value) -> Option<i64> {
    crate::loopidiom::iconst(func, v)
}

/// `eq`/`ne` test against a constant: (x, K, arm that matches).
fn eq_test(func: &Function, cc: IntCC, x: Value, y: Value) -> Option<(Value, i64, u8)> {
    let (v, k) = match (is_const(func, x), is_const(func, y)) {
        (None, Some(k)) => (x, k),
        (Some(k), None) => (y, k),
        _ => return None,
    };
    match cc {
        IntCC::Equal => Some((v, k, 0)),
        IntCC::NotEqual => Some((v, k, 1)),
        _ => None,
    }
}

pub fn run(func: &mut Function, _an: &mut Analysis) -> usize {
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut hints: Vec<(Block, u8)> = Vec::new();
    for &b in &blocks {
        let Some((cc, x, y, dests)) = test(func, b) else { continue };
        if dests[0] == dests[1] {
            continue;
        }
        let mut hint: Option<u8> = None;
        if let Some((v, k, arm)) = eq_test(func, cc, x, y) {
            // Cascade: the non-matching arm tests the same value again.
            let next = dests[1 - arm as usize];
            if next != b
                && let Some((cc2, x2, y2, _)) = test(func, next)
                && let Some((v2, _, _)) = eq_test(func, cc2, x2, y2)
                && v2 == v
            {
                hint = Some(arm);
            } else if k == 0 {
                hint = Some(arm);
            }
        }
        if let Some(arm) = hint {
            hints.push((b, arm));
        }
    }
    let n = hints.len();
    for (b, arm) in hints {
        func.layout.set_unlikely_arm(b, arm);
    }
    if n > 0 && pliron_cfg::tracing_on() {
        tracing::debug!(target: "pliron::brhint", "{}: {} hints", func.name, n);
    }
    0
}

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
//!   (null pointers, empty lengths, error codes) — except for masked
//!   values (`band`/`bor`/`bxor`), where the zero arm is as often the
//!   common path. A single-bit `band x, 1<<k` flips it: the bit-set arm
//!   is the "flag present" special case and goes unlikely instead.
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

/// `v` is a `band`/`bor`/`bxor` result — a bit mask, not a scalar where
/// "== 0" implies cold.
fn masked(func: &Function, v: Value) -> bool {
    matches!(
        func.dfg.value_def(v).inst().map(|i| func.dfg.insts[i].opcode()),
        Some(Opcode::Band | Opcode::Bor | Opcode::Bxor)
    )
}

/// `v` is a single-bit `band x, 1<<k` test: the nonzero arm is the
/// "flag set" path — the special case (UTF-8's `c & 0x80` sets the bit
/// only for non-ASCII). Returns true only for `band`: `bor x, K != 0`
/// is near-always true and `bxor` has no flag reading.
fn maskbit(func: &Function, v: Value) -> bool {
    let Some(i) = func.dfg.value_def(v).inst() else {
        return false;
    };
    let InstructionData::Binary {
        opcode: Opcode::Band,
        args,
    } = func.dfg.insts[i]
    else {
        return false;
    };
    args.iter().any(|&a| {
        is_const(func, a).is_some_and(|k| k != 0 && (k as u64).is_power_of_two())
    })
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
            } else if k == 0 && !masked(func, v) {
                // `x == 0` rare for pointers/lengths/counters — but not
                // for a `band`/`bor` mask, where the zero arm is just as
                // often the common one (UTF-8's `c & 0x80 == 0` is ASCII).
                hint = Some(arm);
            } else if k == 0 && maskbit(func, v) {
                // A single-bit mask flips the direction: `flags & BIT`
                // nonzero is the "flag set" arm — the special case —
                // so the bit-clear arm keeps the fallthrough instead.
                hint = Some(1 - arm);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hinted(src: &str) -> Vec<Option<u8>> {
        let mut funcs = cranelift_reader::parse_functions(src).unwrap();
        let mut an = Analysis::default();
        run(&mut funcs[0], &mut an);
        funcs[0]
            .layout
            .blocks()
            .map(|b| funcs[0].layout.unlikely_arm(b))
            .collect()
    }

    /// `band x, 1<<k != 0` is a flag test — the bit-set arm is the
    /// special case (UTF-8's `c & 0x80 != 0` is non-ASCII), so it is
    /// the arm marked unlikely.
    #[test]
    fn band_single_bit_marks_nonzero() {
        let h = hinted(
            "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 128
    v2 = band v0, v1
    v3 = iconst.i32 0
    v4 = icmp ne v2, v3
    brif v4, block1, block2
block1:
    return v0
block2:
    return v1
}",
        );
        assert_eq!(h, vec![Some(0), None, None]);
    }

    /// A multi-bit `band` is a field test, not a flag test — no hint.
    #[test]
    fn band_multibit_not_hinted() {
        let h = hinted(
            "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 240
    v2 = band v0, v1
    v3 = iconst.i32 0
    v4 = icmp ne v2, v3
    brif v4, block1, block2
block1:
    return v0
block2:
    return v1
}",
        );
        assert_eq!(h, vec![None, None, None]);
    }

    /// `bor x, K != 0` is near-always true — the nonzero arm is hot, so
    /// no hint (and the zero arm is not common either way).
    #[test]
    fn bor_not_hinted() {
        let h = hinted(
            "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 128
    v2 = bor v0, v1
    v3 = iconst.i32 0
    v4 = icmp ne v2, v3
    brif v4, block1, block2
block1:
    return v0
block2:
    return v1
}",
        );
        assert_eq!(h, vec![None, None, None]);
    }

    /// A plain `x == 0`/`x != 0` scalar test still gets the null hint.
    #[test]
    fn scalar_zero_hinted() {
        let h = hinted(
            "function %f(i64) -> i64 {
block0(v0: i64):
    v3 = iconst.i64 0
    v4 = icmp eq v0, v3
    brif v4, block1, block2
block1:
    return v0
block2:
    return v3
}",
        );
        assert_eq!(h, vec![Some(0), None, None]);
    }
}

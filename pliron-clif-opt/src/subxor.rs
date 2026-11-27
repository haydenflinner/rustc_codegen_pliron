//! `isub` on an all-ones minuend is `bxor` (`PLIRON_SUBXOR`, opt-in).
//!
//! `M - t` with `M = 2^k - 1` and `t <= M` is `t ^ M`: the borrow
//! propagates through every set bit, which is exactly the ones
//! complement inside `M`'s bits. The motivating shape is
//! `NonZero::ilog2` = `BITS - 1 - clz` — `63 - clz(x)` when `x != 0`,
//! which `ctlz_nonzero` proves (`lower.rs` records the count's range as
//! a `vfacts` entry, and `ranges` tightens `clz`/`ctz` of any
//! range-proven nonzero operand). `sub` needs a `movz` for the mask
//! and the `sub` itself; `bxor` is `eor` with the mask an immediate —
//! one instruction saved per site.

use cranelift_codegen::ir::{Function, InstructionData, Opcode};

use crate::pipeline::{Analysis, CfgDtLa};
use crate::ranges::{Ranges, bits_of};

pub fn run(func: &mut Function, an: &mut Analysis, facts: &crate::vfacts::VFacts) -> usize {
    let mut fixes = Vec::new();
    {
        let CfgDtLa { cfg, dt, .. } = an.cfg_dt_la(func);
        let mut rs = Ranges::new(func, cfg).with_facts(facts);
        for b in func.layout.blocks() {
            if !dt.is_reachable(b) {
                continue;
            }
            for inst in func.layout.block_insts(b) {
                let InstructionData::Binary {
                    opcode: Opcode::Isub,
                    args: [x, y],
                } = func.dfg.insts[inst]
                else {
                    continue;
                };
                let Some(wb) = bits_of(func.dfg.value_type(y)) else { continue };
                let wmask = if wb >= 64 { u64::MAX } else { (1u64 << wb) - 1 };
                let Some(k) = crate::loopidiom::iconst(func, func.dfg.resolve_aliases(x)) else { continue };
                let k = k as u64 & wmask;
                if k.count_ones() != k.trailing_ones() {
                    continue;
                }
                let y = func.dfg.resolve_aliases(y);
                if rs.at(func, dt, y, b).uhi <= k {
                    fixes.push(inst);
                }
            }
        }
    }
    for &inst in &fixes {
        let InstructionData::Binary { args, .. } = func.dfg.insts[inst] else { continue };
        func.dfg.insts[inst] = InstructionData::Binary { opcode: Opcode::Bxor, args };
    }
    if !fixes.is_empty() {
        an.invalidate(func);
    }
    fixes.len()
}

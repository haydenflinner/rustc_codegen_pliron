//! Lowering rules for AArch64.
//!
//! TODO: opportunities for better code generation:
//!
//! - Smarter use of addressing modes. Recognize a+SCALE*b patterns. Recognize
//!   pre/post-index opportunities.
//!
//! - Floating-point immediates (FIMM instruction).

use crate::inst_predicates::{
    has_lowering_side_effect, inst_addr_offset_type, inst_store_data, is_constant_64bit,
};
use crate::ir::Inst as IRInst;
use crate::ir::condcodes::{FloatCC, IntCC};
use crate::ir::types;
use crate::ir::{Function, InstructionData, Opcode, Value};
use crate::isa::aarch64::AArch64Backend;
use crate::isa::aarch64::inst::*;
use crate::machinst::*;
use smallvec::smallvec;

pub mod isle;

//============================================================================
// Lowering: convert instruction inputs to forms that we can use.

fn get_as_extended_value(ctx: &mut Lower<Inst>, val: Value) -> Option<(Value, ExtendOp)> {
    let inputs = ctx.get_value_as_source_or_const(val);
    let (insn, n) = inputs.inst.as_inst()?;
    if n != 0 {
        return None;
    }
    let op = ctx.data(insn).opcode();
    let out_ty = ctx.output_ty(insn, 0);
    let out_bits = ty_bits(out_ty);

    // Is this a zero-extend or sign-extend and can we handle that with a register-mode operator?
    if op == Opcode::Uextend || op == Opcode::Sextend {
        let sign_extend = op == Opcode::Sextend;
        let inner_ty = ctx.input_ty(insn, 0);
        let inner_bits = ty_bits(inner_ty);
        assert!(inner_bits < out_bits);
        let extendop = match (sign_extend, inner_bits) {
            (true, 8) => ExtendOp::SXTB,
            (false, 8) => ExtendOp::UXTB,
            (true, 16) => ExtendOp::SXTH,
            (false, 16) => ExtendOp::UXTH,
            (true, 32) => ExtendOp::SXTW,
            (false, 32) => ExtendOp::UXTW,
            _ => unreachable!(),
        };
        return Some((ctx.input_as_value(insn, 0), extendop));
    }

    None
}

pub(crate) fn lower_condcode(cc: IntCC) -> Cond {
    match cc {
        IntCC::Equal => Cond::Eq,
        IntCC::NotEqual => Cond::Ne,
        IntCC::SignedGreaterThanOrEqual => Cond::Ge,
        IntCC::SignedGreaterThan => Cond::Gt,
        IntCC::SignedLessThanOrEqual => Cond::Le,
        IntCC::SignedLessThan => Cond::Lt,
        IntCC::UnsignedGreaterThanOrEqual => Cond::Hs,
        IntCC::UnsignedGreaterThan => Cond::Hi,
        IntCC::UnsignedLessThanOrEqual => Cond::Ls,
        IntCC::UnsignedLessThan => Cond::Lo,
    }
}

//=============================================================================
// PLIRON: post-index load/store fusion.
//
// When lowering `vN = iadd vB, C` (or `isub` / `iadd_imm`), if the most
// recent side-effecting instruction earlier in the same block is a
// scalar `load vB` / `store vD, vB` at offset 0 whose load result is
// not used in between, fuse it into `load v, [vB], #C` /
// `store vD, [vB], #C`: the fused inst defines the load result's
// canonical vreg directly, and `vN`'s value becomes a reuse-def tied
// to `vB`'s vreg so the register allocator copies the old base once
// for the writeback. `PLIRON_POSTIDX=0` disables.

/// The constant operand of `iconst`-produced value, if any.
fn postindex_const(f: &Function, val: Value) -> Option<i64> {
    let inst = f.dfg.value_def(f.dfg.resolve_aliases(val)).inst()?;
    is_constant_64bit(f, inst).map(|k| k as i64)
}

enum PostIndexCand {
    Load(LoadPostOp),
    Store(StorePostOp, Value),
}

/// If `inst` is a single scalar load/store through `[base + 0]`, return
/// how it fuses.
fn postindex_mem(f: &Function, inst: IRInst, base: Value) -> Option<PostIndexCand> {
    let (addr, offset, ty) = inst_addr_offset_type(f, inst)?;
    if i64::from(offset) != 0 || f.dfg.resolve_aliases(addr) != base {
        return None;
    }
    let kind = match f.dfg.insts[inst].opcode() {
        Opcode::Uload8 => PostIndexCand::Load(LoadPostOp::ULoad8),
        Opcode::Sload8 => PostIndexCand::Load(LoadPostOp::SLoad8),
        Opcode::Uload16 => PostIndexCand::Load(LoadPostOp::ULoad16),
        Opcode::Sload16 => PostIndexCand::Load(LoadPostOp::SLoad16),
        Opcode::Uload32 => PostIndexCand::Load(LoadPostOp::ULoad32),
        Opcode::Sload32 => PostIndexCand::Load(LoadPostOp::SLoad32),
        Opcode::Load => PostIndexCand::Load(match ty {
            types::I8 => LoadPostOp::ULoad8,
            types::I16 => LoadPostOp::ULoad16,
            types::I32 => LoadPostOp::ULoad32,
            types::I64 => LoadPostOp::ULoad64,
            types::F16 => LoadPostOp::FpuLoad16,
            types::F32 => LoadPostOp::FpuLoad32,
            types::F64 => LoadPostOp::FpuLoad64,
            types::F128 => LoadPostOp::FpuLoad128,
            ty if ty.is_vector() && ty.bytes() == 16 => LoadPostOp::FpuLoad128,
            _ => return None,
        }),
        Opcode::Istore8 => PostIndexCand::Store(StorePostOp::Store8, inst_store_data(f, inst)?),
        Opcode::Store => PostIndexCand::Store(
            match ty {
                types::I8 => StorePostOp::Store8,
                types::I16 => StorePostOp::Store16,
                types::I32 => StorePostOp::Store32,
                types::I64 => StorePostOp::Store64,
                types::F16 => StorePostOp::FpuStore16,
                types::F32 => StorePostOp::FpuStore32,
                types::F64 => StorePostOp::FpuStore64,
                types::F128 => StorePostOp::FpuStore128,
                ty if ty.is_vector() && ty.bytes() == 16 => StorePostOp::FpuStore128,
                _ => return None,
            },
            inst_store_data(f, inst)?,
        ),
        _ => return None,
    };
    Some(kind)
}

fn try_postindex(ctx: &mut Lower<Inst>, ir_inst: IRInst) -> Option<InstOutput> {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("PLIRON_POSTIDX").map_or(true, |v| v != "0"))
    {
        return None;
    }
    let f = ctx.f;

    // `vN = iadd vB, C` / `isub vB, C`, 64-bit only.
    let (base, wb) = match ctx.data(ir_inst) {
        &InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } => postindex_const(f, args[1])
            .map(|k| (args[0], k))
            .or_else(|| postindex_const(f, args[0]).map(|k| (args[1], k)))?,
        &InstructionData::Binary {
            opcode: Opcode::Isub,
            args,
        } => (args[0], postindex_const(f, args[1])?.checked_neg()?),
        _ => return None,
    };
    if wb == 0 || !(-256..=255).contains(&wb) {
        return None;
    }
    if f.dfg.inst_results(ir_inst).len() != 1
        || f.dfg.value_type(f.dfg.inst_results(ir_inst)[0]) != types::I64
    {
        return None;
    }
    let base = f.dfg.resolve_aliases(base);

    // The most recent side-effecting inst before `ir_inst` (sunk insts
    // are transparent — their effects moved elsewhere) must be the
    // load/store to fuse: `merge_side_effecting` re-checks this through
    // the color accounting anyway, but locating it here lets us look at
    // its shape first.
    let block = f.layout.inst_block(ir_inst)?;
    let mut cand = None;
    for inst in f.layout.block_insts(block) {
        if inst == ir_inst {
            break;
        }
        if !ctx.is_inst_sunk(inst) && has_lowering_side_effect(f, inst) {
            cand = Some(inst);
        }
    }
    let cand = cand?;
    let kind = postindex_mem(f, cand, base)?;

    // `base` may only be used inside `block`, at positions no later
    // than `ir_inst`: the writeback happens at the add's slot, so a use
    // after it (or in any other block, which necessarily executes after
    // the writeback when it executes at all) would observe the
    // post-increment value. Uses earlier in the block still read the
    // old value, exactly as before — except a use by a *sunk* inst
    // in between, whose args are consumed at its sink site, which may
    // be at or after `ir_inst`: reject those too.
    let mut past_add = false;
    let mut between = false;
    for inst in f.layout.block_insts(block) {
        if inst == ir_inst {
            past_add = true;
            break;
        }
        if inst == cand {
            between = true;
            continue;
        }
        if between
            && ctx.is_inst_sunk(inst)
            && f.dfg.inst_values(inst).any(|u| f.dfg.resolve_aliases(u) == base)
        {
            return None;
        }
    }
    if past_add {
        let mut past = false;
        for inst in f.layout.block_insts(block) {
            if inst == ir_inst {
                past = true;
                continue;
            }
            if past && f.dfg.inst_values(inst).any(|u| f.dfg.resolve_aliases(u) == base) {
                return None;
            }
        }
    }
    for bb in f.layout.blocks() {
        if bb == block {
            continue;
        }
        for inst in f.layout.block_insts(bb) {
            if f.dfg.inst_values(inst).any(|u| f.dfg.resolve_aliases(u) == base) {
                return None;
            }
        }
    }

    // A fused load defines its result's canonical vreg directly; the
    // value must not be consumed by anything between the load and the
    // add (nor by the add itself — impossible in practice since the
    // add's args are checked above, but be explicit).
    let ld_result = f
        .dfg
        .inst_results(cand)
        .first()
        .map(|v| f.dfg.resolve_aliases(*v));
    if let Some(v) = ld_result {
        let mut in_range = false;
        for inst in f.layout.block_insts(block) {
            if inst == cand {
                in_range = true;
                continue;
            }
            if !in_range {
                continue;
            }
            if f.dfg.inst_values(inst).any(|u| f.dfg.resolve_aliases(u) == v) {
                return None;
            }
            if inst == ir_inst {
                break;
            }
        }
    }

    // Everything is checked; claim the load/store. Fails only if some
    // other side effect is scheduled between it and this point (the
    // color check) — then the reorder would be unsafe.
    if !ctx.merge_side_effecting(cand) {
        return None;
    }

    let flags = match f.dfg.insts[cand] {
        InstructionData::Load { flags, .. }
        | InstructionData::LoadNoOffset { flags, .. }
        | InstructionData::Store { flags, .. }
        | InstructionData::StoreNoOffset { flags, .. } => f.dfg.mem_flags[flags],
        _ => unreachable!(),
    };
    let rn = ctx.put_value_in_regs(base).only_reg().unwrap();
    let simm9 = SImm9::maybe_from_i64(wb).expect("range checked above");
    let rwb = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    match kind {
        PostIndexCand::Load(op) => {
            let rd =
                Writable::from_reg(ctx.get_value_regs(ld_result.unwrap()).only_reg().unwrap());
            ctx.emit(Inst::LoadPost {
                op,
                rd,
                rwb,
                rn,
                simm9,
                flags,
            });
        }
        PostIndexCand::Store(op, data) => {
            let rt = ctx
                .put_value_in_regs(f.dfg.resolve_aliases(data))
                .only_reg()
                .unwrap();
            ctx.emit(Inst::StorePost {
                op,
                rt,
                rwb,
                rn,
                simm9,
                flags,
            });
        }
    }
    // `vN`'s value is the writeback temp; the framework aliases the
    // add's canonical vregs to it.
    Some(smallvec![ValueRegs::one(rwb.to_reg())])
}

//=============================================================================
// Lowering-backend trait implementation.

impl LowerBackend for AArch64Backend {
    type MInst = Inst;

    fn lower(&self, ctx: &mut Lower<Inst>, ir_inst: IRInst) -> Option<InstOutput> {
        if let out @ Some(_) = try_postindex(ctx, ir_inst) {
            return out;
        }
        isle::lower(ctx, self, ir_inst)
    }

    fn lower_branch(
        &self,
        ctx: &mut Lower<Inst>,
        ir_inst: IRInst,
        targets: &[MachLabel],
    ) -> Option<()> {
        isle::lower_branch(ctx, self, ir_inst, targets)
    }

    fn maybe_pinned_reg(&self) -> Option<Reg> {
        Some(regs::pinned_reg())
    }
}

//! Lowering rules for AArch64.
//!
//! TODO: opportunities for better code generation:
//!
//! - Smarter use of addressing modes. Recognize a+SCALE*b patterns. Recognize
//!   pre/post-index opportunities.
//!
//! - Floating-point immediates (FIMM instruction).

use crate::inst_predicates::{
    inst_addr_offset_type, inst_store_data, is_constant_64bit,
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
    static POSTIDX_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("PLIRON_POSTIDX").map_or(true, |v| v != "0"))
    {
        return None;
    }
    let f = ctx.f;
    if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
        if let InstructionData::Binary { opcode, .. } = ctx.data(ir_inst) {
            if matches!(opcode, Opcode::Iadd | Opcode::Isub) {
                eprintln!(
                    "postidx {} {}@{:?}: seen pending={}",
                    f.name,
                    f.dfg.display_inst(ir_inst),
                    f.layout.inst_block(ir_inst),
                    ctx.has_pending_postindex()
                );
            }
        }
    }
    if ctx.has_pending_postindex() {
        return None;
    }

    // `vN = iadd vB, C` / `isub vB, C`, 64-bit only.
    let pair = match ctx.data(ir_inst) {
        &InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } => postindex_const(f, args[1])
            .map(|k| (args[0], k))
            .or_else(|| postindex_const(f, args[0]).map(|k| (args[1], k))),
        &InstructionData::Binary {
            opcode: Opcode::Isub,
            args,
        } => postindex_const(f, args[1])
            .and_then(|k| k.checked_neg())
            .map(|k| (args[0], k)),
        _ => None,
    };
    let Some((base, wb)) = pair else {
        if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
            if let InstructionData::Binary { args, .. } = ctx.data(ir_inst) {
                eprintln!(
                    "postidx {} {ir_inst}: no const in args {:?} ({:?})",
                    f.name,
                    args,
                    args.map(|a| f.dfg.value_def(f.dfg.resolve_aliases(a)))
                );
            }
        }
        return None;
    };
    let debug = *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some());
    if wb == 0 || !(-256..=255).contains(&wb) {
        return None;
    }
    if debug {
        eprintln!("postidx {} {ir_inst}: iadd base {base} wb {wb}", f.name);
    }
    if f.dfg.inst_results(ir_inst).len() != 1
        || f.dfg.value_type(f.dfg.inst_results(ir_inst)[0]) != types::I64
    {
        return None;
    }
    let base = f.dfg.resolve_aliases(base);
    let block = f.layout.inst_block(ir_inst)?;

    // Find the nearest scalar load/store through `[base + 0]` earlier
    // in this block. The fused inst is emitted at the load/store's own
    // position (via `pending_postindex`), so the memory access does not
    // move at all — only the pointer increment disappears.
    let mut cand = None;
    for inst in f.layout.block_insts(block) {
        if inst == ir_inst {
            break;
        }
        if !ctx.is_inst_sunk(inst) && postindex_mem(f, inst, base).is_some() {
            cand = Some(inst);
        }
    }
    let Some(cand) = cand else {
        if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
            eprintln!(
                "postidx {} {ir_inst}@{:?}: no cand on base {base}",
                f.name,
                f.layout.inst_block(ir_inst)
            );
        }
        return None;
    };
    let Some(kind) = postindex_mem(f, cand, base) else {
        if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
            eprintln!("postidx {} {ir_inst}: cand {cand} not mem", f.name);
        }
        return None;
    };

    // The writeback is modeled as a use of `base`'s vreg plus a
    // reuse-def tied to it, so the register allocator preserves `base`'s
    // old value with a copy whenever it is still needed. That copy costs
    // as much as the `add` we remove, so fuse only when `base` is dead
    // at the fused inst: no use of `base` other than `ir_inst`'s arg,
    // at or after `cand`'s position, in any block.
    let mut in_range = false;
    for inst in f.layout.block_insts(block) {
        if inst == cand {
            in_range = true;
            continue;
        }
        if !in_range || inst == ir_inst {
            continue;
        }
        if f.dfg.inst_values(inst).any(|u| f.dfg.resolve_aliases(u) == base) {
            if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
                eprintln!("postidx {} {ir_inst}: base live at {inst}", f.name);
            }
            return None;
        }
    }
    for bb in f.layout.blocks() {
        if bb == block {
            continue;
        }
        for inst in f.layout.block_insts(bb) {
            if f.dfg.inst_values(inst).any(|u| f.dfg.resolve_aliases(u) == base) {
                if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
                    eprintln!("postidx {} {ir_inst}: base live in {bb} at {inst}", f.name);
                }
                return None;
            }
        }
    }

    // The pending inst is emitted when the scan reaches `cand`. `cand`
    // can only be stolen from under us if some inst in between sinks it
    // (requires the load result's sole use to be that inst), so a
    // single-use result with a user in between is rejected. Multi-use
    // results can't be sunk at all; uses in other blocks are already
    // lowered (a sinking user would already show in `inst_sunk`).
    let ld_result = f
        .dfg
        .inst_results(cand)
        .first()
        .map(|v| f.dfg.resolve_aliases(*v));
    if let Some(v) = ld_result {
        if !ctx.value_has_multiple_uses(v) {
            if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
                eprintln!("postidx {} {ir_inst}: cand {cand} single-use result {v}", f.name);
            }
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
    }

    if *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some()) {
        eprintln!("postidx {} {ir_inst}@{block:?}: FUSED cand {cand}", f.name);
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
            let rd = ctx.alloc_tmp(ctx.output_ty(cand, 0)).only_reg().unwrap();
            ctx.set_pending_postindex(
                cand,
                Inst::LoadPost {
                    op,
                    rd,
                    rwb,
                    rn,
                    simm9,
                    flags,
                },
                smallvec![ValueRegs::one(rd.to_reg())],
            );
        }
        PostIndexCand::Store(op, data) => {
            let rt = ctx
                .put_value_in_regs(f.dfg.resolve_aliases(data))
                .only_reg()
                .unwrap();
            ctx.set_pending_postindex(
                cand,
                Inst::StorePost {
                    op,
                    rt,
                    rwb,
                    rn,
                    simm9,
                    flags,
                },
                smallvec![],
            );
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
        if let Some((inst, out)) = ctx.take_pending_postindex(ir_inst) {
            ctx.emit(inst);
            return Some(out);
        }
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

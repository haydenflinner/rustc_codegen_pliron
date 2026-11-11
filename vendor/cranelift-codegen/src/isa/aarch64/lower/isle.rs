//! ISLE integration glue code for aarch64 lowering.

// Pull in the ISLE generated code.
pub mod generated_code;
use generated_code::{ALUOp, CondResult, Context, ImmExtend, ProducesFlags};

// Types that the generated ISLE code uses via `use super::*`.
use super::{
    ASIMDFPModImm, ASIMDMovModImm, BranchTarget, CallInfo, Cond, CondBrKind, ExtendOp, FPUOpRI,
    FPUOpRIMod, FloatCC, Imm12, ImmLogic, ImmShift, Inst as MInst, IntCC, MachLabel, MemLabel,
    MoveWideConst, MoveWideOp, NZCV, Opcode, OperandSize, Reg, SImm9, ScalarSize, ShiftOpAndAmt,
    UImm5, UImm12Scaled, VecMisc2, VectorSize, fp_reg, lower_condcode, stack_reg,
    writable_link_reg, writable_zero_reg, zero_reg,
};
use crate::ir::{ArgumentExtension, condcodes};
use crate::isa;
use crate::isa::aarch64::AArch64Backend;
use crate::isa::aarch64::inst::{FPULeftShiftImm, FPURightShiftImm, ReturnCallInfo};
use crate::machinst::isle::*;
use crate::{
    binemit::CodeOffset,
    ir::{
        AtomicRmwOp, BlockCall, ExternalName, Inst, InstructionData, MemFlagsData, TrapCode, Value,
        ValueList, immediates::*, types::*,
    },
    isa::aarch64::abi::AArch64MachineDeps,
    isa::aarch64::inst::SImm7Scaled,
    isa::aarch64::inst::args::{ShiftOp, ShiftOpShiftImm},
    machinst::{
        CallArgList, CallRetList, InstOutput, MachInst, VCodeConstant, VCodeConstantData,
        abi::ArgPair, ty_bits,
    },
};
use alloc::boxed::Box;
use alloc::vec::Vec;
use regalloc2::PReg;

type BoxCallInfo = Box<CallInfo<ExternalName>>;
type BoxCallIndInfo = Box<CallInfo<Reg>>;
type BoxReturnCallInfo = Box<ReturnCallInfo<ExternalName>>;
type BoxReturnCallIndInfo = Box<ReturnCallInfo<Reg>>;
type VecMachLabel = Vec<MachLabel>;
type BoxExternalName = Box<ExternalName>;
type VecArgPair = Vec<ArgPair>;

/// The main entry point for lowering with ISLE.
pub(crate) fn lower(
    lower_ctx: &mut Lower<MInst>,
    backend: &AArch64Backend,
    inst: Inst,
) -> Option<InstOutput> {
    // TODO: reuse the ISLE context across lowerings so we can reuse its
    // internal heap allocations.
    let mut isle_ctx = IsleContext { lower_ctx, backend };
    generated_code::constructor_lower(&mut isle_ctx, inst)
}

pub(crate) fn lower_branch(
    lower_ctx: &mut Lower<MInst>,
    backend: &AArch64Backend,
    branch: Inst,
    targets: &[MachLabel],
) -> Option<()> {
    // TODO: reuse the ISLE context across lowerings so we can reuse its
    // internal heap allocations.
    let mut isle_ctx = IsleContext { lower_ctx, backend };
    generated_code::constructor_lower_branch(&mut isle_ctx, branch, targets)
}

/// PLIRON: the `val_already_extended` oracle for `Uext`, from Rust.
pub(crate) fn val_uext_clean(lower_ctx: &mut Lower<MInst>, backend: &AArch64Backend, val: Value) -> bool {
    let mut isle_ctx = IsleContext { lower_ctx, backend };
    generated_code::constructor_val_already_extended(&mut isle_ctx, &ArgumentExtension::Uext, val)
        == Some(true)
}

/// PLIRON: `PLIRON_NARROWEXT` (default on; `=0` off) turns on the extra
/// narrow-int cleanliness facts (`narrowext_clean`) and the rules that
/// consume them: skipping the return-value `uxtb`/`uxth`, `uextend`
/// passthrough, `cbnz` on a clean narrow `brif`, and `band (ireduce x) k`
/// reading `x` directly.
pub(crate) fn narrowext_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PLIRON_NARROWEXT").as_deref() != Ok("0"))
}

/// PLIRON: one `PLIRON_NARROWEXT` user, by bit, for bisecting:
/// 1 = the extra cleanliness facts, 2 = return extension (the ABI
/// `uext`-on-retval skip and the `ret (ireduce)` source sink, both via
/// `narrow_ret_uext_clean`), 4 = `uextend`
/// passthrough, 8 = `cbnz` on a clean byte, 16 = `band (ireduce x) k`,
/// 32 = narrow `ushr`-of-clean and `ushr`-operand fusion on narrow
/// `bor`/`band`/`bxor`, 64 = narrow `ireduce` passthrough when the wide
/// source is already clean (`wide_uext_clean`), 128 = `ireduce` source
/// fusions (`ushr`+`ireduce` → `ubfm`, `load`+`ireduce` → narrow `uload`,
/// `istore8`/`store` of `ireduce` → store the wide reg), 256 = `ireduce`
/// consumer sinks: `ushr` by a constant → `ubfx` on the wide register,
/// chained narrow `ireduce`s extend once, and `select` arms read the
/// wide source directly (the low bits are exact and the result is
/// modelled dirty anyway).
/// `PLIRON_NARROWEXT_MASK` (default all) selects which are live.
pub(crate) fn narrowext_part(bit: u64) -> bool {
    static MASK: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    narrowext_enabled()
        && *MASK.get_or_init(|| {
            std::env::var("PLIRON_NARROWEXT_MASK")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(u64::MAX)
        }) & bit
            != 0
}

/// PLIRON: is the register of `v` (an i8/i16 value) zero-extended through
/// all 64 bits? Covers the producers `val_already_extended` leaves out:
/// canonical narrow `iconst` (materialized by `movz` or a masked constant
/// load), `band` with a clean operand, `bor`/`bxor` with two clean operands
/// (all three lower to exact 32-bit `and`/`orr`/`eor`; the `bic`/`orn`/`eon`
/// forms only match through a `bnot` operand, which is never clean), plus
/// the existing leaf cases so the recursion sees through them. `budget`
/// bounds the walk; running out answers "dirty".
pub(crate) fn narrow_uext_clean(dfg: &crate::ir::DataFlowGraph, v: Value, budget: &mut u32) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let ty = dfg.value_type(v);
    let mask: u64 = match ty {
        I8 => 0xff,
        I16 => 0xffff,
        _ => return false,
    };
    let Some(inst) = dfg.value_def(v).inst() else {
        return false;
    };
    match &dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => (imm.bits() as u64) <= mask,
        InstructionData::IntCompare { .. } | InstructionData::FloatCompare { .. } => true,
        InstructionData::Unary {
            opcode: Opcode::Uextend | Opcode::Ireduce | Opcode::BlackBox,
            ..
        } => true,
        InstructionData::Load {
            opcode: Opcode::Load | Opcode::Uload8,
            ..
        } => true,
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => {
            let [a, b] = *args;
            narrow_uext_clean(dfg, b, budget) || narrow_uext_clean(dfg, a, budget)
        }
        InstructionData::Binary {
            opcode: Opcode::Bor | Opcode::Bxor,
            args,
        } => {
            let [a, b] = *args;
            narrow_uext_clean(dfg, a, budget) && narrow_uext_clean(dfg, b, budget)
        }
        // A narrow `ushr`'s register is clean either way: the default
        // lowering zero-extends the operand first, and the clean-operand
        // rule (`ushr_clean_narrow`) shifts an already-narrow register.
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            ..
        } => true,
        _ => false,
    }
}

/// PLIRON: bits of `v` known zero — a bounded known-bits walk for the
/// `bfi` fusion (`y | field` lowers to `bfm` only when `y`'s field bits
/// are all provably zero). Answers a `u64` mask; a miss returns 0
/// (nothing known), never a wrong bit. Only low-bandwidth producers are
/// followed; `budget` bounds the walk.
fn bf_known_zero(dfg: &crate::ir::DataFlowGraph, v: Value, budget: &mut u32) -> u64 {
    if *budget == 0 {
        return 0;
    }
    *budget -= 1;
    let ty = dfg.value_type(v);
    let Some(inst) = dfg.value_def(v).inst() else {
        return 0;
    };
    match &dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => !imm.bits() as u64,
        InstructionData::Binary {
            opcode: Opcode::Band,
            args: [a, b],
        } => bf_known_zero(dfg, *a, budget) | bf_known_zero(dfg, *b, budget),
        InstructionData::Binary {
            opcode: Opcode::Bor | Opcode::Bxor,
            args: [a, b],
        } => bf_known_zero(dfg, *a, budget) & bf_known_zero(dfg, *b, budget),
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [a, k],
        } => {
            let Some(k) = iconst_bits(dfg, *k) else {
                return 0;
            };
            if k >= 64 {
                return !0u64;
            }
            (bf_known_zero(dfg, *a, budget) << k) | ((1u64 << k) - 1)
        }
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            args: [a, k],
        } => {
            let Some(k) = iconst_bits(dfg, *k) else {
                return 0;
            };
            let tz = ty.bits().min(64) as u64;
            if k >= tz {
                return !0u64;
            }
            (bf_known_zero(dfg, *a, budget) >> k) | (!0u64 << (tz - k))
        }
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => {
            let from = dfg.value_type(*arg).bits() as u64;
            if from >= 64 {
                return 0;
            }
            (!0u64 << from) | (bf_known_zero(dfg, *arg, budget) & ((1u64 << from) - 1))
        }
        // Load width, not result width: `uload8.i64` leaves 56 clean
        // high bits.
        InstructionData::Load {
            opcode: Opcode::Uload8,
            ..
        } => !0u64 << 8,
        InstructionData::Load {
            opcode: Opcode::Uload16,
            ..
        } => !0u64 << 16,
        InstructionData::Load {
            opcode: Opcode::Uload32,
            ..
        } => !0u64 << 32,
        InstructionData::IntCompare { .. } | InstructionData::FloatCompare { .. } => !1u64,
        InstructionData::Unary {
            opcode: Opcode::Ireduce | Opcode::BlackBox,
            arg,
        } => bf_known_zero(dfg, *arg, budget),
        _ => 0,
    }
}

/// PLIRON: bits of `v`'s register above `bits` are all zero — the wide
/// operand of a narrow `ireduce` that needs no `uxtb`/`uxth` of its own
/// (`PLIRON_NARROWEXT` bit 64). `narrow_uext_clean` answers this at the
/// value's own i8/i16 width; this answers it for the wider (i16/i32/i64)
/// sources. `budget` bounds the walk; running out answers "dirty".
pub(crate) fn wide_uext_clean(
    dfg: &crate::ir::DataFlowGraph,
    v: Value,
    bits: u32,
    budget: &mut u32,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    if dfg.value_type(v).bits() <= bits {
        return false;
    }
    let Some(inst) = dfg.value_def(v).inst() else {
        return false;
    };
    match &dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => (imm.bits() as u64) >> bits == 0,
        InstructionData::IntCompare { .. } | InstructionData::FloatCompare { .. } => true,
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => {
            dfg.value_type(*arg).bits() <= bits
                || wide_uext_clean(dfg, *arg, bits, budget)
        }
        // A narrow `ireduce`/`black_box` materializes `uxtb`/`uxth`, so the
        // register is clean at the value's own width; clean above a smaller
        // `bits` only when the source already is.
        InstructionData::Unary {
            opcode: Opcode::Ireduce | Opcode::BlackBox,
            arg,
        } => wide_uext_clean(dfg, *arg, bits, budget),
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => {
            let [a, b] = *args;
            wide_uext_clean(dfg, a, bits, budget) || wide_uext_clean(dfg, b, bits, budget)
        }
        InstructionData::Binary {
            opcode: Opcode::Bor | Opcode::Bxor,
            args,
        } => {
            let [a, b] = *args;
            wide_uext_clean(dfg, a, bits, budget) && wide_uext_clean(dfg, b, bits, budget)
        }
        // `x >> k` with a constant `k` leaves `ty_bits - k` live bits; the
        // whole register is clean when that fits under `bits`.
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            args,
        } => {
            let [x, k] = *args;
            let _ = x;
            let Some(k) = iconst_bits(dfg, k) else {
                return false;
            };
            let ty_bits = dfg.value_type(v).bits() as u64;
            k < ty_bits && ty_bits - k <= bits as u64
        }
        InstructionData::Ternary {
            opcode: Opcode::Select | Opcode::SelectSpectreGuard,
            args,
        } => {
            let [_, a, b] = *args;
            wide_uext_clean(dfg, a, bits, budget) && wide_uext_clean(dfg, b, bits, budget)
        }
        _ => false,
    }
}

fn iconst_bits(dfg: &crate::ir::DataFlowGraph, v: Value) -> Option<u64> {
    let inst = dfg.value_def(v).inst()?;
    if let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = &dfg.insts[inst]
    {
        Some(imm.bits() as u64)
    } else {
        None
    }
}

/// `(1<<w)-1` → `w`, when `1 <= w < bits`.
fn bf_low_w(bits: u32, m: u64) -> Option<u32> {
    let w = m.checked_add(1)?.trailing_zeros();
    (w >= 1 && w < bits && (1u64 << w) - 1 == m).then_some(w)
}

/// `((1<<w)-1) << k` → `w`, when `k >= 1`.
fn bf_field_w(bits: u32, m: u64, k: u64) -> Option<u32> {
    if k == 0 || k >= bits as u64 {
        return None;
    }
    let mm = m >> k;
    if mm.checked_shl(k as u32)? != m {
        return None;
    }
    bf_low_w(bits, mm)
}

/// PLIRON: does `cond` hold under the NZCV bit pattern `bits`
/// (n<<3|z<<2|c<<1|v)? Used by `nzcv_for_cond` to pick the `ccmp`
/// constant-NZCV operand.
fn eval_cond(cond: Cond, bits: u32) -> bool {
    let (n, z, c, v) = (bits & 8 != 0, bits & 4 != 0, bits & 2 != 0, bits & 1 != 0);
    match cond {
        Cond::Eq => z,
        Cond::Ne => !z,
        Cond::Hs => c,
        Cond::Lo => !c,
        Cond::Mi => n,
        Cond::Pl => !n,
        Cond::Vs => v,
        Cond::Vc => !v,
        Cond::Hi => c && !z,
        Cond::Ls => !c || z,
        Cond::Ge => n == v,
        Cond::Lt => n != v,
        Cond::Gt => !z && n == v,
        Cond::Le => z || n != v,
        Cond::Al => true,
        Cond::Nv => false,
    }
}

/// PLIRON (`PLIRON_CCMP`, default on): fuse integer-compare bool
/// combinations into `cmp; ccmp` flag chains — see the `ccmp_*`
/// comment block in inst.isle.
fn ccmp_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PLIRON_CCMP").as_deref() != Ok("0"))
}

/// An NZCV constant that makes `cond` evaluate to `value` — the
/// `ccmp` immediate forcing the fused result when the primary
/// comparison's condition didn't hold. `None` when no flag state
/// yields `value` (`al` forced false / `nv` forced true).
fn nzcv_for_cond(cond: &Cond, value: bool) -> Option<NZCV> {
    (0..16)
        .map(|bits| NZCV::new(bits & 8 != 0, bits & 4 != 0, bits & 2 != 0, bits & 1 != 0))
        .find(|nzcv| eval_cond(*cond, nzcv.bits()) == value)
}

/// Try the two compares in both primary/secondary orders — `band`
/// and `bor` operands have no canonical order, and the secondary's
/// operands must be 32/64-bit (`ccmp` reg/reg form).
fn ccmp_pair(
    ctx: &mut IsleContext<'_, '_, MInst, AArch64Backend>,
    mode: u8,
    cc_a: &IntCC,
    x: Value,
    y: Value,
    cc_b: &IntCC,
    u: Value,
    v: Value,
) -> Option<CondResult> {
    let cr_a = generated_code::constructor_emit_icmp(ctx, cc_a, x, y);
    if let Some(cr) = ccmp_chain(ctx, mode, &cr_a, cc_b, u, v) {
        return Some(cr);
    }
    let cr_b = generated_code::constructor_emit_icmp(ctx, cc_b, u, v);
    ccmp_chain(ctx, mode, &cr_b, cc_a, x, y)
}

/// Build `cmp_primary; ccmp u,v,#nzcv,guard` as one flag chain whose
/// final condition is the secondary compare's. Declines when the
/// primary isn't a single flag-producing instruction (the
/// `ProducesFlags` enum can't express chains longer than two) or the
/// secondary operands aren't 32/64-bit.
fn ccmp_chain(
    ctx: &mut IsleContext<'_, '_, MInst, AArch64Backend>,
    mode: u8,
    cr: &CondResult,
    cc_b: &IntCC,
    u: Value,
    v: Value,
) -> Option<CondResult> {
    let (flags_a, cond_a) = match cr {
        CondResult::Cond {
            flags: ProducesFlags::ProducesFlagsSideEffect { inst },
            cond,
        } => (inst.clone(), *cond),
        _ => return None,
    };
    let size = match ctx.value_type(u) {
        I32 => OperandSize::Size32,
        I64 => OperandSize::Size64,
        _ => return None,
    };
    let cond_b = ctx.cond_code(cc_b);
    let nzcv = nzcv_for_cond(&cond_b, mode & 2 != 0)?;
    let guard = if mode & 1 != 0 {
        cond_a.invert()
    } else {
        cond_a
    };
    Some(CondResult::Cond {
        flags: ProducesFlags::ProducesFlagsTwiceSideEffect {
            inst1: flags_a,
            inst2: MInst::CCmp {
                size,
                rn: ctx.put_in_reg(u),
                rm: ctx.put_in_reg(v),
                nzcv,
                cond: guard,
            },
        },
        cond: cond_b,
    })
}

pub struct ExtendedValue {
    val: Value,
    extend: ExtendOp,
}

impl Context for IsleContext<'_, '_, MInst, AArch64Backend> {
    isle_lower_prelude_methods!();

    fn gen_call_info(
        &mut self,
        sig: Sig,
        dest: ExternalName,
        uses: CallArgList,
        defs: CallRetList,
        try_call_info: Option<TryCallInfo>,
        patchable: bool,
    ) -> BoxCallInfo {
        let stack_ret_space = self.lower_ctx.sigs()[sig].sized_stack_ret_space();
        let stack_arg_space = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_outgoing_args_size(stack_ret_space + stack_arg_space);

        Box::new(
            self.lower_ctx
                .gen_call_info(sig, dest, uses, defs, try_call_info, patchable),
        )
    }

    fn gen_call_ind_info(
        &mut self,
        sig: Sig,
        dest: Reg,
        uses: CallArgList,
        defs: CallRetList,
        try_call_info: Option<TryCallInfo>,
    ) -> BoxCallIndInfo {
        let stack_ret_space = self.lower_ctx.sigs()[sig].sized_stack_ret_space();
        let stack_arg_space = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_outgoing_args_size(stack_ret_space + stack_arg_space);

        Box::new(
            self.lower_ctx
                .gen_call_info(sig, dest, uses, defs, try_call_info, false),
        )
    }

    fn gen_return_call_info(
        &mut self,
        sig: Sig,
        dest: ExternalName,
        uses: CallArgList,
    ) -> BoxReturnCallInfo {
        let new_stack_arg_size = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_tail_args_size(new_stack_arg_size);

        let key =
            AArch64MachineDeps::select_api_key(&self.backend.isa_flags, isa::CallConv::Tail, true);

        Box::new(ReturnCallInfo {
            dest,
            uses,
            key,
            sign_return_address_all: self.backend.isa_flags.sign_return_address_all(),
            new_stack_arg_size,
        })
    }

    fn gen_return_call_ind_info(
        &mut self,
        sig: Sig,
        dest: Reg,
        uses: CallArgList,
    ) -> BoxReturnCallIndInfo {
        let new_stack_arg_size = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_tail_args_size(new_stack_arg_size);

        let key =
            AArch64MachineDeps::select_api_key(&self.backend.isa_flags, isa::CallConv::Tail, true);

        Box::new(ReturnCallInfo {
            dest,
            uses,
            key,
            sign_return_address_all: self.backend.isa_flags.sign_return_address_all(),
            new_stack_arg_size,
        })
    }

    fn sign_return_address_disabled(&mut self) -> Option<()> {
        if self.backend.isa_flags.sign_return_address() {
            None
        } else {
            Some(())
        }
    }

    fn use_lse(&mut self, _: Inst) -> Option<()> {
        if self.backend.isa_flags.has_lse() {
            Some(())
        } else {
            None
        }
    }

    fn use_dotprod(&mut self, _: Inst) -> Option<()> {
        if self.backend.isa_flags.has_dotprod() {
            Some(())
        } else {
            None
        }
    }

    fn use_i8mm(&mut self, _: Inst) -> Option<()> {
        if self.backend.isa_flags.has_i8mm() {
            Some(())
        } else {
            None
        }
    }

    fn use_fp16(&mut self) -> bool {
        self.backend.isa_flags.has_fp16()
    }

    fn use_csdb(&mut self) -> bool {
        self.backend.isa_flags.use_csdb()
    }

    fn move_wide_const_from_u64(&mut self, ty: Type, n: u64) -> Option<MoveWideConst> {
        let bits = ty.bits();
        let n = if bits < 64 {
            n & !(u64::MAX << bits)
        } else {
            n
        };
        MoveWideConst::maybe_from_u64(n)
    }

    fn move_wide_const_from_inverted_u64(&mut self, ty: Type, n: u64) -> Option<MoveWideConst> {
        self.move_wide_const_from_u64(ty, !n)
    }

    fn imm_logic_from_u64(&mut self, ty: Type, n: u64) -> Option<ImmLogic> {
        ImmLogic::maybe_from_u64(n, ty)
    }

    fn imm_size_from_type(&mut self, ty: Type) -> Option<u16> {
        match ty {
            I32 => Some(32),
            I64 => Some(64),
            _ => None,
        }
    }

    fn imm_logic_from_imm64(&mut self, ty: Type, n: Imm64) -> Option<ImmLogic> {
        let ty = if ty.bits() < 32 { I32 } else { ty };
        self.imm_logic_from_u64(ty, n.bits() as u64)
    }

    fn imm12_from_u64(&mut self, n: u64) -> Option<Imm12> {
        Imm12::maybe_from_u64(n)
    }

    fn imm_shift_from_u8(&mut self, n: u8) -> ImmShift {
        ImmShift::maybe_from_u64(n.into()).unwrap()
    }

    fn lshr_from_u64(&mut self, ty: Type, n: u64) -> Option<ShiftOpAndAmt> {
        let shiftimm = ShiftOpShiftImm::maybe_from_shift(n)?;
        if let Ok(bits) = u8::try_from(ty_bits(ty)) {
            let shiftimm = shiftimm.mask(bits);
            Some(ShiftOpAndAmt::new(ShiftOp::LSR, shiftimm))
        } else {
            None
        }
    }

    fn lshl_from_imm64(&mut self, ty: Type, n: Imm64) -> Option<ShiftOpAndAmt> {
        self.lshl_from_u64(ty, n.bits() as u64)
    }

    fn lshl_from_u64(&mut self, ty: Type, n: u64) -> Option<ShiftOpAndAmt> {
        let shiftimm = ShiftOpShiftImm::maybe_from_shift(n)?;
        let shiftee_bits = ty_bits(ty);
        if shiftee_bits <= u8::MAX as usize {
            let shiftimm = shiftimm.mask(shiftee_bits as u8);
            Some(ShiftOpAndAmt::new(ShiftOp::LSL, shiftimm))
        } else {
            None
        }
    }

    fn ashr_from_u64(&mut self, ty: Type, n: u64) -> Option<ShiftOpAndAmt> {
        let shiftimm = ShiftOpShiftImm::maybe_from_shift(n)?;
        let shiftee_bits = ty_bits(ty);
        if shiftee_bits <= u8::MAX as usize {
            let shiftimm = shiftimm.mask(shiftee_bits as u8);
            Some(ShiftOpAndAmt::new(ShiftOp::ASR, shiftimm))
        } else {
            None
        }
    }

    // PLIRON: `imul` by `2^n + 1` / `2^n - 1` strength reduction — the shift
    // amount `n` when `c` has that form and the shift fits the operand's
    // register width (`bits < 32` ops run as 32-bit regs).
    fn imul_pow2p1_shamt(&mut self, ty: Type, c: u64) -> Option<u8> {
        let bits = ty_bits(ty);
        let c = if bits < 64 { c & ((1u64 << bits) - 1) } else { c };
        let n = c.checked_sub(1)?.trailing_zeros();
        (n > 0 && n < bits.max(32) as u32 && (1u64 << n) + 1 == c).then_some(n as u8)
    }

    fn imul_pow2m1_shamt(&mut self, ty: Type, c: u64) -> Option<u8> {
        let bits = ty_bits(ty);
        let c = if bits < 64 { c & ((1u64 << bits) - 1) } else { c };
        let n = c.checked_add(1)?.trailing_zeros();
        (n > 1 && n < bits.max(32) as u32 && (1u64 << n).checked_sub(1) == Some(c))
            .then_some(n as u8)
    }

    // PLIRON: ROR operand for the logical (shifted register) fold below.
    fn rotr_from_u64(&mut self, ty: Type, n: u64) -> Option<ShiftOpAndAmt> {
        let shiftimm = ShiftOpShiftImm::maybe_from_shift(n)?;
        let shiftee_bits = ty_bits(ty);
        if shiftee_bits <= u8::MAX as usize {
            let shiftimm = shiftimm.mask(shiftee_bits as u8);
            Some(ShiftOpAndAmt::new(ShiftOp::ROR, shiftimm))
        } else {
            None
        }
    }

    fn integral_ty(&mut self, ty: Type) -> Option<Type> {
        match ty {
            I8 | I16 | I32 | I64 => Some(ty),
            _ => None,
        }
    }

    fn is_zero_simm9(&mut self, imm: &SImm9) -> Option<()> {
        if imm.value() == 0 { Some(()) } else { None }
    }

    fn is_zero_uimm12(&mut self, imm: &UImm12Scaled) -> Option<()> {
        if imm.value() == 0 { Some(()) } else { None }
    }

    /// This is target-word-size dependent.  And it excludes booleans and reftypes.
    fn valid_atomic_transaction(&mut self, ty: Type) -> Option<Type> {
        match ty {
            I8 | I16 | I32 | I64 => Some(ty),
            _ => None,
        }
    }

    /// This is the fallback case for loading a 64-bit integral constant into a
    /// register.
    ///
    /// The logic here is nontrivial enough that it's not really worth porting
    /// this over to ISLE.
    fn load_constant_full(
        &mut self,
        ty: Type,
        extend: &ImmExtend,
        extend_to: &OperandSize,
        value: u64,
    ) -> Reg {
        let bits = ty.bits();

        let value = match (extend_to, *extend) {
            (OperandSize::Size32, ImmExtend::Sign) if bits < 32 => {
                let shift = 32 - bits;
                let value = value as i32;

                // we cast first to a u32 and then to a u64, to ensure that we are representing a
                // i32 in a u64, and not a i64. This is important, otherwise value will not fit in
                // 32 bits
                ((value << shift) >> shift) as u32 as u64
            }
            (OperandSize::Size32, ImmExtend::Zero) if bits < 32 => {
                value & !((u32::MAX as u64) << bits)
            }
            (OperandSize::Size64, ImmExtend::Sign) if bits < 64 => {
                let shift = 64 - bits;
                let value = value as i64;

                ((value << shift) >> shift) as u64
            }
            (OperandSize::Size64, ImmExtend::Zero) if bits < 64 => value & !(u64::MAX << bits),
            _ => value,
        };

        // Divide the value into 16-bit slices that we can manipulate using
        // `movz`, `movn`, and `movk`.
        fn get(value: u64, shift: u8) -> u16 {
            (value >> (shift * 16)) as u16
        }
        fn replace(mut old: u64, new: u16, shift: u8) -> u64 {
            let offset = shift * 16;
            old &= !(0xffff << offset);
            old |= u64::from(new) << offset;
            old
        }

        // The 32-bit versions of `movz`/`movn`/`movk` will clear the upper 32
        // bits, so if that's the outcome we want we might as well use them. For
        // simplicity and ease of reading the disassembly, we use the same size
        // for all instructions in the sequence.
        let size = if value >> 32 == 0 {
            OperandSize::Size32
        } else {
            OperandSize::Size64
        };

        // The `movz` instruction initially sets all bits to zero, while `movn`
        // initially sets all bits to one. A good choice of initial value can
        // reduce the number of `movk` instructions we need afterward, so we
        // check both variants to determine which is closest to the constant
        // we actually wanted. In case they're equally good, we prefer `movz`
        // because the assembly listings are generally harder to read when the
        // operands are negated.
        let (mut running_value, op, first) =
            [(MoveWideOp::MovZ, 0), (MoveWideOp::MovN, size.max_value())]
                .into_iter()
                .map(|(op, base)| {
                    // Both `movz` and `movn` can overwrite one slice after setting
                    // the initial value; we get to pick which one. 32-bit variants
                    // can only modify the lower two slices.
                    let first = (0..(size.bits() / 16))
                        // Pick one slice that's different from the initial value
                        .find(|&i| get(base ^ value, i) != 0)
                        // If none are different, we still have to pick one
                        .unwrap_or(0);
                    // Compute the value we'll get from this `movz`/`movn`
                    (replace(base, get(value, first), first), op, first)
                })
                // Count how many `movk` instructions we'll need.
                .min_by_key(|(base, ..)| (0..4).filter(|&i| get(base ^ value, i) != 0).count())
                // `variants` isn't empty so `min_by_key` always returns something.
                .unwrap();

        // Build the initial instruction we chose above, putting the result
        // into a new temporary virtual register. Note that the encoding for the
        // immediate operand is bitwise-inverted for `movn`.
        let mut rd = self.temp_writable_reg(I64);
        self.lower_ctx.emit(MInst::MovWide {
            op,
            rd,
            imm: MoveWideConst {
                bits: match op {
                    MoveWideOp::MovZ => get(value, first),
                    MoveWideOp::MovN => !get(value, first),
                },
                shift: first,
            },
            size,
        });

        // Emit a `movk` instruction for each remaining slice of the desired
        // constant that does not match the initial value constructed above.
        for shift in (first + 1)..(size.bits() / 16) {
            let bits = get(value, shift);
            if bits != get(running_value, shift) {
                let rn = rd.to_reg();
                rd = self.temp_writable_reg(I64);
                self.lower_ctx.emit(MInst::MovK {
                    rd,
                    rn,
                    imm: MoveWideConst { bits, shift },
                    size,
                });
                running_value = replace(running_value, bits, shift);
            }
        }

        debug_assert_eq!(value, running_value);
        return rd.to_reg();
    }

    fn zero_reg(&mut self) -> Reg {
        zero_reg()
    }

    fn stack_reg(&mut self) -> Reg {
        stack_reg()
    }

    fn fp_reg(&mut self) -> Reg {
        fp_reg()
    }

    fn writable_link_reg(&mut self) -> WritableReg {
        writable_link_reg()
    }

    fn extended_value_from_value(&mut self, val: Value) -> Option<ExtendedValue> {
        let (val, extend) = super::get_as_extended_value(self.lower_ctx, val)?;
        Some(ExtendedValue { val, extend })
    }

    fn put_extended_in_reg(&mut self, reg: &ExtendedValue) -> Reg {
        self.put_in_reg(reg.val)
    }

    fn get_extended_op(&mut self, reg: &ExtendedValue) -> ExtendOp {
        reg.extend
    }

    fn emit(&mut self, inst: &MInst) -> Unit {
        self.lower_ctx.emit(inst.clone());
    }

    fn cond_br_zero(&mut self, reg: Reg, size: &OperandSize) -> CondBrKind {
        CondBrKind::Zero(reg, *size)
    }

    fn cond_br_not_zero(&mut self, reg: Reg, size: &OperandSize) -> CondBrKind {
        CondBrKind::NotZero(reg, *size)
    }

    fn cond_br_cond(&mut self, cond: &Cond) -> CondBrKind {
        CondBrKind::Cond(*cond)
    }

    fn nzcv(&mut self, n: bool, z: bool, c: bool, v: bool) -> NZCV {
        NZCV::new(n, z, c, v)
    }

    fn u8_into_uimm5(&mut self, x: u8) -> UImm5 {
        UImm5::maybe_from_u8(x).unwrap()
    }

    fn u8_into_imm12(&mut self, x: u8) -> Imm12 {
        Imm12::maybe_from_u64(x.into()).unwrap()
    }

    fn writable_zero_reg(&mut self) -> WritableReg {
        writable_zero_reg()
    }

    fn shift_mask(&mut self, ty: Type) -> ImmLogic {
        debug_assert!(ty.lane_bits().is_power_of_two());

        let mask = (ty.lane_bits() - 1) as u64;
        ImmLogic::maybe_from_u64(mask, I32).unwrap()
    }

    fn imm_shift_from_imm64(&mut self, ty: Type, val: Imm64) -> Option<ImmShift> {
        let imm_value = (val.bits() as u64) & ((ty.bits() - 1) as u64);
        ImmShift::maybe_from_u64(imm_value)
    }

    fn u64_into_imm_logic(&mut self, ty: Type, val: u64) -> ImmLogic {
        ImmLogic::maybe_from_u64(val, ty).unwrap()
    }

    fn negate_imm_shift(&mut self, ty: Type, mut imm: ImmShift) -> ImmShift {
        let size = u8::try_from(ty.bits()).unwrap();
        imm.imm = size.wrapping_sub(imm.value());
        imm.imm &= size - 1;
        imm
    }

    fn rotr_mask(&mut self, ty: Type) -> ImmLogic {
        ImmLogic::maybe_from_u64((ty.bits() - 1) as u64, I32).unwrap()
    }

    fn rotr_opposite_amount(&mut self, ty: Type, val: ImmShift) -> ImmShift {
        let amount = val.value() & u8::try_from(ty.bits() - 1).unwrap();
        ImmShift::maybe_from_u64(u64::from(ty.bits()) - u64::from(amount)).unwrap()
    }

    fn icmp_zero_cond(&mut self, cond: &IntCC) -> Option<IntCC> {
        match cond {
            &IntCC::Equal
            | &IntCC::SignedGreaterThanOrEqual
            | &IntCC::SignedGreaterThan
            | &IntCC::SignedLessThanOrEqual
            | &IntCC::SignedLessThan => Some(*cond),
            _ => None,
        }
    }

    fn fcmp_zero_cond(&mut self, cond: &FloatCC) -> Option<FloatCC> {
        match cond {
            &FloatCC::Equal
            | &FloatCC::GreaterThanOrEqual
            | &FloatCC::GreaterThan
            | &FloatCC::LessThanOrEqual
            | &FloatCC::LessThan => Some(*cond),
            _ => None,
        }
    }

    fn fcmp_zero_cond_not_eq(&mut self, cond: &FloatCC) -> Option<FloatCC> {
        match cond {
            &FloatCC::NotEqual => Some(FloatCC::NotEqual),
            _ => None,
        }
    }

    fn icmp_zero_cond_not_eq(&mut self, cond: &IntCC) -> Option<IntCC> {
        match cond {
            &IntCC::NotEqual => Some(IntCC::NotEqual),
            _ => None,
        }
    }

    fn float_cc_cmp_zero_to_vec_misc_op(&mut self, cond: &FloatCC) -> VecMisc2 {
        match cond {
            &FloatCC::Equal => VecMisc2::Fcmeq0,
            &FloatCC::GreaterThanOrEqual => VecMisc2::Fcmge0,
            &FloatCC::LessThanOrEqual => VecMisc2::Fcmle0,
            &FloatCC::GreaterThan => VecMisc2::Fcmgt0,
            &FloatCC::LessThan => VecMisc2::Fcmlt0,
            _ => panic!(),
        }
    }

    fn int_cc_cmp_zero_to_vec_misc_op(&mut self, cond: &IntCC) -> VecMisc2 {
        match cond {
            &IntCC::Equal => VecMisc2::Cmeq0,
            &IntCC::SignedGreaterThanOrEqual => VecMisc2::Cmge0,
            &IntCC::SignedLessThanOrEqual => VecMisc2::Cmle0,
            &IntCC::SignedGreaterThan => VecMisc2::Cmgt0,
            &IntCC::SignedLessThan => VecMisc2::Cmlt0,
            _ => panic!(),
        }
    }

    fn float_cc_cmp_zero_to_vec_misc_op_swap(&mut self, cond: &FloatCC) -> VecMisc2 {
        match cond {
            &FloatCC::Equal => VecMisc2::Fcmeq0,
            &FloatCC::GreaterThanOrEqual => VecMisc2::Fcmle0,
            &FloatCC::LessThanOrEqual => VecMisc2::Fcmge0,
            &FloatCC::GreaterThan => VecMisc2::Fcmlt0,
            &FloatCC::LessThan => VecMisc2::Fcmgt0,
            _ => panic!(),
        }
    }

    fn int_cc_cmp_zero_to_vec_misc_op_swap(&mut self, cond: &IntCC) -> VecMisc2 {
        match cond {
            &IntCC::Equal => VecMisc2::Cmeq0,
            &IntCC::SignedGreaterThanOrEqual => VecMisc2::Cmle0,
            &IntCC::SignedLessThanOrEqual => VecMisc2::Cmge0,
            &IntCC::SignedGreaterThan => VecMisc2::Cmlt0,
            &IntCC::SignedLessThan => VecMisc2::Cmgt0,
            _ => panic!(),
        }
    }

    fn cond_code(&mut self, cc: &condcodes::IntCC) -> Cond {
        lower_condcode(*cc)
    }

    fn invert_cond(&mut self, cond: &Cond) -> Cond {
        (*cond).invert()
    }
    fn preg_sp(&mut self) -> PReg {
        super::regs::stack_reg().to_real_reg().unwrap().into()
    }

    fn preg_fp(&mut self) -> PReg {
        super::regs::fp_reg().to_real_reg().unwrap().into()
    }

    fn preg_link(&mut self) -> PReg {
        super::regs::link_reg().to_real_reg().unwrap().into()
    }

    fn preg_pinned(&mut self) -> PReg {
        super::regs::pinned_reg().to_real_reg().unwrap().into()
    }

    fn branch_target(&mut self, label: MachLabel) -> BranchTarget {
        BranchTarget::Label(label)
    }

    fn targets_jt_space(&mut self, elements: &BoxVecMachLabel) -> CodeOffset {
        // calculate the number of bytes needed for the jumptable sequence:
        // 4 bytes per instruction, with 8 instructions base + the size of
        // the jumptable more.
        (4 * (8 + elements.len())).try_into().unwrap()
    }

    fn min_fp_value(&mut self, signed: bool, in_bits: u8, out_bits: u8) -> Reg {
        if in_bits == 32 {
            // From float32.
            let min = match (signed, out_bits) {
                (true, 8) => i8::MIN as f32 - 1.,
                (true, 16) => i16::MIN as f32 - 1.,
                (true, 32) => i32::MIN as f32, // I32_MIN - 1 isn't precisely representable as a f32.
                (true, 64) => i64::MIN as f32, // I64_MIN - 1 isn't precisely representable as a f32.

                (false, _) => -1.,
                _ => unimplemented!(
                    "unexpected {} output size of {} bits for 32-bit input",
                    if signed { "signed" } else { "unsigned" },
                    out_bits
                ),
            };

            generated_code::constructor_constant_f32(self, min.to_bits())
        } else if in_bits == 64 {
            // From float64.
            let min = match (signed, out_bits) {
                (true, 8) => i8::MIN as f64 - 1.,
                (true, 16) => i16::MIN as f64 - 1.,
                (true, 32) => i32::MIN as f64 - 1.,
                (true, 64) => i64::MIN as f64,

                (false, _) => -1.,
                _ => unimplemented!(
                    "unexpected {} output size of {} bits for 64-bit input",
                    if signed { "signed" } else { "unsigned" },
                    out_bits
                ),
            };

            generated_code::constructor_constant_f64(self, min.to_bits())
        } else {
            unimplemented!(
                "unexpected input size for min_fp_value: {} (signed: {}, output size: {})",
                in_bits,
                signed,
                out_bits
            );
        }
    }

    fn max_fp_value(&mut self, signed: bool, in_bits: u8, out_bits: u8) -> Reg {
        if in_bits == 32 {
            // From float32.
            let max = match (signed, out_bits) {
                (true, 8) => i8::MAX as f32 + 1.,
                (true, 16) => i16::MAX as f32 + 1.,
                (true, 32) => (i32::MAX as u64 + 1) as f32,
                (true, 64) => (i64::MAX as u64 + 1) as f32,

                (false, 8) => u8::MAX as f32 + 1.,
                (false, 16) => u16::MAX as f32 + 1.,
                (false, 32) => (u32::MAX as u64 + 1) as f32,
                (false, 64) => (u64::MAX as u128 + 1) as f32,
                _ => unimplemented!(
                    "unexpected {} output size of {} bits for 32-bit input",
                    if signed { "signed" } else { "unsigned" },
                    out_bits
                ),
            };

            generated_code::constructor_constant_f32(self, max.to_bits())
        } else if in_bits == 64 {
            // From float64.
            let max = match (signed, out_bits) {
                (true, 8) => i8::MAX as f64 + 1.,
                (true, 16) => i16::MAX as f64 + 1.,
                (true, 32) => i32::MAX as f64 + 1.,
                (true, 64) => (i64::MAX as u64 + 1) as f64,

                (false, 8) => u8::MAX as f64 + 1.,
                (false, 16) => u16::MAX as f64 + 1.,
                (false, 32) => u32::MAX as f64 + 1.,
                (false, 64) => (u64::MAX as u128 + 1) as f64,
                _ => unimplemented!(
                    "unexpected {} output size of {} bits for 64-bit input",
                    if signed { "signed" } else { "unsigned" },
                    out_bits
                ),
            };

            generated_code::constructor_constant_f64(self, max.to_bits())
        } else {
            unimplemented!(
                "unexpected input size for max_fp_value: {} (signed: {}, output size: {})",
                in_bits,
                signed,
                out_bits
            );
        }
    }

    fn fpu_op_ri_ushr(&mut self, ty_bits: u8, shift: u8) -> FPUOpRI {
        if ty_bits == 32 {
            FPUOpRI::UShr32(FPURightShiftImm::maybe_from_u8(shift, ty_bits).unwrap())
        } else if ty_bits == 64 {
            FPUOpRI::UShr64(FPURightShiftImm::maybe_from_u8(shift, ty_bits).unwrap())
        } else {
            unimplemented!(
                "unexpected input size for fpu_op_ri_ushr: {} (shift: {})",
                ty_bits,
                shift
            );
        }
    }

    fn fpu_op_ri_sli(&mut self, ty_bits: u8, shift: u8) -> FPUOpRIMod {
        if ty_bits == 32 {
            FPUOpRIMod::Sli32(FPULeftShiftImm::maybe_from_u8(shift, ty_bits).unwrap())
        } else if ty_bits == 64 {
            FPUOpRIMod::Sli64(FPULeftShiftImm::maybe_from_u8(shift, ty_bits).unwrap())
        } else {
            unimplemented!(
                "unexpected input size for fpu_op_ri_sli: {} (shift: {})",
                ty_bits,
                shift
            );
        }
    }

    fn vec_extract_imm4_from_immediate(&mut self, imm: Immediate) -> Option<u8> {
        let bytes = self.lower_ctx.get_immediate_data(imm).as_slice();

        if bytes.array_windows().all(|[a, b]| *a + 1 == *b) && bytes[0] < 16 {
            Some(bytes[0])
        } else {
            None
        }
    }

    fn vec_rotate_imm4_from_immediate(&mut self, imm: Immediate) -> Option<u8> {
        let bytes = self.lower_ctx.get_immediate_data(imm).as_slice();
        let n = bytes[0];
        if n < 16
            && bytes
                .iter()
                .enumerate()
                .all(|(i, &b)| b == (n.wrapping_add(i as u8)) % 16)
        {
            Some(n)
        } else {
            None
        }
    }

    fn shuffle_dup8_from_imm(&mut self, imm: Immediate) -> Option<u8> {
        let bytes = self.lower_ctx.get_immediate_data(imm).as_slice();
        if bytes.iter().all(|b| *b == bytes[0]) && bytes[0] < 16 {
            Some(bytes[0])
        } else {
            None
        }
    }
    fn shuffle_dup16_from_imm(&mut self, imm: Immediate) -> Option<u8> {
        let (a, b, c, d, e, f, g, h) = self.shuffle16_from_imm(imm)?;
        if a == b && b == c && c == d && d == e && e == f && f == g && g == h && a < 8 {
            Some(a)
        } else {
            None
        }
    }
    fn shuffle_dup32_from_imm(&mut self, imm: Immediate) -> Option<u8> {
        let (a, b, c, d) = self.shuffle32_from_imm(imm)?;
        if a == b && b == c && c == d && a < 4 {
            Some(a)
        } else {
            None
        }
    }
    fn shuffle_dup64_from_imm(&mut self, imm: Immediate) -> Option<u8> {
        let (a, b) = self.shuffle64_from_imm(imm)?;
        if a == b && a < 2 { Some(a) } else { None }
    }

    fn asimd_mov_mod_imm_zero(&mut self, size: &ScalarSize) -> ASIMDMovModImm {
        ASIMDMovModImm::zero(*size)
    }

    fn asimd_mov_mod_imm_from_u64(
        &mut self,
        val: u64,
        size: &ScalarSize,
    ) -> Option<ASIMDMovModImm> {
        ASIMDMovModImm::maybe_from_u64(val, *size)
    }

    fn asimd_fp_mod_imm_from_u64(&mut self, val: u64, size: &ScalarSize) -> Option<ASIMDFPModImm> {
        ASIMDFPModImm::maybe_from_u64(val, *size)
    }

    fn u64_low32_bits_unset(&mut self, val: u64) -> Option<u64> {
        if val & 0xffffffff == 0 {
            Some(val)
        } else {
            None
        }
    }

    fn shift_masked_imm(&mut self, ty: Type, imm: u64) -> u8 {
        (imm as u8) & ((ty.lane_bits() - 1) as u8)
    }

    fn simm7_scaled_from_i64(&mut self, val: i64, ty: Type) -> Option<SImm7Scaled> {
        SImm7Scaled::maybe_from_i64(val, ty)
    }

    fn simm9_from_i64(&mut self, val: i64) -> Option<SImm9> {
        SImm9::maybe_from_i64(val)
    }

    /// PLIRON: see `amode_fold_ok` in `inst.isle`. `PLIRON_BIGOFF=0` disables.
    fn amode_fold_ok(&mut self, ty: Type, off: i32) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_BIGOFF").as_deref() != Ok("0")) {
            return true;
        }
        let off = i64::from(off);
        SImm9::maybe_from_i64(off).is_some() || UImm12Scaled::maybe_from_i64(off, ty).is_some()
    }

    fn uimm12_scaled_from_i64(&mut self, val: i64, ty: Type) -> Option<UImm12Scaled> {
        UImm12Scaled::maybe_from_i64(val, ty)
    }

    /// Like `uimm12_scaled_from_i64`, but rejects a zero value so `base + index
    /// + 0` keeps its single-instruction `RegExtended` amode.
    fn uimm12_scaled_nonzero_from_i64(&mut self, val: i64, ty: Type) -> Option<UImm12Scaled> {
        if val == 0 {
            return None;
        }
        UImm12Scaled::maybe_from_i64(val, ty)
    }

    /// PLIRON: may a multi-use `iadd` load/store base stay whole in the amode
    /// instead of being decomposed into reg+reg? Free when the sum is needed
    /// as a value anyway; bounded for small all-address fanout under a
    /// block-pressure cutoff (see `Lower::amode_shareable`).
    /// `PLIRON_AMODESHARE=0` disables.
    fn amode_share_ok(&mut self, val: Value) -> bool {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var("PLIRON_AMODESHARE").as_deref() != Ok("0"))
            && self.lower_ctx.amode_shareable(val)
    }

    /// PLIRON: `PLIRON_CARRYCINC` (default on), see `lower.isle`.
    fn carry_cinc_on(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_CARRYCINC").as_deref() != Ok("0"))
    }

    /// PLIRON (`PLIRON_CINC`, opt-in): `x + (a cc b ? 1 : 0)` —
    /// `cmp a, b; csinc x, x, !cc` (LLVM's `cinc`) instead of
    /// `cmp; cset; add`. Declines to the plain `add` when off, when
    /// the `icmp` has other uses (its `cset` still materializes
    /// there, so the fused `cmp` is a second compare for nothing),
    /// or when the compare didn't produce a single flag-setting
    /// inst.
    fn iadd_cinc_for(
        &mut self,
        ty: Type,
        x: Value,
        y: Value,
        cc: &IntCC,
        a: Value,
        b: Value,
    ) -> InstOutput {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ON.get_or_init(|| std::env::var("PLIRON_CINC").as_deref() == Ok("1"))
            && !self.lower_ctx.value_has_multiple_uses(y)
            && let CondResult::Cond {
                flags: ProducesFlags::ProducesFlagsSideEffect { inst },
                cond,
            } = generated_code::constructor_emit_icmp(self, cc, a, b)
        {
            self.lower_ctx.emit(inst);
            let rx = self.put_in_reg(x);
            let dst = self.lower_ctx.alloc_tmp(I64).only_reg().unwrap();
            // `csinc c, x, x` is `c ? x : x+1`, so inverting `cond`
            // gives `cc ? x+1 : x`.
            self.lower_ctx.emit(MInst::CSInc {
                rd: dst,
                cond: cond.invert(),
                rn: rx,
                rm: rx,
            });
            return generated_code::constructor_output_reg(self, dst.to_reg());
        }
        let rx = self.put_in_reg(x);
        let ry = self.put_in_reg(y);
        let r = generated_code::constructor_add(self, ty, rx, ry);
        generated_code::constructor_output_reg(self, r)
    }

    /// PLIRON: may a constant rotate be folded into a logic op's shifted
    /// operand? Only when it has a single use (see `rot_fold_ok` in
    /// `lower.isle`); `PLIRON_RORFOLD_MULTI=1` folds multi-use rotates too.
    fn rot_fold_ok(&mut self, val: Value) -> bool {
        static MULTI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *MULTI.get_or_init(|| std::env::var("PLIRON_RORFOLD_MULTI").as_deref() == Ok("1"))
            || !self.lower_ctx.value_has_multiple_uses(val)
    }

    /// PLIRON: `PLIRON_TBNZ_IREDUCE=0` off (default on), see `lower.isle`.
    fn tbnz_ireduce_on(&mut self, _bit: u64) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_TBNZ_IREDUCE").as_deref() != Ok("0"))
    }

    fn narrowext_on(&mut self, bit: u64) -> bool {
        narrowext_part(bit)
    }

    fn narrowext_clean(&mut self, val: Value) -> bool {
        let mut budget = 16;
        narrowext_part(1) && narrow_uext_clean(self.lower_ctx.dfg(), val, &mut budget)
    }

    /// PLIRON (`PLIRON_NARROWEXT` bit 64): `val`'s register is already zero
    /// above `bits`, so a narrow `ireduce` of it needs no `uxtb`/`uxth`.
    fn wide_clean(&mut self, bits: u64, val: Value) -> bool {
        let mut budget = 16;
        let Ok(bits) = u32::try_from(bits) else {
            return false;
        };
        narrowext_part(64) && wide_uext_clean(self.lower_ctx.dfg(), val, bits, &mut budget)
    }

    /// PLIRON (`PLIRON_CMN`, default on): `cmp x, -c` -> `cmn x, c`.
    fn cmn_on(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_CMN").as_deref() != Ok("0"))
    }

    /// PLIRON (`PLIRON_CCMP`, default on): `band`/`bor` of two integer
    /// compares consumed as a condition → `cmp; ccmp` flag chain
    /// (`a && b` / `a || b`). `mode`: bit0 inverts the primary guard,
    /// bit1 makes the constant NZCV satisfy (not falsify) the secondary
    /// cond — 0 = `A && B`, 3 = `A || B`. Declines to `is_nonzero` of
    /// the whole combination value, the generic catch-all's result.
    fn ccmp_band_cond(
        &mut self,
        mode: u8,
        val: Value,
        cc_a: &IntCC,
        x: Value,
        y: Value,
        cc_b: &IntCC,
        u: Value,
        v: Value,
    ) -> CondResult {
        if ccmp_on() {
            if let Some(cr) = ccmp_pair(self, mode, cc_a, x, y, cc_b, u, v) {
                return cr;
            }
        }
        generated_code::constructor_is_nonzero(self, val)
    }

    /// PLIRON: `select` of a compare against a constant arm is a bool op
    /// on the select's own condition — `sel(A,B,0)` = 0 `A&&B`,
    /// `sel(A,0,B)` = 1 `!A&&B`, `sel(A,B,1)` = 2 `!A||B`,
    /// `sel(A,1,B)` = 3 `A||B`. Same `is_nonzero` decline as above.
    fn ccmp_sel_cond(
        &mut self,
        mode: u8,
        val: Value,
        cond: Value,
        cc_b: &IntCC,
        u: Value,
        v: Value,
    ) -> CondResult {
        if ccmp_on() {
            let cr = generated_code::constructor_is_nonzero_cmp(self, cond);
            if let Some(cr) = ccmp_chain(self, mode, &cr, cc_b, u, v) {
                return cr;
            }
        }
        generated_code::constructor_is_nonzero(self, val)
    }

    /// PLIRON: `band`/`bor` of two compares produced as a *value* —
    /// fused is `cmp; ccmp; cset` (3) vs `cmp; cset; cmp; cset; op` (5).
    /// Declines to the base `and`/`orr` on the bound arm values.
    fn ccmp_band_out(
        &mut self,
        mode: u8,
        ty: Type,
        a0: Value,
        cc_a: &IntCC,
        x: Value,
        y: Value,
        a1: Value,
        cc_b: &IntCC,
        u: Value,
        v: Value,
    ) -> InstOutput {
        if ccmp_on() {
            if let Some(cr) = ccmp_pair(self, mode, cc_a, x, y, cc_b, u, v) {
                let reg = generated_code::constructor_lower_cond_result_bool(self, &cr);
                return generated_code::constructor_output_reg(self, reg);
            }
        }
        let op = if mode & 2 != 0 {
            ALUOp::Orr
        } else {
            ALUOp::And
        };
        let reg =
            generated_code::constructor_alu_rs_imm_logic_commutative(self, &op, ty, a0, a1);
        generated_code::constructor_output_reg(self, reg)
    }

    /// PLIRON: `select` of a compare against a constant arm produced as
    /// a value — same fusion, declining to `lower_select`.
    fn ccmp_sel_out(
        &mut self,
        mode: u8,
        ty: Type,
        cond: Value,
        rn: Value,
        rm: Value,
        cc_b: &IntCC,
        u: Value,
        v: Value,
    ) -> InstOutput {
        let cr = generated_code::constructor_is_nonzero_cmp(self, cond);
        if ccmp_on() {
            if let Some(cr) = ccmp_chain(self, mode, &cr, cc_b, u, v) {
                let reg = generated_code::constructor_lower_cond_result_bool(self, &cr);
                return generated_code::constructor_output_reg(self, reg);
            }
        }
        let regs = generated_code::constructor_lower_select(self, ty, &cr, rn, rm);
        self.output(regs)
    }

    /// PLIRON bitfield fusion (`PLIRON_BFUSE`, default on): `(x>>k)&m`,
    /// `(x&m)<<k`, `y|((x&m)<<k)`, `y|((x<<k)&m')` → `ubfx`/`ubfiz`/`bfi`.
    /// The externs compute UBFM immediates; `bf_disjoint*` proves `y`'s
    /// field bits zero with a bounded known-bits walk.
    fn bfuse_on(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_BFUSE").as_deref() != Ok("0"))
    }

    /// `0 < k < bits` for UBFM.
    fn bf_imm_u8(&mut self, k: u64) -> Option<u8> {
        (k >= 1 && k < 64).then_some(k as u8)
    }

    /// `(x>>k)&m` → `ubfx`: `m = (1<<w)-1`, `k+w <= bits`; imms = k+w-1.
    fn bf_ubfx_imms(&mut self, ty: Type, m: u64, k: u64) -> Option<u8> {
        let bits = ty.bits();
        let w = bf_low_w(bits, m)?;
        (k + w as u64 <= bits as u64).then_some((k + w as u64 - 1) as u8)
    }

    /// `(x<<k)&m` → `ubfiz`: `m = ((1<<w)-1)<<k`; imms = k+w-1.
    fn bf_field_imms(&mut self, ty: Type, m: u64, k: u64) -> Option<u8> {
        let bits = ty.bits();
        let w = bf_field_w(bits, m, k)?;
        Some((k + w as u64 - 1) as u8)
    }

    /// `ubfiz` with lsb `k`: immr = bits - k.
    fn bf_ubfiz_immr(&mut self, ty: Type, k: u64) -> Option<u8> {
        let bits = ty.bits();
        (k >= 1 && k < bits as u64).then_some((bits as u64 - k) as u8)
    }

    /// `(x&m)<<k` → `ubfiz`: imms = w-1.
    fn bf_ubfiz_imms(&mut self, ty: Type, m: u64, k: u64) -> Option<u8> {
        let w = bf_low_w(ty.bits(), m)?;
        (k + w as u64 <= ty.bits() as u64).then_some((w - 1) as u8)
    }

    /// `(x<<k)&m` as `ubfiz` mask form: imms = w-1.
    fn bf_field_ubfiz_imms(&mut self, ty: Type, m: u64, k: u64) -> Option<u8> {
        let w = bf_field_w(ty.bits(), m, k)?;
        Some((w - 1) as u8)
    }

    /// `bfi`: `y`'s bits inside `((1<<w)-1)<<k` all provably zero.
    fn bf_disjoint_lowfield(&mut self, y: Value, m: u64, k: u64) -> bool {
        let mut budget = 16;
        let z = bf_known_zero(self.lower_ctx.dfg(), y, &mut budget);
        k < 64 && (m << k) & z == (m << k)
    }

    /// `bfi`: `y`'s bits inside mask `m` all provably zero.
    fn bf_disjoint_mask(&mut self, y: Value, m: u64) -> bool {
        let mut budget = 16;
        let z = bf_known_zero(self.lower_ctx.dfg(), y, &mut budget);
        m & z == m
    }

    fn test_and_compare_bit_const(&mut self, ty: Type, n: u64) -> Option<u8> {
        if n.count_ones() != 1 {
            return None;
        }
        let bit = n.trailing_zeros();
        if bit >= ty.bits() {
            return None;
        }
        Some(bit as u8)
    }

    /// Use as a helper when generating `AluRRRShift` for `extr` instructions.
    fn a64_extr_imm(&mut self, ty: Type, shift: ImmShift) -> ShiftOpAndAmt {
        // The `ShiftOpAndAmt` immediate is used with `AluRRRShift` shape which
        // requires `ShiftOpAndAmt` so the shift of `ty` and `shift` are
        // translated into `ShiftOpAndAmt` here. The `ShiftOp` value here is
        // only used for its encoding, not its logical meaning.
        let (op, expected) = match ty {
            types::I32 => (ShiftOp::LSL, 0b00),
            types::I64 => (ShiftOp::LSR, 0b01),
            _ => unreachable!(),
        };
        assert_eq!(op.bits(), expected);
        ShiftOpAndAmt::new(
            op,
            ShiftOpShiftImm::maybe_from_shift(shift.value().into()).unwrap(),
        )
    }

    fn is_pic(&mut self) -> bool {
        self.backend.flags.is_pic()
    }
}

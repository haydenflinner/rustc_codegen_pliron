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
use crate::FxHashSet;
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
/// modelled dirty anyway), 512 = call-arg `uext` skip
/// (`narrow_arg_uext_clean`: a clean narrow operand needs no
/// `uxtb`/`uxth` at the call site, mirroring the return path), 1024 =
/// narrow `icmp` of `iadd`/`isub`-by-constant against a constant: drop
/// the operand's `uxtb`/`uxth` by rolling the constant (eq/ne) or by
/// comparing the unmasked 32-bit result when the wrapped/unwrapped
/// orderings agree (`narrow_addsub_cmp`), and the same unsigned-bound
/// fold on `br_table` indices (`btsub_imm`), 2048 = cleanliness of
/// blockparams (proven clean when every incoming blockcall arg is
/// clean — `build_clean_blockparams`' greatest-fixpoint sweep) and of
/// call results whose signature return slot declares `uext`.
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
/// forms only match through a `bnot` operand, which is never clean),
/// blockparams in `params` (`build_clean_blockparams` proves them clean
/// from every incoming edge arg), call results whose signature declares a
/// zero-extended return, plus the existing leaf cases so the recursion
/// sees through them. `budget` bounds the walk; running out answers
/// "dirty".
pub(crate) fn narrow_uext_clean(
    lctx: &Lower<'_, MInst>,
    v: Value,
    budget: &mut u32,
    params: &FxHashSet<Value>,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let dfg = lctx.dfg();
    let ty = dfg.value_type(v);
    let mask: u64 = match ty {
        I8 => 0xff,
        I16 => 0xffff,
        _ => return false,
    };
    let def = dfg.value_def(v);
    if let crate::ir::ValueDef::Param(..) = def {
        return params.contains(&v);
    }
    let Some(inst) = def.inst() else {
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
            narrow_uext_clean(lctx, b, budget, params)
                || narrow_uext_clean(lctx, a, budget, params)
        }
        InstructionData::Binary {
            opcode: Opcode::Bor | Opcode::Bxor,
            args,
        } => {
            let [a, b] = *args;
            narrow_uext_clean(lctx, a, budget, params)
                && narrow_uext_clean(lctx, b, budget, params)
        }
        // A call result is clean when this return slot's ABI declares
        // unsigned extension (`call_signature` covers `call`,
        // `call_indirect`, and the `try_call` pair; `try_call` return
        // values are `ValueDef::Result`s of the try_call).
        InstructionData::Call { .. }
        | InstructionData::CallIndirect { .. }
        | InstructionData::TryCall { .. }
        | InstructionData::TryCallIndirect { .. } => {
            narrowext_part(2048)
                && dfg
                    .call_signature(inst)
                    .and_then(|sig| dfg.signatures[sig].returns.get(def.num()))
                    .map(|p| p.extension == ArgumentExtension::Uext)
                    .unwrap_or(false)
        }
        // A narrow `ushr`'s register is clean either way: the default
        // lowering zero-extends the operand first, and the clean-operand
        // rule (`ushr_clean_narrow`) shifts an already-narrow register.
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            ..
        } => true,
        // `ishl v, k` on extended operands computes `(v & mask) << k`;
        // the register is clean only when `v`'s bits at and above
        // `bits - k` are known zero (`bool << 5`, the lowercase-bit
        // idiom, is the common case).
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [a, k],
        } => {
            let Some(k) = iconst_bits(dfg, *k) else {
                return false;
            };
            let bits = ty.bits() as u64;
            if k >= bits {
                return false;
            }
            let keep = bits - k;
            (bf_known_zero(dfg, *a, budget) >> keep) == (u64::MAX >> keep)
        }
        _ => false,
    }
}

/// PLIRON: blockparams whose every incoming blockcall arg is provably
/// narrow-clean — O(E) per pass, converging in a few passes, memoized
/// in `Lower::narrow_clean_params`.
///
/// Greatest-fixpoint direction: start from all params assumed clean and
/// evict any param with an edge arg that fails under the assumption,
/// to a fixpoint. The converged set is sound by coinduction — for every
/// survivor, every edge arg is clean given that all survivors' regs are
/// clean, so induction over any execution trace keeps them clean —
/// and it proves self-sustaining cases a grow-direction pass cannot,
/// like loop-carried `y = phi(clean, band(y, k))`. Intermediate sets
/// are NOT sound, so on the (unrealistic) non-convergence cap the
/// result is thrown away rather than shipped half-proven.
fn build_clean_blockparams(lctx: &Lower<'_, MInst>) -> FxHashSet<Value> {
    let f = lctx.f;
    let dfg = &f.dfg;
    let mut edges: Vec<(Value, Option<Value>)> = Vec::new();
    let mut prev: FxHashSet<Value> = FxHashSet::default();
    for block in f.layout.blocks() {
        for p in dfg.block_params(block) {
            if matches!(dfg.value_type(*p), I8 | I16) {
                prev.insert(*p);
            }
        }
        let Some(term) = f.layout.last_inst(block) else {
            continue;
        };
        for bc in dfg.insts[term].branch_destination(&dfg.jump_tables, &dfg.exception_tables) {
            let target = bc.block(&dfg.value_lists);
            for (i, arg) in bc.args(&dfg.value_lists).enumerate() {
                if let Some(&p) = dfg.block_params(target).get(i) {
                    edges.push((p, arg.as_value()));
                }
            }
        }
    }
    // Entry params whose signature arg doesn't declare uext can be
    // dirty in the register; evict them up front rather than assume.
    if let Some(entry) = f.layout.entry_block() {
        for (i, p) in dfg.block_params(entry).iter().enumerate() {
            if !f
                .signature
                .params
                .get(i)
                .map(|a| a.extension == ArgumentExtension::Uext)
                .unwrap_or(false)
            {
                prev.remove(p);
            }
        }
    }
    for _ in 0..16 {
        let mut next = prev.clone();
        let mut dirty = FxHashSet::default();
        for &(p, arg) in &edges {
            if dirty.contains(&p) {
                continue;
            }
            let ok = match arg {
                Some(av) => {
                    let mut budget = 8;
                    narrow_uext_clean(lctx, av, &mut budget, &prev)
                }
                None => false,
            };
            if !ok {
                next.remove(&p);
                dirty.insert(p);
            }
        }
        if next == prev {
            return next;
        }
        prev = next;
    }
    FxHashSet::default()
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
        // PLIRON (`PLIRON_EXTCLEAN=0` off): `ldrb`/`ldrh`/`ldr w`
        // zero the register above the loaded width. pulldown-cmark's
        // byte dispatch (`ireduce.i8 (uload8.i64 p)`) took a `uxtb` per
        // special byte in `scalar_iterate_special_bytes`.
        InstructionData::Load { opcode, .. } if wideclean_load() => match opcode {
            Opcode::Uload8 => bits >= 8,
            Opcode::Uload16 => bits >= 16,
            Opcode::Uload32 => bits >= 32,
            _ => false,
        },
        _ => false,
    }
}

/// PLIRON (`PLIRON_EXTCLEAN=0` off; default on since 2026-11-24):
/// zero-extending loads and narrow lane extracts (`umov`) count as
/// zero-extended producers.
pub(crate) fn wideclean_load() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PLIRON_EXTCLEAN").as_deref() != Ok("0"))
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

/// PLIRON: shared modular arithmetic of `btsub_imm`/`btsub_pre` (see
/// `btsub_imm`).
fn btsub_imm_inner(op: u64, ty: Type, j: u64, n: u64) -> Option<u64> {
    if ty.bits() > 16 {
        return None;
    }
    let w = 1u64 << ty.bits();
    let j = j & (w - 1);
    let s = match op {
        0 => j,
        1 if j >= w / 2 => w - j,
        _ => return None,
    };
    if s == 0 || s > 0xfff || s + n > w {
        return None;
    }
    Some(s)
}

// PLIRON (`PLIRON_IMMSYNTH`, default on; `=0` off): logical-immediate-aware
// constant synthesis. The upstream `load_constant_full` models constants
// only as movz/movn/movk 16-bit slices; LLVM additionally uses the bitmask
// ("logical") immediates of `orr`/`and`/`eor` — both as exact one-instruction
// forms and as bases or patches combined with `movk` — plus a shifted-orr
// replication trick for constants whose two 32-bit halves are identical.
// This is a port of the candidate set of LLVM's AArch64ExpandImm.cpp, driven
// strictly by instruction count: a candidate replaces the movz/movn/movk
// sequence only when it is strictly shorter, so the transform can never
// lengthen a materialization.
fn pliron_immsynth_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PLIRON_IMMSYNTH").as_deref() != Ok("0"))
}

/// One instruction of a constant-materialization plan. `slice` is the 16-bit
/// slice index (0 = bits 15..0); `bits` is the slice value as it should appear
/// in the final constant for `MovZ`/`MovK` (the inversion needed by the `movn`
/// encoding happens at emission, as in the original code).
#[derive(Clone, Copy, Debug, PartialEq)]
enum SynthStep {
    MovZ { bits: u16, slice: u8, size: OperandSize },
    MovN { bits: u16, slice: u8, size: OperandSize },
    MovK { bits: u16, slice: u8, size: OperandSize },
    /// `orr` with a logical immediate; `orr rd, xzr, #imm` as the first step
    /// of a plan, `orr rd, rd, #imm` afterwards.
    OrrImm { imml: ImmLogic, size: OperandSize },
    /// `and` with a logical immediate; only valid as a non-first step.
    AndImm { imml: ImmLogic, size: OperandSize },
    /// `eor` with a logical immediate; only valid as a non-first step.
    EorImm { imml: ImmLogic, size: OperandSize },
    /// `orr xd, xd, xd, lsl #32`: replicate the low 32 bits into the high
    /// half. Only valid as a non-first step of a 64-bit plan.
    OrrSelfLsl32,
}

/// The 16-bit slice of `value` at slice index `shift`.
fn synth_get(value: u64, shift: u8) -> u16 {
    (value >> (shift * 16)) as u16
}

/// The movz/movn + movk plan: the same sequence the upstream code emitted,
/// expressed as steps. `value` must already be normalized to fit in `size`.
fn movwide_plan(value: u64, size: OperandSize) -> Vec<SynthStep> {
    fn replace(mut old: u64, new: u16, shift: u8) -> u64 {
        let offset = shift * 16;
        old &= !(0xffff << offset);
        old |= u64::from(new) << offset;
        old
    }

    let (mut running_value, op, first) =
        [(MoveWideOp::MovZ, 0), (MoveWideOp::MovN, size.max_value())]
            .into_iter()
            .map(|(op, base)| {
                let first = (0..(size.bits() / 16))
                    .find(|&i| synth_get(base ^ value, i) != 0)
                    .unwrap_or(0);
                (replace(base, synth_get(value, first), first), op, first)
            })
            .min_by_key(|(base, ..)| {
                (0..4)
                    .filter(|&i| synth_get(base ^ value, i) != 0)
                    .count()
            })
            .unwrap();

    let mut steps = Vec::with_capacity(4);
    match op {
        MoveWideOp::MovZ => steps.push(SynthStep::MovZ {
            bits: synth_get(value, first),
            slice: first,
            size,
        }),
        MoveWideOp::MovN => steps.push(SynthStep::MovN {
            bits: synth_get(value, first),
            slice: first,
            size,
        }),
    }

    for shift in (first + 1)..(size.bits() / 16) {
        let bits = synth_get(value, shift);
        if bits != synth_get(running_value, shift) {
            steps.push(SynthStep::MovK {
                bits,
                slice: shift,
                size,
            });
            running_value = replace(running_value, bits, shift);
        }
    }
    let _ = running_value;
    steps
}

/// A single `orr rd, xzr, #imm` when `value` itself is a logical immediate.
fn exact_logic_plan(value: u64, size: OperandSize) -> Option<Vec<SynthStep>> {
    let imml = ImmLogic::maybe_from_u64(value, size.to_ty())?;
    Some(alloc::vec![SynthStep::OrrImm { imml, size }])
}

/// `orr rd, xzr, #imm` plus a single `movk` patching one 16-bit slice. The
/// logical-immediate base only has to match `value` outside the patched
/// slice; following LLVM, three reconstructions of the missing slice are
/// tried per position: zeroed, all-ones, and copied from the opposite 32-bit
/// half (which is what covers replicated-byte-pattern constants with one
/// deviating slice, e.g. SWAR masks like 0xfefefefefefefeff).
fn orr_movk_plan(value: u64) -> Option<Vec<SynthStep>> {
    for slice in 0..4u8 {
        let mask = 0xffffu64 << (slice * 16);
        let zero_chunk = value & !mask;
        let one_chunk = value | mask;
        let rot = value.rotate_left(32);
        let replicate_chunk = zero_chunk | (rot & mask);
        for cand in [zero_chunk, one_chunk, replicate_chunk] {
            if let Some(imml) = ImmLogic::maybe_from_u64(cand, I64) {
                return Some(alloc::vec![
                    SynthStep::OrrImm {
                        imml,
                        size: OperandSize::Size64,
                    },
                    SynthStep::MovK {
                        bits: synth_get(value, slice),
                        slice,
                        size: OperandSize::Size64,
                    },
                ]);
            }
        }
    }
    None
}

// --- Helpers for the decomposition of a constant into the orr/and/eor of a
// --- pair of logical immediates, ported from LLVM's AArch64ExpandImm.cpp.

fn run_of_ones_starting_at(v: u64, pos: u32) -> u64 {
    let num = (v >> pos).trailing_ones();
    let unshifted = if num == 64 { u64::MAX } else { (1u64 << num) - 1 };
    unshifted << pos
}

fn maximally_replicate_sub_imm(v: u64, subset: u64) -> u64 {
    let mut result = subset;
    for i in 0..6u32 {
        let rotation = 1u32 << (6 - i);
        let closure = result | result.rotate_left(rotation);
        if closure != (closure & v) {
            break;
        }
        result = closure;
    }
    result
}

/// Find the logical immediate covering the most bits of `remaining`, allowing
/// extra bits only where they are also set in `original`.
fn maximal_logical_imm_within(remaining: u64, original: u64) -> u64 {
    let pos = remaining.trailing_zeros();
    let first_run = run_of_ones_starting_at(original, pos);
    maximally_replicate_sub_imm(original, first_run)
}

/// Decompose `uimm` into `(a, b)` with `uimm == a | b` where both are logical
/// immediates-by-construction (encodability is checked by the caller).
fn decompose_into_orr_of_logical_immediates(uimm: u64) -> Option<(u64, u64)> {
    if uimm == 0 || uimm == u64::MAX {
        return None;
    }
    // Avoid a run of ones split around the rotation boundary.
    let initial_trailing_ones = uimm.trailing_ones();
    let rotated = uimm.rotate_right(initial_trailing_ones);

    let m1 = maximal_logical_imm_within(rotated, rotated);
    let remaining = rotated & !m1;
    if remaining == 0 {
        // `rotated` is a single repeating run — already covered by the exact
        // logical-immediate case.
        return None;
    }
    let m2 = maximal_logical_imm_within(remaining, rotated);
    if remaining & !m2 != 0 {
        return None;
    }
    Some((
        m1.rotate_left(initial_trailing_ones),
        m2.rotate_left(initial_trailing_ones),
    ))
}

/// `orr` of two logical immediates (e.g. 0x5c5c5c5c5c5c5c5c, a repeated byte
/// with two runs of ones per element, = 0x1c1c.. | 0x4444..).
fn orr_orr_plan(value: u64) -> Option<Vec<SynthStep>> {
    let (i1, i2) = decompose_into_orr_of_logical_immediates(value)?;
    let l1 = ImmLogic::maybe_from_u64(i1, I64)?;
    let l2 = ImmLogic::maybe_from_u64(i2, I64)?;
    Some(alloc::vec![
        SynthStep::OrrImm {
            imml: l1,
            size: OperandSize::Size64,
        },
        SynthStep::OrrImm {
            imml: l2,
            size: OperandSize::Size64,
        },
    ])
}

/// `orr` then `and` of logical immediates, via De Morgan on the inverse.
fn orr_and_plan(value: u64) -> Option<Vec<SynthStep>> {
    let (i1, i2) = decompose_into_orr_of_logical_immediates(!value)?;
    let l1 = ImmLogic::maybe_from_u64(!i1, I64)?;
    let l2 = ImmLogic::maybe_from_u64(!i2, I64)?;
    Some(alloc::vec![
        SynthStep::OrrImm {
            imml: l1,
            size: OperandSize::Size64,
        },
        SynthStep::AndImm {
            imml: l2,
            size: OperandSize::Size64,
        },
    ])
}

/// `orr` then `eor` of logical immediates; covers the remaining repeated
/// byte/halfword patterns and some irregular bitmasks.
fn orr_eor_plan(value: u64) -> Option<Vec<SynthStep>> {
    // Determine the larger repetition size of the two possible logical
    // immediates, by finding the repetition size of `value`.
    let mut big_size = 64u32;
    loop {
        big_size /= 2;
        let mask = (1u64 << big_size) - 1;
        if (value & mask) != ((value >> big_size) & mask) {
            big_size *= 2;
            break;
        }
        if big_size <= 2 {
            break;
        }
    }

    let big_mask = u64::MAX >> (64 - big_size);
    // The last bit of each run of ones, circularly.
    let run_starts = value & !value.rotate_left(1);
    let runs_per_big_chunk = (run_starts & big_mask).count_ones() as usize;

    const BIG_TO_SMALL_SIZE_TABLE: [i32; 32] = [
        -1, -1, 0, 1, 2, 2, -1, 3, 3, 3, -1, -1, -1, -1, -1, 4, 4, 4, -1, -1, -1, -1, -1, -1, -1,
        -1, -1, -1, -1, -1, -1, 5,
    ];
    if runs_per_big_chunk >= BIG_TO_SMALL_SIZE_TABLE.len() {
        return None;
    }
    let big_to_small_shift = BIG_TO_SMALL_SIZE_TABLE[runs_per_big_chunk];
    if big_to_small_shift == -1 {
        return None;
    }
    let small_size = big_size >> big_to_small_shift;

    // 64-bit values with a bit set every (1 << index) bits.
    const REPEATED_ONES_TABLE: [u64; 7] = [
        0xffffffffffffffff,
        0x5555555555555555,
        0x1111111111111111,
        0x0101010101010101,
        0x0001000100010001,
        0x0000000100000001,
        0x0000000000000001,
    ];
    let small_ones = REPEATED_ONES_TABLE[small_size.trailing_zeros() as usize];

    // Try to find the number of ones in each of the smaller repetitions by
    // looking at runs of ones in `value`; up to three attempts, as the eor
    // may have changed the length of the first two runs we find.
    let mut rotation = run_starts.trailing_zeros();
    let mut rotated_imm = value.rotate_right(rotation);
    for _attempt in 0..3 {
        let run_length = rotated_imm.trailing_ones();

        let small_imm = (small_ones
            .wrapping_shl(run_length)
            .wrapping_sub(small_ones))
        .rotate_left(rotation);
        let big_imm = value ^ small_imm;

        if let (Some(big_imml), Some(small_imml)) = (
            ImmLogic::maybe_from_u64(big_imm, I64),
            ImmLogic::maybe_from_u64(small_imm, I64),
        ) {
            return Some(alloc::vec![
                SynthStep::OrrImm {
                    imml: small_imml,
                    size: OperandSize::Size64,
                },
                SynthStep::EorImm {
                    imml: big_imml,
                    size: OperandSize::Size64,
                },
            ]);
        }

        // Rotate to the next run of ones.
        rotation = rotation.wrapping_add(
            (run_starts.rotate_right(rotation) & !1).trailing_zeros(),
        );
        rotated_imm = value.rotate_right(rotation);
    }
    None
}

/// A 16-bit chunk occurring two or three times whose 4x replication is a
/// logical immediate: materialize the replication with one `orr` and patch
/// the deviating chunks with `movk` (e.g. |A|B|A|A| or |A|B|C|A|).
fn replicate_chunks_plan(value: u64) -> Option<Vec<SynthStep>> {
    let chunks = [
        synth_get(value, 0),
        synth_get(value, 1),
        synth_get(value, 2),
        synth_get(value, 3),
    ];
    for &cand in chunks.iter() {
        let count = chunks.iter().filter(|&&c| c == cand).count();
        if count != 2 && count != 3 {
            continue;
        }
        let c = u64::from(cand);
        let rep = (c << 48) | (c << 32) | (c << 16) | c;
        if let Some(imml) = ImmLogic::maybe_from_u64(rep, I64) {
            let mut steps = alloc::vec![SynthStep::OrrImm {
                imml,
                size: OperandSize::Size64,
            }];
            for slice in 0..4u8 {
                if chunks[slice as usize] != cand {
                    steps.push(SynthStep::MovK {
                        bits: chunks[slice as usize],
                        slice,
                        size: OperandSize::Size64,
                    });
                }
            }
            return Some(steps);
        }
    }
    None
}

/// `isMask_64` from LLVM: ones contiguous from bit 0 upwards.
fn synth_is_mask(x: u64) -> bool {
    (x & x.wrapping_add(1)) == 0
}

/// A contiguous sequence of ones interrupted by one or two chunks: the run is
/// materialized with one `orr`, the interrupting/outside chunks with `movk`.
fn sequence_of_ones_plan(value: u64) -> Option<Vec<SynthStep>> {
    let not_set = -1i32;
    let mut start_idx = not_set;
    let mut end_idx = not_set;
    for idx in 0..4i32 {
        // Sign-extend the 16-bit chunk to 64 bits.
        let chunk = ((synth_get(value, idx as u8) as i64) << 48) >> 48;
        let chunk = chunk as u64;
        let is_start = chunk != 0 && chunk != u64::MAX && synth_is_mask(!chunk);
        let is_end = chunk != 0 && chunk != u64::MAX && synth_is_mask(chunk);
        if is_start {
            start_idx = idx;
        } else if is_end {
            end_idx = idx;
        }
    }
    if start_idx == not_set || end_idx == not_set {
        return None;
    }

    let mut outside = 0u64;
    let mut inside = 0xffffu64;
    let (mut start_idx, mut end_idx) = (start_idx, end_idx);
    if start_idx > end_idx {
        // The sequence of ones wraps around the MSB into the LSB; swap and
        // pretend we are materializing zeros surrounded by ones.
        core::mem::swap(&mut start_idx, &mut end_idx);
        core::mem::swap(&mut outside, &mut inside);
    }

    let mut orr_imm = value;
    let mut movks: Vec<u8> = Vec::new();
    for idx in 0..4i32 {
        let chunk = u64::from(synth_get(value, idx as u8));
        if (idx < start_idx || idx > end_idx) && chunk != outside {
            // A chunk outside the contiguous sequence of ones.
            if outside == 0 {
                orr_imm &= !(0xffffu64 << (idx * 16));
            } else {
                orr_imm |= 0xffffu64 << (idx * 16);
            }
            movks.push(idx as u8);
        } else if idx > start_idx && idx < end_idx && chunk != inside {
            // A chunk interrupting the sequence of ones.
            if inside != 0xffff {
                orr_imm &= !(0xffffu64 << (idx * 16));
            } else {
                orr_imm |= 0xffffu64 << (idx * 16);
            }
            movks.push(idx as u8);
        }
    }
    if movks.is_empty() || movks.len() > 2 {
        return None;
    }
    let imml = ImmLogic::maybe_from_u64(orr_imm, I64)?;
    let mut steps = alloc::vec![SynthStep::OrrImm {
        imml,
        size: OperandSize::Size64,
    }];
    for slice in movks {
        steps.push(SynthStep::MovK {
            bits: synth_get(value, slice),
            slice,
            size: OperandSize::Size64,
        });
    }
    Some(steps)
}

/// Identical 32-bit halves: build the low half with movz/movn/movk in 32-bit
/// form (which zeroes the top), then replicate it upward with
/// `orr xd, xd, xd, lsl #32`.
fn half_replication_plan(value: u64) -> Option<Vec<SynthStep>> {
    let low = value & 0xffff_ffff;
    if (value >> 32) != low || low == 0 {
        return None;
    }
    let mut steps = movwide_plan(low, OperandSize::Size32);
    if steps.len() > 2 {
        return None;
    }
    steps.push(SynthStep::OrrSelfLsl32);
    Some(steps)
}

/// Evaluate the full candidate set for a normalized `value` of the given
/// size and return the shortest plan. Ties keep the movz/movn/movk plan.
fn synth_plan(value: u64, size: OperandSize) -> Vec<SynthStep> {
    let mut best = movwide_plan(value, size);

    if !pliron_immsynth_enabled() || best.len() <= 1 {
        return best;
    }

    // A single `orr` with a logical immediate beats any multi-instruction
    // sequence. (For zero-extended constants the ISLE rules ahead of
    // `load_constant_full` catch most of these already, but not the
    // sign-extended narrow forms nor inverted-encodable values.)
    if let Some(plan) = exact_logic_plan(value, size) {
        if plan.len() < best.len() {
            best = plan;
        }
    }

    // Everything below is only profitable for 64-bit constants: 32-bit ones
    // always fit in movz/movn + at most one movk, so no 2+-instruction
    // logical sequence can be strictly shorter than the baseline except the
    // exact form handled above.
    if size != OperandSize::Size64 {
        return best;
    }

    if best.len() > 2 {
        // Two- and three-instruction candidates; a candidate wins only when
        // strictly shorter than the best plan so far.
        for plan in [
            orr_movk_plan(value),
            orr_orr_plan(value),
            orr_and_plan(value),
            orr_eor_plan(value),
            replicate_chunks_plan(value),
            sequence_of_ones_plan(value),
            half_replication_plan(value),
        ]
        .into_iter()
        .flatten()
        {
            if plan.len() < best.len() {
                best = plan;
            }
        }
    }

    best
}

/// Bit-level reference evaluation of a plan, used to check correctness in
/// debug builds and tests: returns the final register value. `movn`'s bits
/// are the un-inverted slice value of the target (the encoding inversion is
/// accounted for here). Always compiled: `debug_assert!` still name-resolves
/// its arguments when the assertions themselves are compiled out.
#[allow(dead_code)]
fn eval_synth_steps(steps: &[SynthStep]) -> u64 {
    let mut reg: u64 = 0;
    for (i, step) in steps.iter().enumerate() {
        match *step {
            SynthStep::MovZ { bits, slice, size } => {
                debug_assert!(i == 0, "movz must be the first step");
                reg = u64::from(bits) << (slice * 16);
                if size == OperandSize::Size32 {
                    reg &= u64::from(u32::MAX);
                }
            }
            SynthStep::MovN { bits, slice, size } => {
                debug_assert!(i == 0, "movn must be the first step");
                let width = u64::from(size.bits());
                let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
                // movn encodes the inverse of the slice; the net effect is
                // all-ones within the width except `bits` in the slice.
                reg = mask & !(0xffffu64 << (slice * 16));
                reg |= u64::from(bits) << (slice * 16);
                reg &= mask;
            }
            SynthStep::MovK { bits, slice, size } => {
                debug_assert!(i != 0, "movk cannot be the first step");
                let width = u64::from(size.bits());
                let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
                reg &= mask & !(0xffffu64 << (slice * 16));
                reg |= u64::from(bits) << (slice * 16);
                reg &= mask;
            }
            SynthStep::OrrImm { imml, size } => {
                let mask = if size == OperandSize::Size64 {
                    u64::MAX
                } else {
                    u64::from(u32::MAX)
                };
                if i == 0 {
                    reg = imml.value() & mask;
                } else {
                    reg |= imml.value() & mask;
                }
                if size == OperandSize::Size32 {
                    reg &= u64::from(u32::MAX);
                }
            }
            SynthStep::AndImm { imml, size } => {
                debug_assert!(i != 0, "and cannot be the first step");
                let mask = if size == OperandSize::Size64 {
                    u64::MAX
                } else {
                    u64::from(u32::MAX)
                };
                reg &= imml.value() & mask;
                if size == OperandSize::Size32 {
                    reg &= u64::from(u32::MAX);
                }
            }
            SynthStep::EorImm { imml, size } => {
                debug_assert!(i != 0, "eor cannot be the first step");
                let mask = if size == OperandSize::Size64 {
                    u64::MAX
                } else {
                    u64::from(u32::MAX)
                };
                reg ^= imml.value() & mask;
                if size == OperandSize::Size32 {
                    reg &= u64::from(u32::MAX);
                }
            }
            SynthStep::OrrSelfLsl32 => {
                debug_assert!(i != 0, "orr-shift cannot be the first step");
                reg &= u64::from(u32::MAX);
                reg |= reg << 32;
            }
        }
    }
    reg
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

        let mut info = self
            .lower_ctx
            .gen_call_info(sig, dest, uses, defs, try_call_info, patchable);
        // PLIRON: FASTCC (`PLIRON_FASTCC=1`; scheme in `machinst/abi.rs`).
        // A direct call to a local callee whose machine code is already
        // known destroys only what that code writes: narrow the ABI
        // clobber set to the callee's recorded set. Not for `try_call`
        // (the unwind edge resumes with ABI-clobbered state) and not for
        // patchable calls (the target can change).
        if info.try_call_info.is_none() && !info.patchable {
            if let ExternalName::User(name_ref) = &info.dest {
                let callee = self
                    .lower_ctx
                    .f
                    .params
                    .user_named_funcs()
                    .get(*name_ref)
                    .and_then(|n| crate::machinst::pliron_fastcc_callee(n.namespace, n.index));
                if let Some(mut keep) = callee {
                    // What the call sequence itself writes regardless of
                    // the callee's body: the IP scratch registers
                    // (prologue/epilogue and emitter temporaries, linker
                    // veneers and branch islands), the link register
                    // (`bl`), and the temporary that moves stack-carried
                    // return values into spill slots after the call.
                    keep.add(crate::isa::aarch64::inst::regs::xreg_preg(16));
                    keep.add(crate::isa::aarch64::inst::regs::xreg_preg(17));
                    keep.add(crate::isa::aarch64::inst::regs::xreg_preg(30));
                    let any_stack_ret = info.defs.iter().any(|d| {
                        matches!(d.location, crate::machinst::abi::RetLocation::Stack(..))
                    });
                    if any_stack_ret {
                        let temp = <AArch64MachineDeps as crate::machinst::abi::ABIMachineSpec>::retval_temp_reg(info.callee_conv);
                        keep.add(PReg::from(temp.to_reg().to_real_reg().unwrap()));
                    }
                    // Intersection: registers outside the ABI set stay
                    // callee-preserved, return registers stay removed.
                    info.clobbers.intersect_from(keep);
                }
            }
        }
        Box::new(info)
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

        // PLIRON: FASTCC — the callee's recorded write set, if it is known.
        let pliron_clobbers = match &dest {
            ExternalName::User(name_ref) => self
                .lower_ctx
                .f
                .params
                .user_named_funcs()
                .get(*name_ref)
                .and_then(|n| crate::machinst::pliron_fastcc_callee(n.namespace, n.index)),
            _ => None,
        };
        Box::new(ReturnCallInfo {
            dest,
            uses,
            key,
            sign_return_address_all: self.backend.isa_flags.sign_return_address_all(),
            new_stack_arg_size,
            pliron_clobbers,
            call_conv: self.lower_ctx.sigs()[sig].call_conv(),
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
            pliron_clobbers: None,
            call_conv: self.lower_ctx.sigs()[sig].call_conv(),
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

        // The 32-bit versions of `movz`/`movn`/`movk` will clear the upper 32
        // bits, so if that's the outcome we want we might as well use them. For
        // simplicity and ease of reading the disassembly, we use the same size
        // for all instructions in the sequence.
        let size = if value >> 32 == 0 {
            OperandSize::Size32
        } else {
            OperandSize::Size64
        };

        // PLIRON (`PLIRON_IMMSYNTH`): pick the shortest materialization out
        // of the movz/movn/movk slice sequence and the logical-immediate
        // candidates; see `synth_plan`. The movz/movn/movk-only plan is what
        // upstream emitted, and no candidate is ever longer than it.
        let steps = synth_plan(value, size);
        debug_assert_eq!(
            value,
            eval_synth_steps(&steps),
            "constant synthesis of {value:#x} must reproduce the value"
        );

        // Emit the plan, chaining each instruction through a fresh temporary
        // (vcode wants one def per virtual register).
        let mut rn: Option<Reg> = None;
        for (i, step) in steps.iter().enumerate() {
            let rd = self.temp_writable_reg(I64);
            match *step {
                SynthStep::MovZ { bits, slice, size } | SynthStep::MovK { bits, slice, size } => {
                    if i == 0 {
                        self.lower_ctx.emit(MInst::MovWide {
                            op: MoveWideOp::MovZ,
                            rd,
                            imm: MoveWideConst { bits, shift: slice },
                            size,
                        });
                    } else {
                        self.lower_ctx.emit(MInst::MovK {
                            rd,
                            rn: rn.expect("movk cannot be the first step"),
                            imm: MoveWideConst { bits, shift: slice },
                            size,
                        });
                    }
                }
                SynthStep::MovN { bits, slice, size } => {
                    debug_assert_eq!(i, 0, "movn must be the first step");
                    // The encoding for the immediate operand is
                    // bitwise-inverted for `movn`.
                    self.lower_ctx.emit(MInst::MovWide {
                        op: MoveWideOp::MovN,
                        rd,
                        imm: MoveWideConst {
                            bits: !bits,
                            shift: slice,
                        },
                        size,
                    });
                }
                SynthStep::OrrImm { imml, size } => {
                    self.lower_ctx.emit(MInst::AluRRImmLogic {
                        alu_op: ALUOp::Orr,
                        size,
                        rd,
                        rn: rn.unwrap_or_else(zero_reg),
                        imml,
                    });
                }
                SynthStep::AndImm { imml, size } => {
                    self.lower_ctx.emit(MInst::AluRRImmLogic {
                        alu_op: ALUOp::And,
                        size,
                        rd,
                        rn: rn.expect("and cannot be the first step"),
                        imml,
                    });
                }
                SynthStep::EorImm { imml, size } => {
                    self.lower_ctx.emit(MInst::AluRRImmLogic {
                        alu_op: ALUOp::Eor,
                        size,
                        rd,
                        rn: rn.expect("eor cannot be the first step"),
                        imml,
                    });
                }
                SynthStep::OrrSelfLsl32 => {
                    let rm = rn.expect("orr-shift cannot be the first step");
                    self.lower_ctx.emit(MInst::AluRRRShift {
                        alu_op: ALUOp::Orr,
                        size: OperandSize::Size64,
                        rd,
                        rn: rm,
                        rm,
                        shiftop: ShiftOpAndAmt::new(
                            ShiftOp::LSL,
                            ShiftOpShiftImm::maybe_from_shift(32).unwrap(),
                        ),
                    });
                }
            }
            rn = Some(rd.to_reg());
        }

        return rn.expect("a synthesis plan is never empty");
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

    /// PLIRON (`PLIRON_MINMAXIMM=1`): min/max against a small constant
    /// compares with the immediate (and selects `xzr` for zero).
    /// PLIRON (`PLIRON_PAIRWISE_ISLE`, default on, `=0` off): `umax`/`iadd` of a vector's even
    /// and odd lanes (`uzp1`/`uzp2` shuffles) as one `umaxp`/`addp`.
    fn pliron_pairwise_isle(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_PAIRWISE_ISLE").as_deref() != Ok("0"))
    }

    fn pliron_minmax_imm(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_MINMAXIMM").as_deref() != Ok("0"))
    }

    /// PLIRON (`PLIRON_UNCHECKDIV=1`, opt-in): MIR's `Assert` dominates every
    /// `/`/`%`, so the `cbz`+trap `trap_if_zero_divisor`/`trap_if_div_overflow`
    /// add on each op is dead — emit bare `udiv`/`sdiv`+`msub` like LLVM.
    fn pliron_unchecked_div(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_UNCHECKDIV").as_deref() == Ok("1"))
    }

    /// PLIRON (`PLIRON_ZRMOV`, default on, `=0` off): materialize `iconst 0` as
    /// `mov wN/xN, wzr/xzr` (the ORR register-move form LLVM uses) instead of
    /// `movz wN, #0`. Same instruction count; the register-move form is a
    /// move-elimination candidate where `movz` executes on an ALU
    /// (rust-protobuf ~560k sites: cycles 0.992x at 1.001x instr).
    fn pliron_zrmov(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_ZRMOV").as_deref() != Ok("0"))
    }

    /// PLIRON (`PLIRON_ZRSTP=1`, opt-in): an `iconst 0` half of an `iconcat`
    /// stored via `stp` rides `xzr` instead of a `mov xN, #0` materialization.
    fn pliron_zrstp(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_ZRSTP").as_deref() == Ok("1"))
    }

    /// PLIRON (`PLIRON_CARRYCMP=1`, opt-in): the carry-check idiom
    /// `icmp ult (a+b), a` (and the `uge`/`ugt`/`ule` mirrors) is the
    /// carry-out of `a+b` — return the `Cond` that reads it off the `cmn`
    /// the emit_icmp rules emit, or None when `other` is not an `iadd`
    /// operand or the relation isn't a single flag. `sum_lhs` says which
    /// side of the compare the sum sits on.
    fn carry_cmp_cond(
        &mut self,
        cc: &IntCC,
        other: Value,
        a: Value,
        b: Value,
        sum_lhs: bool,
    ) -> Option<Cond> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_CARRYCMP").as_deref() == Ok("1")) {
            return None;
        }
        if other != a && other != b {
            return None;
        }
        let c = match (cc, sum_lhs) {
            // `sum <u x` / `x >u sum`: the add wrapped.
            (IntCC::UnsignedLessThan, true) | (IntCC::UnsignedGreaterThan, false) => Cond::Hs,
            // `sum >=u x` / `x <=u sum`: it did not.
            (IntCC::UnsignedGreaterThanOrEqual, true) | (IntCC::UnsignedLessThanOrEqual, false) => {
                Cond::Lo
            }
            _ => return None,
        };
        Some(c)
    }

    /// PLIRON (`PLIRON_AMODENEG`, default on, `=0` off): `-off` as an `Imm12` for a negative
    /// `off`, so `amode_add` subtracts it instead of materializing it.
    fn amode_neg_imm12(&mut self, off: i32) -> Option<Imm12> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if off >= 0 || !*ON.get_or_init(|| std::env::var("PLIRON_AMODENEG").as_deref() != Ok("0")) {
            return None;
        }
        Imm12::maybe_from_u64(i64::from(off).unsigned_abs())
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
    ///
    /// `PLIRON_RORFOLD_DIRECT` (on by default since 2026-11-26; `=0`
    /// disables) additionally folds when the rotate has
    /// at most one *direct* use (`value_direct_uses`), even if the
    /// transitive `ValueUseState` marks it `Multiple` because its single
    /// consumer's result fans out (a loop's `bxor` feeding both the
    /// backedge and the exit args). That mark exists for producers that
    /// could be emitted once per consumer site — but a fold here never
    /// materializes the rotate: with one direct use the `ror` simply
    /// dies, which is strictly fewer instructions and no added latency
    /// (unlike the genuinely multi-use case, where the `ror` is emitted
    /// anyway and the shifted-operand op costs a cycle on Apple cores —
    /// blake3's mixing chain). SipHash-1-3's round loop
    /// (`RandomState::hash_one` on regex's DFA-state cache) keeps every
    /// compress rotate singly-used but transitively `Multiple`:
    /// -163k executed instructions on regex.
    fn rot_fold_ok(&mut self, val: Value) -> bool {
        static MULTI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static DIRECT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *MULTI.get_or_init(|| std::env::var("PLIRON_RORFOLD_MULTI").as_deref() == Ok("1"))
            || !self.lower_ctx.value_has_multiple_uses(val)
            || (*DIRECT.get_or_init(|| std::env::var("PLIRON_RORFOLD_DIRECT").as_deref() != Ok("0"))
                && self.lower_ctx.value_single_direct_use(val))
    }

    /// PLIRON: `PLIRON_TBNZ_IREDUCE=0` off (default on), see `lower.isle`.
    fn tbnz_ireduce_on(&mut self, _bit: u64) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_TBNZ_IREDUCE").as_deref() != Ok("0"))
    }

    /// PLIRON: `PLIRON_VTRUEBR=1` (opt-in), see `inst.isle`.
    fn vtruebr_on(&mut self, _bit: u64) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_VTRUEBR").as_deref() == Ok("1"))
    }

    fn narrowext_on(&mut self, bit: u64) -> bool {
        narrowext_part(bit)
    }

    /// PLIRON (`PLIRON_NARROWEXT` bit 1024): a narrow `icmp` of
    /// `isub c, j` / `iadd c, j` (iconst `j`) against iconst `k` can often
    /// drop the operand's `uxtb`/`uxth`, because the wrapped and
    /// unwrapped 32-bit results order identically against `k`. Returns:
    ///   `C`   — compare `c` itself against the rolled constant `k2`
    ///           (eq/ne: `(c ∓ j) mod W == k  ⟺  c == (k ± j) mod W`);
    ///           `put_in_reg_ext32_maybe` covers a dirty `c`.
    ///   `Sub` — compare the `isub` result itself: unmasked `c-j` ∈
    ///           [-j, W-1-j] orders the same as the masked result
    ///           against `k` iff `j + k' ≤ W` (`k'` = k for lt/ge,
    ///           k+1 for le/gt) — wrapped negatives are unsigned-huge
    ///           and the masked range [W-j, W) must all fail.
    ///   `Rewrite` — `iadd c, j` with j ≥ W/2 is `isub c, W-j` in the
    ///           masked type: emit `sub c, s` (`s` = W-j) and compare
    ///           unmasked; needs `k' ≤ j` so the no-wrap masked range
    ///           [j, W) all fails the compare.
    /// `put_in_reg_uext_narrow_maybe` widens a dirty `c` before the
    /// sub/cmp, so the bounds are checked against `c & mask` either
    /// way — no cleanliness precondition.
    fn narrow_addsub_cmp(
        &mut self,
        op: u64,
        cc: &IntCC,
        ty: Type,
        _c: Value,
        j: u64,
        k: u64,
    ) -> Option<generated_code::NarrowAddsubCmp> {
        use crate::ir::condcodes::IntCC::*;
        use generated_code::NarrowAddsubCmp as R;
        if !narrowext_part(1024) || ty.bits() > 16 {
            return None;
        }
        let w = 1u64 << ty.bits();
        let mask = w - 1;
        // Narrow `iconst`s store sign-extended bits (`iconst.i8 -9`);
        // the modular arithmetic wants the truncated value.
        let j = j & mask;
        let k = k & mask;
        // Everything emitted is an imm12.
        if k > 0xfff {
            return None;
        }
        match (op, cc) {
            (0, Equal) | (0, NotEqual) => {
                let k2 = k.wrapping_add(j) & mask;
                (k2 <= 0xfff).then_some(R::C { k2 })
            }
            (1, Equal) | (1, NotEqual) => {
                let k2 = k.wrapping_sub(j) & mask;
                (k2 <= 0xfff).then_some(R::C { k2 })
            }
            (0, UnsignedLessThan) | (0, UnsignedGreaterThanOrEqual) if j + k <= w => {
                (j <= 0xfff).then_some(R::Sub { j, k2: k })
            }
            (0, UnsignedLessThanOrEqual) | (0, UnsignedGreaterThan) if j + k + 1 <= w => {
                (j <= 0xfff).then_some(R::Sub { j, k2: k })
            }
            (1, UnsignedLessThan) | (1, UnsignedGreaterThanOrEqual) if j >= w / 2 && k <= j => {
                (w - j <= 0xfff).then_some(R::Rewrite { s: w - j, k2: k })
            }
            (1, UnsignedLessThanOrEqual) | (1, UnsignedGreaterThan)
                if j >= w / 2 && k + 1 <= j =>
            {
                (w - j <= 0xfff).then_some(R::Rewrite { s: w - j, k2: k })
            }
            _ => None,
        }
    }

    /// PLIRON (`PLIRON_NARROWEXT` bit 1024): the `br_table` bounds
    /// check `uextend((c - s) mod W) <u N` partitions identically to
    /// the unmasked `sub w, c, s` unsigned compare when `s + N <= W` —
    /// no-wrap cases compare equal, wrapped cases produce masked values
    /// in `[W-s, W)` (all `>= N`) while the unmasked result is
    /// unsigned-huge (also `>= N`), and the surviving index is the
    /// correct `c - s`. `op` 0 = `isub c, j` (`s` = j); 1 =
    /// `iadd c, j`, accepted only when `j` is a negative narrow
    /// constant (`s` = W-j) — a positive `iadd` wraps masked indices
    /// back below `N` and cannot fold. Returns `s`, or none when the
    /// bound fails, `s` overflows imm12, or `s` is 0 (`iadd c, 0`).
    fn btsub_imm(&mut self, op: u64, ty: Type, j: u64, n: u64) -> Option<u64> {
        if !narrowext_part(1024) {
            return None;
        }
        btsub_imm_inner(op, ty, j, n)
    }

    /// PLIRON (`PLIRON_SWITCH_PRETEST`, default on since 2026-11-27,
    /// `=0` off): the same `br_table`
    /// index shape as `btsub_imm`, but instead of subtracting the lowest
    /// case value, the table gains `s` copies of the default target in
    /// front and is indexed by the raw value: the bounds check becomes
    /// `cmp x, #(s+n); b.hs default` with no subtraction anywhere —
    /// one instruction less for every byte above the highest case (the
    /// dominant default arm of lol_html's state machines, where letters
    /// sit above the dense `0x9..0x3e` range) and one less for every
    /// in-range byte, at the cost of `s` table bytes and a full table
    /// trip (instead of the short bounds branch) for values below the
    /// lowest case. That last case is why the padding is capped small:
    /// `PLIRON_SWITCH_PRETEST_PAD` (default 64). Values below the lowest
    /// case of a byte switch in practice are control characters, which
    /// the parsed formats exclude. Returns `s` (the padding count).
    fn btsub_pre(&mut self, op: u64, ty: Type, j: u64, n: u64) -> Option<u64> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_SWITCH_PRETEST").as_deref() != Ok("0")) {
            return None;
        }
        static PAD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let pad = *PAD.get_or_init(|| {
            std::env::var("PLIRON_SWITCH_PRETEST_PAD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(64)
        });
        let s = btsub_imm_inner(op, ty, j, n)?;
        (s <= pad).then_some(s)
    }

    /// PLIRON: `pad` copies of `default` in front of the jump table
    /// entries, for `btsub_pre`.
    fn jt_pad_targets(
        &mut self,
        default: MachLabel,
        targets: &BoxVecMachLabel,
        pad: u64,
    ) -> BoxVecMachLabel {
        let mut v = Vec::with_capacity(targets.len() + pad as usize);
        v.extend(std::iter::repeat(default).take(pad as usize));
        v.extend(targets.iter().copied());
        Box::new(v)
    }

    /// PLIRON (`PLIRON_NARROWEXT` bit 8): the mask for a `tst` of a
    /// narrow zero-test — `(v mod W) & k` reads identically on the
    /// unmasked source register. `val` is the narrow value (its own
    /// type gives `W`); returns `k & (W-1)`, or none when the value
    /// isn't narrow or the masked constant is 0.
    fn narrowext_tst_mask(&mut self, val: Value, k: u64) -> Option<u64> {
        if !narrowext_part(8) {
            return None;
        }
        let ty = self.lower_ctx.dfg().value_type(val);
        if ty.bits() > 16 {
            return None;
        }
        let m = k & ((1u64 << ty.bits()) - 1);
        (m != 0).then_some(m)
    }

    fn narrowext_clean(&mut self, val: Value) -> bool {
        let mut budget = 16;
        if !narrowext_part(1) {
            return false;
        }
        // Bit 2048: blockparam cleanliness from incoming edge args.
        static EMPTY: std::sync::OnceLock<FxHashSet<Value>> = std::sync::OnceLock::new();
        let rc;
        let params: &FxHashSet<Value> = if narrowext_part(2048) {
            rc = self.lower_ctx.clean_blockparams(build_clean_blockparams);
            &rc
        } else {
            EMPTY.get_or_init(FxHashSet::default)
        };
        narrow_uext_clean(self.lower_ctx, val, &mut budget, params)
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

    /// PLIRON (`PLIRON_EXTCLEAN=0` off): see `wideclean_load`.
    fn extclean_on(&mut self) -> bool {
        wideclean_load()
    }

    /// PLIRON (`PLIRON_CMN`, default on): `cmp x, -c` -> `cmn x, c`.
    fn cmn_on(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_CMN").as_deref() != Ok("0"))
    }

    /// PLIRON (`PLIRON_CMN_NARROW`, default on since 2026-11-23, `=0`
    /// off): `cmp`/`cmn` immediates for 8/16-bit signed compares
    /// (`emit_icmp` rules 12 and 13).
    fn cmn_narrow_on(&mut self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_CMN_NARROW").as_deref() != Ok("0"))
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
        let s = k.checked_add(w as u64)?;
        (s <= bits as u64).then_some((s - 1) as u8)
    }

    /// `(x<<k)&m` → `ubfiz`: `m = ((1<<w)-1)<<k`; imms = k+w-1.
    fn bf_field_imms(&mut self, ty: Type, m: u64, k: u64) -> Option<u8> {
        let bits = ty.bits();
        let w = bf_field_w(bits, m, k)?;
        Some((k + w as u64 - 1) as u8)
    }

    /// `ubfiz` with lsb `k`: immr = regsize - k. The register size, not
    /// the type's width: an i8/i16 op runs as a 32-bit `ubfm`, whose
    /// insert position is `32 - immr` — `16 - k` put an i16 field at bit
    /// `16 + k` and left the value's own bits zero (fuzz seeds 30694,
    /// 31084).
    fn bf_ubfiz_immr(&mut self, ty: Type, k: u64) -> Option<u8> {
        let bits = ty.bits();
        let rs: u64 = if bits <= 32 { 32 } else { 64 };
        // `then`, not `then_some`: a constant shift past the type width
        // (`x << 100`) must not compute `rs - k` at all (fuzz seed 20430).
        (k >= 1 && k < bits as u64).then(|| (rs - k) as u8)
    }

    /// `(x&m)<<k` → `ubfiz`: imms = w-1.
    fn bf_ubfiz_imms(&mut self, ty: Type, m: u64, k: u64) -> Option<u8> {
        let w = bf_low_w(ty.bits(), m)?;
        let s = k.checked_add(w as u64)?;
        (s <= ty.bits() as u64).then_some((w - 1) as u8)
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

#[cfg(test)]
mod immsynth_tests {
    use super::*;

    /// The movz/movn/movk baseline length for a normalized value, with
    /// logical-immediate synthesis disabled: what upstream cranelift would
    /// have emitted. The invariant of the whole transform is that no plan is
    /// ever longer than this.
    fn baseline_len(value: u64, size: OperandSize) -> usize {
        movwide_plan(value, size).len()
    }

    fn check(value: u64, size: OperandSize) -> Vec<SynthStep> {
        let plan = synth_plan(value, size);
        assert!(
            !plan.is_empty(),
            "empty plan for {value:#x} ({size:?})"
        );
        assert_eq!(
            value,
            eval_synth_steps(&plan),
            "plan for {value:#018x} ({size:?}) does not reproduce the value: {plan:?}"
        );
        assert!(
            plan.len() <= baseline_len(value, size),
            "plan for {value:#018x} ({size:?}) is longer than the movz/movn/movk baseline: {plan:?}"
        );
        plan
    }

    #[test]
    fn serde_json_swar_constants() {
        // The three constants of serde_json's skip_to_escape: LLVM
        // materializes each in two instructions.
        for value in [
            0x5c5c5c5c5c5c5c5cu64,
            0xfefefefefefefeffu64,
            0xdfdfdfdfdfdfdfe0u64,
        ] {
            let plan = check(value, OperandSize::Size64);
            assert!(
                plan.len() <= 2,
                "serde constant {value:#018x} should materialize in <= 2 instructions, got {plan:?}"
            );
        }
    }

    #[test]
    fn pattern_constants() {
        // (value, maximum acceptable plan length)
        let cases: &[(u64, usize)] = &[
            // Exact logical immediates: one instruction.
            (0x0101010101010101, 1),
            (0x8080808080808080, 1),
            (0x7f7f7f7f7f7f7f7f, 1),
            (0x2222222222222222, 1),
            (0xfefefefefefefefe, 1),
            (0xdfdfdfdfdfdfdfdf, 1),
            (0xfffffffffffffffe, 1),
            (0x00000000ffffffff, 1),
            // Two-instruction forms.
            (0x5c5c5c5c5c5c5c5c, 2), // orr | orr
            (0xfefefefefefefeff, 2), // orr + movk
            (0xdfdfdfdfdfdfdfe0, 2), // orr + movk
            // Repeated byte/halfword/deviating patterns must beat the
            // four-instruction slice sequence.
            (0x01010101010101ff, 2),
            (0xdeadbeefdeadbeef, 3), // identical halves: movz/movk + orr-shift
        ];
        for &(value, max_len) in cases {
            let plan = check(value, OperandSize::Size64);
            assert!(
                plan.len() <= max_len,
                "{value:#018x}: expected <= {max_len} instructions, got {}: {plan:?}",
                plan.len()
            );
        }
    }

    #[test]
    fn small_constants_unaffected() {
        for value in [
            0u64,
            1,
            42,
            0xffff,
            0x10000,
            0xffff_ffff,
            0x1_0000_0000,
            0xffff_0000_0000,
        ] {
            let size = if value >> 32 == 0 {
                OperandSize::Size32
            } else {
                OperandSize::Size64
            };
            let plan = check(value, size);
            assert_eq!(
                plan.len(),
                baseline_len(value, size),
                "small constant {value:#x} should keep the baseline sequence: {plan:?}"
            );
        }
    }

    #[test]
    fn random_u64_correctness_and_never_regress() {
        // xorshift64* PRNG, deterministic seed.
        let mut state = 0x243f6a8885a308d3u64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545f4914f6cdd1d)
        };
        let mut improved = 0usize;
        let mut total = 0usize;
        for _ in 0..200_000 {
            let value = next();
            let size = if value >> 32 == 0 {
                OperandSize::Size32
            } else {
                OperandSize::Size64
            };
            let plan = check(value, size);
            total += 1;
            if plan.len() < baseline_len(value, size) {
                improved += 1;
            }
        }
        // Random u64s almost never benefit; this is a regression-tripwire
        // for pathological behavior rather than a target. It is fine for
        // this to be zero.
        let _ = improved;
        let _ = total;
    }

    #[test]
    fn structured_u64_correctness() {
        // Patterned values: repeated bytes with deviations, runs, masks.
        let mut tested = 0usize;
        for byte in 0..=255u64 {
            let rep = byte
                .wrapping_mul(0x0101010101010101);
            // The plain repetition.
            if rep >> 32 != 0 {
                check(rep, OperandSize::Size64);
            } else {
                check(rep, OperandSize::Size32);
            }
            tested += 1;
            // Deviations in each byte position.
            for pos in 0..8u32 {
                let dev = rep ^ (0x5au64 << (pos * 8));
                check(dev, OperandSize::Size64);
                tested += 1;
            }
        }
        // Contiguous runs of ones at every position and length.
        for len in 1..64u32 {
            for pos in 0..=(64 - len) {
                let mask = if len == 64 {
                    u64::MAX
                } else {
                    ((1u64 << len) - 1) << pos
                };
                check(mask, OperandSize::Size64);
                check(!mask, OperandSize::Size64);
                tested += 2;
            }
        }
        // Random 32-bit values.
        let mut state = 0xdeadbeefu64;
        for _ in 0..20_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let value = state >> 32;
            check(value, OperandSize::Size32);
            tested += 1;
        }
        let _ = tested;
    }

    #[test]
    fn exact_logical_immediates_single_instruction() {
        // A selection of valid logical immediates of assorted element sizes:
        // each must materialize in exactly one instruction.
        for value in [
            0xffu64,
            0xff00,
            0xffff,
            0xffff0000ffff,
            0x1111111111111111,
            0x5555555555555555,
            0x3333333333333333,
            0xf0f0f0f0f0f0f0f0,
            0xff000000000000ff,
            0x8000000000000001,
        ] {
            let size = if value >> 32 == 0 {
                OperandSize::Size32
            } else {
                OperandSize::Size64
            };
            let plan = check(value, size);
            assert_eq!(
                plan.len(),
                1,
                "logical immediate {value:#018x} should be one instruction: {plan:?}"
            );
        }
    }
}

//! The A32 (ARM) encoder.
//!
//! Every A32 instruction is one little-endian word whose top four bits are the
//! condition, so the encoder builds a `u32` and hands it over. Fields that
//! depend on a symbol are left zero and filled in by a [`Fixup`] whose scatter
//! function knows where the bits go.

use super::imm;
use super::insn::{AL, Mnem};
use super::operand::{Index, Mem, MemOffset, Operand, OperandKind, Shift, ShiftAmt};
use super::reg::{self, Reg};
use super::{Insn, reloc};
use crate::arch::{AsmCtx, Literal};
use crate::expr::ExprRef;
use crate::section::{Fixup, FixupKind, LinkValue, Variant};
use crate::source::Span;

/// `nop` in A32: `mov r0, r0` would do, but the architectural hint is this.
pub const NOP: u32 = 0xe320_f000;

fn word(cond: u8, bits: u32) -> u32 {
    ((cond as u32) << 28) | bits
}

fn one(w: u32) -> Vec<Variant> {
    vec![Variant::new(w.to_le_bytes().to_vec())]
}

// ---- shared operand helpers ------------------------------------------------

/// Checks the operand count, reporting the accepted arities on failure.
pub fn arity(cx: &mut AsmCtx<'_>, ins: &Insn<'_>, allowed: &[usize]) -> Option<()> {
    if allowed.contains(&ins.ops.len()) {
        return Some(());
    }
    let want: Vec<String> = allowed.iter().map(|n| n.to_string()).collect();
    cx.error(
        ins.span,
        format!(
            "`{}` takes {} operand(s), but {} were given",
            ins.text,
            want.join(" or "),
            ins.ops.len()
        ),
    );
    None
}

pub fn reg_of(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<Reg> {
    match op.reg() {
        Some(r) => Some(r),
        None => {
            cx.error(
                op.span,
                format!("expected a register, found {}", op.describe()),
            );
            None
        }
    }
}

/// The constant value of an immediate operand, which must not be symbolic.
pub fn imm_of(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<i64> {
    let Some(e) = op.imm() else {
        cx.error(
            op.span,
            format!("expected an immediate, found {}", op.describe()),
        );
        return None;
    };
    match cx.constant(e) {
        Some(v) => Some(v),
        None => {
            cx.error(op.span, "this immediate must be a constant expression");
            None
        }
    }
}

/// A 32-bit immediate, accepting either the signed or the unsigned reading
/// (`-1` and `0xffffffff` are the same word).
/// A 32-bit immediate. Both references take the low 32 bits of whatever the
/// expression came to, so `-1`, `0xffffffff` and `0x1ffffffff` are one word.
pub fn imm32(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<u32> {
    Some(imm_of(cx, op)? as u32)
}

/// An unsigned immediate that must fit in `bits` bits.
pub fn imm_bits(cx: &mut AsmCtx<'_>, op: &Operand, bits: u32) -> Option<u32> {
    let v = imm_of(cx, op)?;
    let max = if bits >= 32 {
        u32::MAX as i64
    } else {
        (1i64 << bits) - 1
    };
    if v < 0 || v > max {
        cx.error(
            op.span,
            format!("immediate {v} is out of range (0 to {max})"),
        );
        return None;
    }
    Some(v as u32)
}

/// Rejects a condition on an instruction whose encoding has none: the
/// condition field of `bkpt`, the barriers and `blx <label>` is part of the
/// opcode.
fn no_cond(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<()> {
    if ins.cond_written && ins.cond != AL {
        cx.error(ins.span, format!("`{}` cannot be conditional", ins.text));
        return None;
    }
    Some(())
}

/// GNU as's `RRnpc` operand kind: a register that may not be the PC.
pub fn no_pc(cx: &mut AsmCtx<'_>, span: Span, r: Reg) -> Option<()> {
    if r == reg::PC {
        cx.error(span, "`pc` is not allowed here");
        return None;
    }
    Some(())
}

pub fn no_flags(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<()> {
    if ins.set_flags {
        cx.error(ins.span, format!("`{}` cannot set the flags", ins.text));
        return None;
    }
    Some(())
}

// ---- fixups ----------------------------------------------------------------

/// `b`/`bl`: a 24-bit field counting words, with the PC two instructions
/// ahead of the branch.
fn scatter_branch(w: u64, v: i64) -> u64 {
    (w & 0xff00_0000) | (((v >> 2) as u64) & 0x00ff_ffff)
}

/// `blx <label>`: as `bl`, plus bit 24 carrying the odd halfword so the
/// ARM-to-Thumb call can land on a 2-byte boundary.
fn scatter_blx(w: u64, v: i64) -> u64 {
    (w & 0xfe00_0000) | ((((v >> 1) as u64) & 1) << 24) | (((v >> 2) as u64) & 0x00ff_ffff)
}

fn branch_variant(w: u32, expr: ExprRef, kind: FixupKind, span: Span) -> Vec<Variant> {
    vec![Variant {
        bytes: w.to_le_bytes().to_vec(),
        fixups: vec![Fixup {
            offset: 0,
            expr,
            kind,
            span,
        }],
    }]
}

// ---- entry point -----------------------------------------------------------

pub fn assemble(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    use Mnem::*;
    let ops = ins.ops;
    if ins.width != super::insn::Width::Any {
        cx.error(ins.span, "width suffixes are invalid in ARM mode");
        return None;
    }
    match ins.mnem {
        And | Eor | Sub | Rsb | Add | Adc | Sbc | Rsc | Tst | Teq | Cmp | Cmn | Orr | Mov | Bic
        | Mvn => data_processing(cx, ins),
        Lsl | Lsr | Asr | Ror | Rrx => shift_insn(cx, ins),
        Ldr | Str | Ldrb | Strb | Ldrt | Strt | Ldrbt | Strbt => load_store(cx, ins),
        Ldrh | Strh | Ldrsb | Ldrsh | Ldrht | Strht | Ldrsbt | Ldrsht | Ldrd | Strd => {
            load_store_extra(cx, ins)
        }
        Pld | Pldw | Pli => preload(cx, ins),
        Ldm(_) | Stm(_) | Push | Pop => block_transfer(cx, ins),
        Adr | Adrl => adr(cx, ins),
        B | Bl | Bx | Blx => branch(cx, ins),
        Mul | Mla | Mls | Umull | Umlal | Smull | Smlal => multiply(cx, ins),
        Movw | Movt => move_wide(cx, ins),
        // `neg rd, rm` is `rsb rd, rm, #0` written short.
        Neg => {
            arity(cx, ins, &[2])?;
            let rd = reg_of(cx, &ops[0])? as u32;
            let rn = reg_of(cx, &ops[1])? as u32;
            let s = u32::from(ins.set_flags);
            Some(one(word(
                ins.cond,
                0x0260_0000 | (s << 20) | (rn << 16) | (rd << 12),
            )))
        }
        // Thumb-only spellings.
        Orn | Addw | Subw | Cbz | Cbnz => {
            cx.error(
                ins.span,
                format!(
                    "`{}` is a Thumb instruction; ARM has no such form",
                    ins.text
                ),
            );
            None
        }
        Mrs => status_read(cx, ins),
        Msr => status_write(cx, ins),
        Ext(at) => super::generic::assemble(cx, ins, at),
        // ARM instructions carry their own conditions, so GNU as takes an
        // `it` in ARM code for source shared with Thumb, and emits nothing.
        It(_) => {
            arity(cx, ins, &[1])?;
            if ops[0]
                .word
                .as_deref()
                .and_then(super::insn::condition)
                .is_none()
            {
                cx.error(ops[0].span, "expected a condition code");
            }
            None
        }
    }
}

// ---- data processing -------------------------------------------------------

/// Builds the twelve-bit second operand and, for immediates that only encode
/// with the complementary operation, the opcode that has to replace the one
/// the source wrote.
/// The opcode `operand2` returns where it found a constant only `movw` can
/// hold; `data_processing` writes the instruction out itself.
const MOVW_INSTEAD: u32 = 0xff;

fn operand2(cx: &mut AsmCtx<'_>, ins: &Insn<'_>, op: &Operand) -> Option<(u32, u32, u32)> {
    let base = ins.mnem.dp_opcode()?;
    match &op.kind {
        OperandKind::Reg(rm) => Some((base, 0, *rm as u32)),
        OperandKind::Shifted { rm, shift, amount } => {
            Some((base, 0, shift_field(*rm, *shift, *amount)))
        }
        OperandKind::Imm(_) => {
            let v = imm32(cx, op)?;
            if let Some(field) = imm::modified(v) {
                return Some((base, 1, field));
            }
            // `add rd, rn, #-1` has no encoding, but `sub rd, rn, #1` is the
            // same instruction; every ARM assembler makes the swap.
            if let Some((partner, negate)) = ins.mnem.immediate_partner() {
                let alt = if negate { v.wrapping_neg() } else { !v };
                if let Some(field) = imm::modified(alt)
                    && let Some(op) = partner.dp_opcode()
                {
                    return Some((op, 1, field));
                }
            }
            // `mov rd, #imm` reaches any 16-bit constant through `movw`,
            // which has no flag-setting form.
            if ins.mnem == Mnem::Mov && !ins.set_flags && v <= 0xffff {
                return Some((MOVW_INSTEAD, (v >> 12) & 0xf, v & 0xfff));
            }
            cx.error(
                op.span,
                format!(
                    "{} (0x{v:08x}) is not an ARM modified immediate: it must be an \
                     8-bit value rotated right by an even amount",
                    v as i32
                ),
            );
            None
        }
        _ => {
            cx.error(
                op.span,
                format!("expected a register or immediate, found {}", op.describe()),
            );
            None
        }
    }
}

/// The shifter field of a register operand: `shift_imm:type:0:Rm` or
/// `Rs:0:type:1:Rm`.
fn shift_field(rm: Reg, shift: Shift, amount: ShiftAmt) -> u32 {
    let rm = rm as u32;
    match amount {
        // `rrx` is `ror` by zero; a real `ror #0` has no encoding.
        ShiftAmt::None => (3 << 5) | rm,
        ShiftAmt::Reg(rs) => ((rs as u32) << 8) | (shift.code() << 5) | (1 << 4) | rm,
        ShiftAmt::Imm(n) => {
            // `md_apply_fix`: a shift of zero is written as `lsl`, whichever
            // kind the source names, and `lsr #32` and `asr #32` are spelled
            // with a zero amount -- which is why `lsr #0` cannot mean 32.
            let (kind, n) = match shift {
                _ if n == 0 => (Shift::Lsl, 0),
                Shift::Lsr | Shift::Asr if n == 32 => (shift, 0),
                _ => (shift, n),
            };
            (n << 7) | (kind.code() << 5) | rm
        }
    }
}

/// The two-operand spelling of a three-operand instruction leaves out the
/// first source, not the shift: `add r0, r1, lsl #3` is what GNU as calls
/// garbage following the instruction.
pub fn no_shorthand_shift(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<()> {
    if matches!(op.kind, OperandKind::Shifted { .. }) {
        cx.error(
            op.span,
            "a shifted operand needs all three registers written out",
        );
        return None;
    }
    Some(())
}

fn data_processing(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let ops = ins.ops;
    let m = ins.mnem;
    let (rd, rn, src) = if m.is_compare() {
        arity(cx, ins, &[2])?;
        (0u32, reg_of(cx, &ops[0])? as u32, &ops[1])
    } else if m.is_move() {
        arity(cx, ins, &[2])?;
        (reg_of(cx, &ops[0])? as u32, 0u32, &ops[1])
    } else {
        arity(cx, ins, &[2, 3])?;
        let rd = reg_of(cx, &ops[0])? as u32;
        if ops.len() == 2 {
            // `add r0, r1` is shorthand for `add r0, r0, r1`. The shorthand
            // has no room for a shift: the second operand fills the slot
            // GNU as gives the first source.
            no_shorthand_shift(cx, &ops[1])?;
            (rd, rd, &ops[1])
        } else {
            (rd, reg_of(cx, &ops[1])? as u32, &ops[2])
        }
    };
    let (opcode, i, field) = operand2(cx, ins, src)?;
    if opcode == MOVW_INSTEAD {
        return Some(one(word(
            ins.cond,
            0x0300_0000 | (i << 16) | (rd << 12) | field,
        )));
    }
    // The comparisons have no S bit of their own: they always set the flags.
    let s = if m.is_compare() || ins.set_flags {
        1
    } else {
        0
    };
    Some(one(word(
        ins.cond,
        (i << 25) | (opcode << 21) | (s << 20) | (rn << 16) | (rd << 12) | field,
    )))
}

/// `lsl`/`lsr`/`asr`/`ror`/`rrx`, which A32 encodes as `mov` with a shift.
fn shift_insn(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let ops = ins.ops;
    let shift = match ins.mnem {
        Mnem::Lsl => Shift::Lsl,
        Mnem::Lsr => Shift::Lsr,
        Mnem::Asr => Shift::Asr,
        Mnem::Ror => Shift::Ror,
        _ => Shift::Rrx,
    };
    let (rd, rm, amount) = if shift == Shift::Rrx {
        arity(cx, ins, &[2])?;
        (reg_of(cx, &ops[0])?, reg_of(cx, &ops[1])?, ShiftAmt::None)
    } else {
        arity(cx, ins, &[2, 3])?;
        let rd = reg_of(cx, &ops[0])?;
        // `lsl r0, #3` shifts the destination in place.
        let (rm, amt) = if ops.len() == 2 {
            (rd, &ops[1])
        } else {
            (reg_of(cx, &ops[1])?, &ops[2])
        };
        let amount = match amt.reg() {
            Some(rs) => ShiftAmt::Reg(rs),
            None => {
                let v = imm_of(cx, amt)?;
                let max = match shift {
                    Shift::Lsr | Shift::Asr => 32,
                    _ => 31,
                };
                if v < 0 || v > max {
                    cx.error(
                        amt.span,
                        format!("shift amount {v} is out of range (0 to {max})"),
                    );
                    return None;
                }
                ShiftAmt::Imm(v as u32)
            }
        };
        (rd, rm, amount)
    };
    let field = shift_field(rm, shift, amount);
    let s = u32::from(ins.set_flags);
    Some(one(word(
        ins.cond,
        (13 << 21) | (s << 20) | ((rd as u32) << 12) | field,
    )))
}

// ---- loads and stores ------------------------------------------------------

/// P and W, which together say whether and when the base register is updated.
fn index_bits(index: Index) -> (u32, u32) {
    match index {
        Index::Offset => (1, 0),
        Index::PreIndex => (1, 1),
        Index::PostIndex => (0, 0),
    }
}

fn memory_operand<'a>(cx: &mut AsmCtx<'_>, op: &'a Operand) -> Option<&'a Mem> {
    match &op.kind {
        OperandKind::Mem(m) if m.base == reg::PC && m.index != Index::Offset => {
            cx.error(m.span, "a PC-relative address cannot write `pc` back");
            None
        }
        // `encode_arm_addr_mode_2` and `_3`: the register an address is
        // indexed by is never the program counter.
        OperandKind::Mem(m) if matches!(m.offset, MemOffset::Reg { rm, .. } if rm == reg::PC) => {
            cx.error(m.span, "`pc` cannot be an index register");
            None
        }
        OperandKind::Mem(m) => Some(m),
        _ => {
            cx.error(
                op.span,
                format!("expected a memory operand, found {}", op.describe()),
            );
            None
        }
    }
}

/// The value of `ldr rt, =expr` when it is a number, which GNU as loads with
/// a `mov` or `mvn` instead where one can hold it, and otherwise with a load
/// from the literal pool; `None` for a value the pool holds as an expression.
///
/// Either way the number has to fit in the word the pool would hold.
pub fn literal_constant(cx: &mut AsmCtx<'_>, op: &Operand, e: ExprRef) -> Option<Option<u32>> {
    let Some(v) = cx.constant(e) else {
        return Some(None);
    };
    if !(i32::MIN as i64..=u32::MAX as i64).contains(&v) {
        cx.error(
            op.span,
            format!("{v} does not fit in the 32-bit word of a literal pool entry"),
        );
        return None;
    }
    Some(Some(v as u32))
}

/// The error for an `=expr` operand on anything but `ldr`.
pub fn literal_only_for_ldr(cx: &mut AsmCtx<'_>, ins: &Insn<'_>, op: &Operand) -> Option<()> {
    if ins.mnem == Mnem::Ldr {
        return Some(());
    }
    cx.error(
        op.span,
        format!(
            "`{}` cannot load from a literal pool; only `ldr` can",
            ins.text
        ),
    );
    None
}

/// A PC-relative load's 12-bit offset and its U bit. An offset of zero keeps
/// the U bit it was assembled with, which for a literal load is clear: GNU as
/// writes `ldr r0, [pc, #-0]`.
fn scatter_literal(w: u64, v: i64) -> u64 {
    if v == 0 {
        return w & !0xfff;
    }
    let up = if v > 0 { 0x0080_0000 } else { 0 };
    (w & !0x0080_0fff) | up | (v.unsigned_abs() & 0xfff)
}

/// `ldr rt, =expr` in A32.
fn literal_load(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    rt: u32,
    op: &Operand,
    e: ExprRef,
) -> Option<Vec<Variant>> {
    literal_only_for_ldr(cx, ins, op)?;
    let constant = literal_constant(cx, op, e)?;
    if let Some(v) = constant {
        if let Some(field) = imm::modified(v) {
            return Some(one(word(ins.cond, 0x03a0_0000 | (rt << 12) | field)));
        }
        if let Some(field) = imm::modified(!v) {
            return Some(one(word(ins.cond, 0x03e0_0000 | (rt << 12) | field)));
        }
    }
    let value = match cx.constant(e) {
        Some(v) if constant.is_some() => Literal::Const(v),
        _ => Literal::Expr(e),
    };
    let entry = cx.literal(value, 4, op.span);
    // The PC reads two instructions ahead, and the load reaches 4095 bytes
    // either way from there.
    let kind = FixupKind::pcrel(4, 8)
        .with_limits(-4095, 4095)
        .with_range_hint("the literal pool is too far away; put an `.ltorg` nearer")
        .scatter(scatter_literal);
    Some(vec![Variant {
        bytes: word(ins.cond, 0x051f_0000 | (rt << 12))
            .to_le_bytes()
            .to_vec(),
        fixups: vec![Fixup {
            offset: 0,
            expr: entry,
            kind,
            span: op.span,
        }],
    }])
}

/// P and W for a transfer made with user-mode privileges, which is
/// post-indexed however it was written; `[rn]` with no offset is the way to
/// spell one that does not move the base.
fn translate_bits(cx: &mut AsmCtx<'_>, mem: &Mem) -> Option<(u32, u32)> {
    if mem.index == Index::PostIndex
        || (mem.index == Index::Offset && matches!(mem.offset, MemOffset::None))
    {
        return Some((0, 1));
    }
    cx.error(
        mem.span,
        "an unprivileged transfer is post-indexed: write `[rn], #off`",
    );
    None
}

fn load_store(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let t = ins.mnem.transfer()?;
    no_flags(cx, ins)?;
    arity(cx, ins, &[2])?;
    let rt = reg_of(cx, &ins.ops[0])? as u32;
    if let OperandKind::Literal(e) = ins.ops[1].kind {
        return literal_load(cx, ins, rt, &ins.ops[1], e);
    }
    let mem = *memory_operand(cx, &ins.ops[1])?;
    // Only a word transfer reaches the PC, and of the unprivileged ones
    // only the store: GNU as gives `strt` the register kind that allows it.
    if t.size != 4 || (t.translate && t.load) {
        no_pc(cx, ins.ops[0].span, rt as Reg)?;
    }
    let (p, w) = if t.translate {
        translate_bits(cx, &mem)?
    } else {
        index_bits(mem.index)
    };
    let l = u32::from(t.load);
    let b = u32::from(t.size == 1);
    let (i, u, field) = match mem.offset {
        MemOffset::None => (0, 1, 0),
        MemOffset::Unindexed(_) => {
            cx.error(mem.span, "only `ldc` and `stc` take `[rn], {option}`");
            return None;
        }
        MemOffset::Imm(v) => {
            let mag = v.unsigned_abs();
            if mag > 0xfff {
                cx.error(
                    mem.span,
                    format!("offset {v} does not fit in the 12-bit field (-4095 to 4095)"),
                );
                return None;
            }
            (0, u32::from(v >= 0), mag as u32)
        }
        MemOffset::Reg {
            rm,
            add,
            shift,
            amount,
            ..
        } => (
            1,
            u32::from(add),
            shift_field(rm, shift, ShiftAmt::Imm(amount)),
        ),
    };
    Some(one(word(
        ins.cond,
        (1 << 26)
            | (i << 25)
            | (p << 24)
            | (u << 23)
            | (b << 22)
            | (w << 21)
            | (l << 20)
            | ((mem.base as u32) << 16)
            | (rt << 12)
            | field,
    )))
}

/// The halfword, signed and doubleword transfers, which predate the main
/// load encoding and were squeezed into a gap in the data-processing space:
/// their offset is split into two nibbles around a `1SH1` marker, and their
/// index register cannot be scaled.
fn load_store_extra(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let t = ins.mnem.transfer()?;
    no_flags(cx, ins)?;
    let dual = t.size == 8;
    // `ldrd rt, rt2, [rn]` names both halves of the pair, and GNU as takes
    // the spelling that leaves the second out.
    if dual {
        arity(cx, ins, &[2, 3])?;
    } else {
        arity(cx, ins, &[2])?;
    }
    let rt = reg_of(cx, &ins.ops[0])? as u32;
    let mut mem_at = 1;
    if dual {
        if !rt.is_multiple_of(2) {
            cx.error(ins.ops[0].span, "the first transfer register must be even");
            return None;
        }
        if rt == 14 {
            cx.error(ins.ops[0].span, "`lr` would pair with `pc`");
            return None;
        }
        if ins.ops.len() == 3 {
            let rt2 = reg_of(cx, &ins.ops[1])? as u32;
            if rt2 != rt + 1 {
                cx.error(
                    ins.ops[1].span,
                    format!(
                        "the second transfer register must be `{}`",
                        reg::name_of(rt as u8 + 1)
                    ),
                );
                return None;
            }
            mem_at = 2;
        }
    }
    no_pc(cx, ins.ops[0].span, rt as Reg)?;
    let mem = *memory_operand(cx, &ins.ops[mem_at])?;
    let (p, w) = if t.translate {
        translate_bits(cx, &mem)?
    } else {
        index_bits(mem.index)
    };
    let (l, sh) = match (t.load, t.size, t.signed) {
        (false, 2, _) => (0, 0b01),
        (true, 2, false) => (1, 0b01),
        (true, 1, true) => (1, 0b10),
        (true, 2, true) => (1, 0b11),
        // The doubleword pair is a store with the two signed codes.
        (true, 8, _) => (0, 0b10),
        _ => (0, 0b11),
    };
    let (i, u, field) = match mem.offset {
        MemOffset::None => (1, 1, 0),
        MemOffset::Unindexed(_) => {
            cx.error(mem.span, "only `ldc` and `stc` take `[rn], {option}`");
            return None;
        }
        MemOffset::Imm(v) => {
            let mag = v.unsigned_abs();
            if mag > 0xff {
                cx.error(
                    mem.span,
                    format!("offset {v} does not fit in the 8-bit field (-255 to 255)"),
                );
                return None;
            }
            let mag = mag as u32;
            (1, u32::from(v >= 0), ((mag & 0xf0) << 4) | (mag & 0xf))
        }
        MemOffset::Reg {
            rm,
            add,
            shift,
            amount,
            ..
        } => {
            if amount != 0 || shift != Shift::Lsl {
                cx.error(
                    mem.span,
                    "a halfword, signed or doubleword transfer cannot scale its \
                     index register",
                );
                return None;
            }
            (0, u32::from(add), rm as u32)
        }
    };
    Some(one(word(
        ins.cond,
        (p << 24)
            | (u << 23)
            | (i << 22)
            | (w << 21)
            | (l << 20)
            | ((mem.base as u32) << 16)
            | (rt << 12)
            | (1 << 7)
            | (sh << 5)
            | (1 << 4)
            | field,
    )))
}

/// `pld`, `pldw` and `pli`, which address memory but load nothing: they are
/// unconditional, and their transfer register field is all ones.
fn preload(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    no_flags(cx, ins)?;
    no_cond(cx, ins)?;
    arity(cx, ins, &[1])?;
    let mem = *memory_operand(cx, &ins.ops[0])?;
    if mem.index != Index::Offset {
        cx.error(mem.span, "a preload does not write its base register back");
        return None;
    }
    // The base has P set and W clear already; only U and the register bit
    // are left to the address.
    let base: u32 = match ins.mnem {
        Mnem::Pld => 0xf550_f000,
        Mnem::Pldw => 0xf510_f000,
        _ => 0xf450_f000,
    };
    let (i, u, field) = match mem.offset {
        MemOffset::None => (0, 1, 0),
        MemOffset::Imm(v) => {
            let mag = v.unsigned_abs();
            if mag > 0xfff {
                cx.error(
                    mem.span,
                    format!("offset {v} does not fit in the 12-bit field (-4095 to 4095)"),
                );
                return None;
            }
            (0, u32::from(v >= 0), mag as u32)
        }
        MemOffset::Reg {
            rm,
            add,
            shift,
            amount,
            ..
        } => (
            1,
            u32::from(add),
            shift_field(rm, shift, ShiftAmt::Imm(amount)),
        ),
        MemOffset::Unindexed(_) => {
            cx.error(mem.span, "only `ldc` and `stc` take `[rn], {option}`");
            return None;
        }
    };
    Some(one(base
        | (i << 25)
        | (u << 23)
        | ((mem.base as u32) << 16)
        | field))
}

fn register_list(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<(u16, bool)> {
    match op.kind {
        OperandKind::List { mask, user } => Some((mask, user)),
        _ => {
            cx.error(
                op.span,
                format!("expected a register list, found {}", op.describe()),
            );
            None
        }
    }
}

/// The single register of a one-element list, if that is what this is.
fn sole_register(list: u16) -> Option<u32> {
    (list.count_ones() == 1).then(|| list.trailing_zeros())
}

/// The register list of a `push` or `pop`, which GNU as says in so many
/// words has no `^` form.
fn no_user_bank(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<u16> {
    let (list, user) = register_list(cx, &ins.ops[0])?;
    if user {
        cx.error(
            ins.ops[0].span,
            format!("`{}` does not take a `^` register list", ins.text),
        );
        return None;
    }
    Some(list)
}

fn block_transfer(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    no_flags(cx, ins)?;
    // `push`/`pop` are `stmdb sp!` / `ldmia sp!` under another name.
    let (rn, writeback, mode, load, list, user) = match ins.mnem {
        Mnem::Push => {
            arity(cx, ins, &[1])?;
            let list = no_user_bank(cx, ins)?;
            // A one-register push is a plain pre-indexed store, which is one
            // cycle cheaper; GNU as and LLVM both rewrite it that way.
            if let Some(rt) = sole_register(list) {
                return Some(one(word(ins.cond, 0x052d_0004 | (rt << 12))));
            }
            (reg::SP, true, (true, false), false, list, false)
        }
        Mnem::Pop => {
            arity(cx, ins, &[1])?;
            let list = no_user_bank(cx, ins)?;
            if let Some(rt) = sole_register(list) {
                return Some(one(word(ins.cond, 0x049d_0004 | (rt << 12))));
            }
            (reg::SP, true, (false, true), true, list, false)
        }
        Mnem::Ldm(m) | Mnem::Stm(m) => {
            arity(cx, ins, &[2])?;
            let rn = reg_of(cx, &ins.ops[0])?;
            no_pc(cx, ins.ops[0].span, rn)?;
            let (list, user) = register_list(cx, &ins.ops[1])?;
            (
                rn,
                ins.ops[0].writeback,
                (m.before, m.increment),
                matches!(ins.mnem, Mnem::Ldm(_)),
                list,
                user,
            )
        }
        _ => return None,
    };
    Some(one(word(
        ins.cond,
        (1 << 27)
            | (u32::from(mode.0) << 24)
            | (u32::from(mode.1) << 23)
            | (u32::from(user) << 22)
            | (u32::from(writeback) << 21)
            | (u32::from(load) << 20)
            | ((rn as u32) << 16)
            | list as u32,
    )))
}

// ---- adr and adrl ------------------------------------------------------------

/// `add rd, pc, #imm` or `sub rd, pc, #imm` for a PC-relative value, as its
/// data-processing opcode bits and immediate field: GNU as's
/// `encode_arm_immediate`, then the negated value with the opposite
/// operation.
fn adr_one(v: i64) -> Option<(u32, u32)> {
    const ADD: u32 = 0x0080_0000;
    const SUB: u32 = 0x0040_0000;
    let v = v as u32;
    if (v as i32) >= 0
        && let Some(field) = imm::modified(v)
    {
        return Some((ADD, field));
    }
    imm::modified(v.wrapping_neg()).map(|field| (SUB, field))
}

/// GNU as's `validate_immediate_twopart`: `v` as the sum of two modified
/// immediates, the low one first.
fn adr_two(v: u32) -> Option<(u32, u32)> {
    for i in (0..32).step_by(2) {
        let a = v.rotate_left(i);
        if a & 0xff == 0 {
            continue;
        }
        let high = if a & 0xff00 != 0 {
            if a & !0xffff != 0 {
                continue;
            }
            (a >> 8) | ((i + 24) << 7)
        } else if a & 0x00ff_0000 != 0 {
            if a & 0xff00_0000 != 0 {
                continue;
            }
            (a >> 16) | ((i + 16) << 7)
        } else {
            (a >> 24) | ((i + 8) << 7)
        };
        return Some(((a & 0xff) | (i << 7), high));
    }
    None
}

fn adr_reaches(v: i64) -> bool {
    adr_one(v).is_some()
}

fn adrl_reaches(v: i64) -> bool {
    adr_one(v).is_some()
        || adr_two(v as u32).is_some()
        || adr_two((v as u32).wrapping_neg()).is_some()
}

/// `adr`: one `add` or `sub` from the PC.
fn scatter_adr(w: u64, v: i64) -> u64 {
    let (op, field) = adr_one(v).unwrap_or((0, 0));
    (w & 0xf000_f000) | 0x020f_0000 | op as u64 | field as u64
}

/// `adrl`: the `add` or `sub` of `adr` followed by a no-op where one
/// instruction reaches, and otherwise two, the second adding to (or
/// subtracting from) the register the first set. The field is both words,
/// the first in the low half.
fn scatter_adrl(w: u64, v: i64) -> u64 {
    let first = w & 0xf000_f000;
    let rd = (first >> 12) & 0xf;
    let (low, high) = match adr_one(v) {
        Some((op, field)) => (first | 0x020f_0000 | op as u64 | field as u64, 0xe1a0_0000),
        None => {
            let (op, (lo, hi)) = match adr_two(v as u32) {
                Some(parts) => (0x0080_0000, parts),
                None => (
                    0x0040_0000,
                    adr_two((v as u32).wrapping_neg()).unwrap_or((0, 0)),
                ),
            };
            let insn = first | 0x0200_0000 | op;
            (
                insn | 0x000f_0000 | lo as u64,
                insn | (rd << 16) | hi as u64,
            )
        }
    };
    low | (high << 32)
}

/// `adr rd, label` and `adrl rd, label`, which GNU as resolves within the
/// section and refuses to relocate.
fn adr(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    no_flags(cx, ins)?;
    arity(cx, ins, &[2])?;
    let rd = reg_of(cx, &ins.ops[0])? as u32;
    let Some(e) = ins.ops[1].imm() else {
        cx.error(ins.ops[1].span, "expected a label");
        return None;
    };
    // GNU as sets the low bit for a Thumb function only when assembling for
    // interworking (`-mthumb-interwork`), which rsasm has no option for.
    let long = ins.mnem == Mnem::Adrl;
    let (size, reaches, what): (u8, fn(i64) -> bool, _) = if long {
        (
            8,
            adrl_reaches,
            "`adrl` reaches this far only with an address two `add`s can build",
        )
    } else {
        (
            4,
            adr_reaches,
            "`adr` reaches only an 8-bit value rotated by an even amount; try `adrl`",
        )
    };
    let kind = FixupKind::pcrel(size, 8)
        .accepting(reaches)
        .with_range_hint(what)
        .scatter(if long { scatter_adrl } else { scatter_adr });
    let w = word(ins.cond, rd << 12) as u64;
    let bytes = if long {
        (w | (0xe1a0_0000 << 32)).to_le_bytes().to_vec()
    } else {
        (w as u32).to_le_bytes().to_vec()
    };
    Some(vec![Variant {
        bytes,
        fixups: vec![Fixup {
            offset: 0,
            expr: e,
            kind,
            span: ins.ops[1].span,
        }],
    }])
}

// ---- branches --------------------------------------------------------------

fn branch(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    no_flags(cx, ins)?;
    arity(cx, ins, &[1])?;
    let op = &ins.ops[0];
    match ins.mnem {
        Mnem::Bx | Mnem::Blx if op.reg().is_some() => {
            let rm = reg_of(cx, op)? as u32;
            let sub = if ins.mnem == Mnem::Bx { 1 } else { 3 };
            Some(one(word(ins.cond, 0x012f_ff00 | (sub << 4) | rm)))
        }
        Mnem::Bx => {
            cx.error(op.span, "`bx` takes a register");
            None
        }
        Mnem::Blx => {
            // The label form of `blx` swaps to Thumb state, so it is
            // unconditional and its target is halfword-aligned.
            no_cond(cx, ins)?;
            let Some(e) = op.imm() else {
                cx.error(op.span, "expected a label or register");
                return None;
            };
            Some(branch_variant(0xfa00_0000, e, blx_kind(), ins.span))
        }
        _ => {
            let Some(e) = op.imm() else {
                cx.error(op.span, "expected a branch target");
                return None;
            };
            let link = ins.mnem == Mnem::Bl;
            // Only an unconditional `bl` is a call to the linker, which may
            // make it a `blx`; a conditional one cannot change state, and is
            // relocated as a jump.
            let call = link && ins.cond == AL;
            let kind = if call { bl_kind() } else { jump_kind() };
            let w = word(ins.cond, if link { 0x0b00_0000 } else { 0x0a00_0000 });
            Some(branch_variant(w, e, kind, ins.span))
        }
    }
}

/// `bl label`. The 24-bit field counts words, and the PC an instruction
/// reads is two instructions ahead of itself: hence the +8 adjustment.
pub fn bl_kind() -> FixupKind {
    FixupKind::pcrel(4, 8)
        .with_field(26, 4)
        .with_reloc(reloc::CALL)
        .link(LinkValue::Interwork(super::IW_ARM_BL))
        .scatter(scatter_branch)
}

/// `blx label`, which swaps to Thumb state, so its target is a halfword
/// boundary.
pub fn blx_kind() -> FixupKind {
    FixupKind::pcrel(4, 8)
        .with_field(26, 2)
        .with_reloc(reloc::CALL)
        .link(LinkValue::Interwork(super::IW_ARM_BLX))
        .scatter(scatter_blx)
}

/// `b label`, `b<cond> label` and `bl<cond> label`.
fn jump_kind() -> FixupKind {
    FixupKind::pcrel(4, 8)
        .with_field(26, 4)
        .with_reloc(reloc::JUMP24)
        .link(LinkValue::Interwork(super::IW_ARM_JUMP))
        .scatter(scatter_branch)
}

/// `bl` rewritten as `blx`, for a call into Thumb.
pub fn to_blx(w: u64) -> u64 {
    (w & 0x00ff_ffff) | 0xfa00_0000
}

/// `blx` rewritten as `bl`, for a call that stays in ARM.
pub fn to_bl(w: u64) -> u64 {
    (w & 0x00ff_ffff) | 0xeb00_0000
}

/// Thumb `adr` of a Thumb function sets the address's low bit, as GNU as
/// does where it already knows the label is one when it reads the `adr`.
pub fn thumb_function_address(cx: &mut AsmCtx<'_>, e: ExprRef) -> ExprRef {
    let v = crate::expr::SymbolEnv::new(cx.exprs, cx.symbols).value(e);
    let Some(crate::expr::Value {
        plus: Some(p),
        minus: None,
        ..
    }) = v
    else {
        return e;
    };
    let sym = cx.symbols.get(p);
    if !sym.is_defined() || !super::thumb_is_func(sym.target_flags, sym.ty) {
        return e;
    }
    let span = cx.exprs.span(e);
    let one = cx.exprs.int(1, span);
    cx.exprs.alloc(
        crate::expr::ExprKind::Binary(crate::expr::BinOp::Add, e, one),
        span,
    )
}

// ---- multiply --------------------------------------------------------------

fn multiply(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    let ops = ins.ops;
    let s = u32::from(ins.set_flags);
    for op in ops {
        if let Some(r) = op.reg() {
            no_pc(cx, op.span, r)?;
        }
    }
    let w = match ins.mnem {
        Mnem::Mul => {
            // `mul rd, rn` multiplies the destination by the source.
            arity(cx, ins, &[2, 3])?;
            let rd = reg_of(cx, &ops[0])? as u32;
            let rn = reg_of(cx, &ops[1])? as u32;
            let rm = if ops.len() == 3 {
                reg_of(cx, &ops[2])? as u32
            } else {
                rd
            };
            (s << 20) | (rd << 16) | (rm << 8) | (9 << 4) | rn
        }
        Mnem::Mla | Mnem::Mls => {
            arity(cx, ins, &[4])?;
            let (rd, rn, rm, ra) = (
                reg_of(cx, &ops[0])? as u32,
                reg_of(cx, &ops[1])? as u32,
                reg_of(cx, &ops[2])? as u32,
                reg_of(cx, &ops[3])? as u32,
            );
            if ins.mnem == Mnem::Mls {
                no_flags(cx, ins)?;
                (3 << 21) | (rd << 16) | (ra << 12) | (rm << 8) | (9 << 4) | rn
            } else {
                (1 << 21) | (s << 20) | (rd << 16) | (ra << 12) | (rm << 8) | (9 << 4) | rn
            }
        }
        _ => {
            arity(cx, ins, &[4])?;
            let (lo, hi, rn, rm) = (
                reg_of(cx, &ops[0])? as u32,
                reg_of(cx, &ops[1])? as u32,
                reg_of(cx, &ops[2])? as u32,
                reg_of(cx, &ops[3])? as u32,
            );
            let op = match ins.mnem {
                Mnem::Umull => 4,
                Mnem::Umlal => 5,
                Mnem::Smull => 6,
                _ => 7,
            };
            (op << 21) | (s << 20) | (hi << 16) | (lo << 12) | (rm << 8) | (9 << 4) | rn
        }
    };
    Some(one(word(ins.cond, w)))
}

// ---- movw / movt and the small unary operations ----------------------------

fn move_wide(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    no_flags(cx, ins)?;
    arity(cx, ins, &[2])?;
    let rd = reg_of(cx, &ins.ops[0])?;
    no_pc(cx, ins.ops[0].span, rd)?;
    let rd = rd as u32;
    let v = imm_bits(cx, &ins.ops[1], 16)?;
    let top = if ins.mnem == Mnem::Movw {
        0x0300_0000
    } else {
        0x0340_0000
    };
    Some(one(word(
        ins.cond,
        top | ((v & 0xf000) << 4) | (rd << 12) | (v & 0xfff),
    )))
}

// ---- status registers ------------------------------------------------------

/// The banked register a name stands for, as `(R, m1, m)`: the three fields
/// `mrs` and `msr` spell a mode's private register with.
///
/// The list is `reg_names[]`'s in `gas/config/tc-arm.c`, where `lr_irq` and
/// its neighbours are generated from the mode's base number.
pub fn banked(name: &str) -> Option<(u32, u32, u32)> {
    // Each mode's `lr`, `sp` and `spsr`, from the base the mode's registers
    // start at; `sp` is the one after `lr`, and `spsr` shares `lr`'s number
    // with the R bit set.
    const MODES: [(&str, u32); 6] = [
        ("irq", 0),
        ("svc", 2),
        ("abt", 4),
        ("und", 6),
        ("mon", 12),
        ("hyp", 14),
    ];
    if let Some(rest) = name.strip_prefix('r')
        && let Some((n, mode)) = rest.split_once('_')
        && let Ok(n) = n.parse::<u32>()
        && (8..=12).contains(&n)
    {
        return match mode {
            "usr" => Some((0, n - 8, 0)),
            "fiq" => Some((0, n, 0)),
            _ => None,
        };
    }
    match name {
        "sp_usr" => return Some((0, 5, 0)),
        "lr_usr" => return Some((0, 6, 0)),
        "sp_fiq" => return Some((0, 13, 0)),
        "lr_fiq" => return Some((0, 14, 0)),
        "spsr_fiq" => return Some((1, 14, 0)),
        // The hypervisor's link register is spelled `elr`, and it has no
        // banked `lr`.
        "elr_hyp" => return Some((0, 14, 1)),
        "lr_hyp" => return None,
        _ => {}
    }
    let (which, mode) = name.split_once('_')?;
    let base = MODES.iter().find(|(m, _)| *m == mode)?.1;
    Some(match which {
        "lr" => (0, base, 1),
        "sp" => (0, base + 1, 1),
        "spsr" => (1, base, 1),
        _ => return None,
    })
}

/// The `R` bit and field mask of a status register written as `cpsr_fsxc`,
/// `spsr_c` or `apsr_nzcvq`.
///
/// `parse_psr` in `gas/config/tc-arm.c`: `cpsr` and `spsr` take the field
/// letters in any order, plus the three names an older assembler used, and
/// `apsr` names bits instead — `nzcvq` all together for the flags byte and
/// `g` for the `ge` bits.
pub fn psr_fields(cx: &mut AsmCtx<'_>, op: &Operand, spec: &str) -> Option<(u32, u32)> {
    let (name, suffix) = match spec.split_once('_') {
        Some((n, f)) => (n, Some(f)),
        None => (spec, None),
    };
    let (r, apsr) = match name {
        "cpsr" => (0, false),
        "spsr" => (1, false),
        "apsr" => (0, true),
        _ => {
            cx.error(op.span, "expected `cpsr`, `spsr` or `apsr`");
            return None;
        }
    };
    let Some(suffix) = suffix else {
        // Writing `apsr` with no bitmask is deprecated and means the flags;
        // `cpsr` and `spsr` mean the control and flags bytes.
        return Some((r, if apsr { 8 } else { 9 }));
    };
    if apsr {
        let mut flags = 0u32;
        let mut ge = 0u32;
        for c in suffix.chars() {
            match c {
                'n' => flags |= 1,
                'z' => flags |= 2,
                'c' => flags |= 4,
                'v' => flags |= 8,
                'q' => flags |= 16,
                'g' => ge = 4,
                _ => {
                    cx.error(op.span, format!("unexpected bit `{c}` after `apsr`"));
                    return None;
                }
            }
        }
        if flags != 0 && flags != 0x1f {
            cx.error(
                op.span,
                "`apsr` takes all of `nzcvq` together, with or without `g`",
            );
            return None;
        }
        return Some((r, if flags == 0 { 0 } else { 8 } | ge));
    }
    // The names an assembler before UAL used for whole fields.
    let mask = match suffix {
        "all" => 9,
        "flg" => 8,
        "ctl" => 1,
        _ => {
            let mut mask = 0;
            for c in suffix.chars() {
                mask |= match c {
                    'c' => 1,
                    'x' => 2,
                    's' => 4,
                    'f' => 8,
                    _ => {
                        cx.error(op.span, format!("unknown status register field `{c}`"));
                        return None;
                    }
                };
            }
            mask
        }
    };
    Some((r, mask))
}

fn status_read(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    no_flags(cx, ins)?;
    arity(cx, ins, &[2])?;
    let rd = reg_of(cx, &ins.ops[0])?;
    no_pc(cx, ins.ops[0].span, rd)?;
    let rd = rd as u32;
    let name = ins.ops[1].word.clone().unwrap_or_default();
    if let Some((r, m1, m)) = banked(&name) {
        return Some(one(word(
            ins.cond,
            0x0100_0200 | (r << 22) | (m1 << 16) | (rd << 12) | (m << 8),
        )));
    }
    let spsr = match name.as_str() {
        "cpsr" | "apsr" => 0,
        "spsr" => 1,
        _ => {
            cx.error(
                ins.ops[1].span,
                "expected `cpsr`, `apsr`, `spsr` or a banked register",
            );
            return None;
        }
    };
    Some(one(word(ins.cond, 0x010f_0000 | (spsr << 22) | (rd << 12))))
}

/// `msr cpsr_<fields>, rm` and `msr <banked>, rm`. The four field letters
/// select which byte of the status register the write reaches; `_f` alone
/// (the common case) touches only the condition flags.
fn status_write(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) -> Option<Vec<Variant>> {
    no_flags(cx, ins)?;
    arity(cx, ins, &[2])?;
    let spec = ins.ops[0].word.clone().unwrap_or_default();
    if let Some((r, m1, m)) = banked(&spec) {
        let rn = reg_of(cx, &ins.ops[1])? as u32;
        return Some(one(word(
            ins.cond,
            0x0120_f200 | (r << 22) | (m1 << 16) | (m << 8) | rn,
        )));
    }
    let (r, mask) = psr_fields(cx, &ins.ops[0], &spec)?;
    if let Some(rm) = ins.ops[1].reg() {
        return Some(one(word(
            ins.cond,
            0x0120_f000 | (r << 22) | (mask << 16) | rm as u32,
        )));
    }
    // The immediate form, which only A32 has.
    let v = imm32(cx, &ins.ops[1])?;
    let Some(field) = imm::modified(v) else {
        cx.error(
            ins.ops[1].span,
            format!("{} (0x{v:08x}) is not an ARM modified immediate", v as i32),
        );
        return None;
    };
    Some(one(word(
        ins.cond,
        0x0320_f000 | (r << 22) | (mask << 16) | field,
    )))
}

//! Mnemonic dispatch and the per-family encoders.
//!
//! A64 has no operand-encoding table worth the name: every family has its own
//! fixed field layout, and the interesting work is in the aliases, which are
//! defined by the ARM ARM as rewrites onto a base form (`cmp` is `subs` to the
//! zero register, `lsl #n` is a `ubfm`, `mov` is one of four different
//! instructions depending on its operands). Writing those rewrites out is
//! clearer than a table that would need an escape hatch for each of them.

use super::encode::{const_in_range, field, logical_imm, word, word_fixup};
use super::operand::{ExtendOp, Mem, MemKind, Operand, OperandKind, RelocOp, ShiftOp};
use super::reg::{self, Reg, RegClass};
use super::{encode, sysreg};
use crate::arch::{AsmCtx, InsnRequest};
use crate::expr::{ExprKind, ExprRef};
use crate::lexer::{Punct, TokKind};
use crate::section::{LinkValue, Variant};
use crate::source::Span;

/// Everything one `assemble` call needs, so the family encoders take two
/// arguments instead of five.
pub struct Insn<'a, 't> {
    pub mnemonic: &'a str,
    pub ops: &'a [Operand<'t>],
    pub span: Span,
}

impl Insn<'_, '_> {
    /// Checks the operand count, reporting the accepted counts on failure.
    fn arity(&self, cx: &mut AsmCtx<'_>, want: &[usize]) -> bool {
        if want.contains(&self.ops.len()) {
            return true;
        }
        let list: Vec<String> = want.iter().map(|n| n.to_string()).collect();
        cx.error(
            self.span,
            format!(
                "`{}` takes {} operand(s), but {} were given",
                self.mnemonic,
                list.join(" or "),
                self.ops.len()
            ),
        );
        false
    }

    fn op(&self, i: usize) -> Option<&Operand<'_>> {
        self.ops.get(i)
    }

    /// A general-purpose register operand.
    ///
    /// Register 31 is the zero register in most fields, so `sp` is refused
    /// here; the few fields that mean the stack pointer use [`Self::gpr_or_sp`].
    fn gpr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let r = self.gpr_or_sp(cx, i)?;
        if r.is_sp() {
            cx.error(
                self.ops[i].span,
                format!(
                    "operand {} of `{}` cannot be the stack pointer",
                    i + 1,
                    self.mnemonic
                ),
            );
            return None;
        }
        Some(r)
    }

    /// A general-purpose register operand where `sp`/`wsp` is also allowed.
    fn gpr_or_sp(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let op = self.op(i)?;
        match op.reg() {
            Some(r) if r.is_gpr() => Some(r),
            _ => {
                cx.error(
                    op.span,
                    format!(
                        "operand {} of `{}` must be a general-purpose register, found {}",
                        i + 1,
                        self.mnemonic,
                        op.describe()
                    ),
                );
                None
            }
        }
    }

    /// A register of any class.
    fn any_reg(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let op = self.op(i)?;
        match op.reg() {
            Some(r) => Some(r),
            None => {
                cx.error(
                    op.span,
                    format!(
                        "operand {} of `{}` must be a register",
                        i + 1,
                        self.mnemonic
                    ),
                );
                None
            }
        }
    }

    fn cond(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<u8> {
        let op = self.op(i)?;
        match op.cond() {
            Some(c) => Some(c),
            None => {
                cx.error(op.span, "expected a condition code");
                None
            }
        }
    }

    fn expr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<ExprRef> {
        let op = self.op(i)?;
        op.expr(cx)
    }

    /// A constant operand with a range check.
    fn imm(&self, cx: &mut AsmCtx<'_>, i: usize, lo: i64, hi: i64, what: &str) -> Option<i64> {
        let e = self.expr(cx, i)?;
        const_in_range(cx, e, lo, hi, what)
    }

    fn mem(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Mem> {
        let op = self.op(i)?;
        match op.mem() {
            Some(m) => Some(m.clone()),
            None => {
                cx.error(op.span, "expected an address in `[...]`");
                None
            }
        }
    }

    /// Rejects mixing `w` and `x` operands, which is the commonest A64 typo.
    fn same_width(&self, cx: &mut AsmCtx<'_>, regs: &[Reg]) -> Option<RegClass> {
        let first = regs.first()?.class;
        for r in regs {
            if r.class != first {
                cx.error(
                    self.span,
                    format!("`{}` cannot mix 32-bit and 64-bit registers", self.mnemonic),
                );
                return None;
            }
        }
        Some(first)
    }

    fn no_sp(&self, cx: &mut AsmCtx<'_>, r: Reg) -> Option<()> {
        if r.is_sp() {
            cx.error(
                self.span,
                format!("`{}` cannot use the stack pointer here", self.mnemonic),
            );
            return None;
        }
        Some(())
    }

    /// The mirror of [`Self::no_sp`], for fields where register 31 is the stack
    /// pointer: writing `xzr` there would silently name `sp` instead.
    fn no_zr(&self, cx: &mut AsmCtx<'_>, r: Reg) -> Option<()> {
        if r.is_zr() {
            cx.error(
                self.span,
                format!(
                    "`{}` cannot use the zero register here: this field would read it as the stack pointer",
                    self.mnemonic
                ),
            );
            return None;
        }
        Some(())
    }
}

fn one(w: u32) -> Option<Vec<Variant>> {
    Some(vec![word(w)])
}

fn one_fixup(w: u32, e: ExprRef, k: crate::section::FixupKind, span: Span) -> Option<Vec<Variant>> {
    Some(vec![word_fixup(w, e, k, span)])
}

/// A PC-relative target. Written as a number rather than a label, it is the
/// offset from the instruction, not an address: `b #16` branches 16 bytes
/// ahead wherever it is, to GNU as and llvm-mc alike. So a number goes
/// straight into the field, range-checked as the fixup would have been.
fn pcrel(
    cx: &mut AsmCtx<'_>,
    w: u32,
    e: ExprRef,
    k: crate::section::FixupKind,
    span: Span,
) -> Option<Vec<Variant>> {
    let Some(v) = cx.constant(e).filter(|_| !names_symbol(cx, e)) else {
        return one_fixup(w, e, k, span);
    };
    let crate::section::FieldEncoding::Scatter(place) = k.encoding else {
        return one_fixup(w, e, k, span);
    };
    let bits = u32::from(k.value_bits);
    let (lo, hi) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
    if !(lo..=hi).contains(&v) {
        cx.error(span, format!("offset {v} is out of range {lo}..={hi}"));
        return None;
    }
    if v % i64::from(k.value_align) != 0 {
        cx.error(
            span,
            format!("offset {v} is not a multiple of {}", k.value_align),
        );
        return None;
    }
    one(place(u64::from(w), v) as u32)
}

/// True for a mnemonic the encoders here handle, whatever else the
/// generated table has under the same name: `add` is handwritten for the
/// general-purpose registers and a table form for vectors. Keep in step with
/// the dispatch in [`assemble`].
pub(crate) fn handwritten(mnemonic: &str) -> bool {
    mnemonic
        .strip_prefix("b.")
        .is_some_and(|c| reg::cond(c).is_some())
        || mnemonic
            .strip_prefix('b')
            .is_some_and(|c| c.len() == 2 && reg::cond(c).is_some())
        || matches!(
            mnemonic,
            "add"
                | "adds"
                | "sub"
                | "subs"
                | "cmp"
                | "cmn"
                | "neg"
                | "negs"
                | "adc"
                | "adcs"
                | "sbc"
                | "sbcs"
                | "ngc"
                | "ngcs"
                | "and"
                | "ands"
                | "orr"
                | "eor"
                | "bic"
                | "bics"
                | "orn"
                | "eon"
                | "tst"
                | "mvn"
                | "mov"
                | "movz"
                | "movn"
                | "movk"
                | "sbfm"
                | "ubfm"
                | "bfm"
                | "sbfx"
                | "ubfx"
                | "bfxil"
                | "sbfiz"
                | "ubfiz"
                | "bfi"
                | "sxtb"
                | "sxth"
                | "sxtw"
                | "uxtb"
                | "uxth"
                | "lsl"
                | "lsr"
                | "asr"
                | "ror"
                | "lslv"
                | "lsrv"
                | "asrv"
                | "rorv"
                | "extr"
                | "mul"
                | "mneg"
                | "smull"
                | "umull"
                | "smnegl"
                | "umnegl"
                | "smulh"
                | "umulh"
                | "madd"
                | "msub"
                | "smaddl"
                | "umaddl"
                | "smsubl"
                | "umsubl"
                | "sdiv"
                | "udiv"
                | "rbit"
                | "rev"
                | "rev16"
                | "rev32"
                | "rev64"
                | "clz"
                | "cls"
                | "csel"
                | "csinc"
                | "csinv"
                | "csneg"
                | "cset"
                | "csetm"
                | "cinc"
                | "cinv"
                | "cneg"
                | "ccmp"
                | "ccmn"
                | "b"
                | "bl"
                | "cbz"
                | "cbnz"
                | "tbz"
                | "tbnz"
                | "br"
                | "blr"
                | "ret"
                | "eret"
                | "drps"
                | "adr"
                | "adrp"
                | "hint"
                | "dmb"
                | "dsb"
                | "isb"
                | "clrex"
                | "sys"
                | "sysl"
                | "svc"
                | "hvc"
                | "smc"
                | "brk"
                | "hlt"
                | "dcps1"
                | "dcps2"
                | "dcps3"
                | "mrs"
                | "msr"
                | "smstart"
                | "smstop"
                | "zero"
        )
        || loads(mnemonic)
        // The system instructions and the aliases of `hint`, whose names are
        // in the generated tables rather than written out here.
        || sysreg::is_sys_ins(mnemonic)
        || sysreg::is_hint(mnemonic)
}

/// True for the handwritten loads and stores, which take the scalar SIMD
/// registers (`ldr d0, [x0]`) and leave only the SVE forms to the table.
pub(crate) fn loads(mnemonic: &str) -> bool {
    matches!(
        mnemonic,
        "ldr"
            | "str"
            | "ldrb"
            | "strb"
            | "ldrh"
            | "strh"
            | "ldrsb"
            | "ldrsh"
            | "ldrsw"
            | "ldur"
            | "stur"
            | "ldurb"
            | "sturb"
            | "ldurh"
            | "sturh"
            | "ldursb"
            | "ldursh"
            | "ldursw"
            | "prfm"
            | "prfum"
            | "ldp"
            | "stp"
            | "ldpsw"
            | "ldnp"
            | "stnp"
    )
}

/// Entry point: resolves a mnemonic to a family and encodes it.
pub fn assemble(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
    ops: &[Operand<'_>],
) -> Option<Vec<Variant>> {
    let i = Insn {
        mnemonic,
        ops,
        span: req.span,
    };

    // `b.<cond>` and the `b<cond>` spelling GNU as also accepts.
    if let Some(rest) = mnemonic.strip_prefix("b.")
        && let Some(c) = reg::cond(rest)
    {
        return branch_cond(cx, &i, c);
    }
    if let Some(rest) = mnemonic.strip_prefix('b')
        && rest.len() == 2
        && let Some(c) = reg::cond(rest)
    {
        return branch_cond(cx, &i, c);
    }

    match mnemonic {
        "add" | "adds" | "sub" | "subs" => addsub(cx, &i),
        "cmp" | "cmn" => cmp(cx, &i),
        "neg" | "negs" => neg(cx, &i),
        "adc" | "adcs" | "sbc" | "sbcs" => addsub_carry(cx, &i),
        "ngc" | "ngcs" => ngc(cx, &i),

        "and" | "ands" | "orr" | "eor" | "bic" | "bics" | "orn" | "eon" => logic(cx, &i),
        "tst" => tst(cx, &i),
        "mvn" => mvn(cx, &i),
        "mov" => mov(cx, &i),
        "movz" | "movn" | "movk" => movw(cx, &i),

        "sbfm" | "ubfm" | "bfm" => bitfield_raw(cx, &i),
        "sbfx" | "ubfx" | "bfxil" => bitfield_extract(cx, &i),
        "sbfiz" | "ubfiz" | "bfi" => bitfield_insert(cx, &i),
        "sxtb" | "sxth" | "sxtw" | "uxtb" | "uxth" => extend(cx, &i),
        "lsl" | "lsr" | "asr" | "ror" => shift(cx, &i),
        "lslv" | "lsrv" | "asrv" | "rorv" => shift_reg(cx, &i, mnemonic.trim_end_matches('v')),
        "extr" => extr(cx, &i),

        "mul" | "mneg" | "smull" | "umull" | "smnegl" | "umnegl" | "smulh" | "umulh" => {
            mul_alias(cx, &i)
        }
        "madd" | "msub" | "smaddl" | "umaddl" | "smsubl" | "umsubl" => madd(cx, &i),
        "sdiv" | "udiv" => div(cx, &i),

        "rbit" | "rev" | "rev16" | "rev32" | "rev64" | "clz" | "cls" => dp1(cx, &i),

        "csel" | "csinc" | "csinv" | "csneg" => csel(cx, &i),
        "cset" | "csetm" => cset(cx, &i),
        "cinc" | "cinv" | "cneg" => cinc(cx, &i),
        "ccmp" | "ccmn" => ccmp(cx, &i),

        "b" | "bl" => branch(cx, &i),
        "cbz" | "cbnz" => cbz(cx, &i),
        "tbz" | "tbnz" => tbz(cx, &i),
        "br" | "blr" | "ret" => branch_reg(cx, &i),
        "eret" => {
            i.arity(cx, &[0]).then_some(())?;
            one(0xd69f_03e0)
        }
        "drps" => {
            i.arity(cx, &[0]).then_some(())?;
            one(0xd6bf_03e0)
        }

        "adr" | "adrp" => adr(cx, &i),

        "ldr" | "str" | "ldrb" | "strb" | "ldrh" | "strh" | "ldrsb" | "ldrsh" | "ldrsw"
        | "ldur" | "stur" | "ldurb" | "sturb" | "ldurh" | "sturh" | "ldursb" | "ldursh"
        | "ldursw" | "prfm" | "prfum" => ldst(cx, &i),
        "ldp" | "stp" | "ldpsw" | "ldnp" | "stnp" => ldst_pair(cx, &i),

        "hint" => hint(cx, &i),
        "dmb" | "dsb" | "isb" | "clrex" => barrier(cx, &i),
        "svc" | "hvc" | "smc" | "brk" | "hlt" | "dcps1" | "dcps2" | "dcps3" => exception(cx, &i),
        "mrs" => mrs(cx, &i),
        "msr" => msr(cx, &i),
        "sys" | "sysl" => sys_raw(cx, &i),
        "smstart" | "smstop" => sme_mode(cx, &i),

        // `dc civac, x0` and `esb` and their like: a name in the generated
        // system tables is all these mnemonics are.
        _ if sysreg::is_sys_ins(mnemonic) => sys_alias(cx, &i),
        _ if sysreg::is_hint(mnemonic) => hint_alias(cx, &i),

        _ => {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            None
        }
    }
}

// ---- add / subtract --------------------------------------------------------

const ADDSUB_IMM: u32 = 0x1100_0000;
const ADDSUB_SHIFT: u32 = 0x0b00_0000;
const ADDSUB_EXT: u32 = 0x0b20_0000;

/// The `op` (subtract) and `S` (set flags) bits shared by the add/sub family.
fn addsub_bits(mnemonic: &str) -> (u32, u32) {
    let sub = mnemonic.starts_with("sub") || mnemonic.starts_with("neg") || mnemonic == "cmp";
    let s = mnemonic.ends_with('s');
    (u32::from(sub), u32::from(s))
}

/// `add`/`adds`/`sub`/`subs` with three or four operands.
fn addsub(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3, 4]).then_some(())?;
    let rd = i.gpr_or_sp(cx, 0)?;
    let rn = i.gpr_or_sp(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let (op, s) = addsub_bits(i.mnemonic);
    encode_addsub(cx, i, op, s, rd, rn, 2)
}

/// `cmp`/`cmn`, which are `subs`/`adds` writing to the zero register.
fn cmp(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rn = i.gpr_or_sp(cx, 0)?;
    let rd = Reg::zero(rn.class);
    let (op, _) = addsub_bits(i.mnemonic);
    encode_addsub(cx, i, op, 1, rd, rn, 1)
}

/// `neg`/`negs`, which are `sub`/`subs` from the zero register.
fn neg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rm = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rm])?;
    let (_, s) = addsub_bits(i.mnemonic);
    let shift = shift_operand(
        cx,
        i,
        2,
        rd.class,
        &[ShiftOp::Lsl, ShiftOp::Lsr, ShiftOp::Asr],
    )?;
    one(field(rd.sf(), 31, 1)
        | field(1, 30, 1)
        | field(s, 29, 1)
        | ADDSUB_SHIFT
        | field(shift.0, 22, 2)
        | field(rm.num as u32, 16, 5)
        | field(shift.1, 10, 6)
        | field(31, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// The shared tail of add/sub: an immediate, a shifted register or an extended
/// register, starting at operand `at`.
fn encode_addsub(
    cx: &mut AsmCtx<'_>,
    i: &Insn<'_, '_>,
    op: u32,
    s: u32,
    rd: Reg,
    rn: Reg,
    at: usize,
) -> Option<Vec<Variant>> {
    let src = i.op(at)?;
    let sf = field(rd.sf(), 31, 1) | field(op, 30, 1) | field(s, 29, 1);
    // A flag-setting add or subtract writes register 31 as the zero register.
    if s == 1 {
        i.no_sp(cx, rd)?;
    }

    if let Some(rm) = src.reg() {
        if !rm.is_gpr() || rm.is_sp() {
            cx.error(
                src.span,
                "expected a general-purpose register other than the stack pointer",
            );
            return None;
        }
        // The extended-register form is the only one that can name the stack
        // pointer, so a bare `add sp, sp, x0` has to use it. There, `lsl`
        // means "extend from the operation's own width", which is `uxtx` at 64
        // bits and `uxtw` at 32.
        let uses_sp = rd.is_sp() || rn.is_sp();
        let natural = if rd.class == RegClass::X {
            ExtendOp::Uxtx
        } else {
            ExtendOp::Uxtw
        };
        let extend = match i.op(at + 1).map(|o| &o.kind) {
            Some(OperandKind::Extend(e, amount)) => Some((*e, *amount)),
            Some(OperandKind::Shift(ShiftOp::Lsl, amount)) if uses_sp => {
                i.same_width(cx, &[rd, rn, rm])?;
                Some((natural, Some(*amount)))
            }
            _ => None,
        };
        if extend.is_some() || uses_sp {
            i.no_zr(cx, rn)?;
            if s == 0 {
                i.no_zr(cx, rd)?;
            }
            let (ext, amount) = match extend {
                Some((e, a)) => (e, a),
                None => {
                    i.same_width(cx, &[rd, rn, rm])?;
                    (natural, None)
                }
            };
            if rd.class == RegClass::W && rm.class != RegClass::W {
                cx.error(
                    src.span,
                    "a 32-bit operation cannot extend a 64-bit register",
                );
                return None;
            }
            if ext.source_class() != rm.class {
                cx.error(
                    src.span,
                    format!(
                        "`{}` needs a {} register",
                        ext.name(),
                        ext.source_class().letter()
                    ),
                );
                return None;
            }
            let amount = match amount {
                Some(e) => const_in_range(cx, e, 0, 4, "an extend amount")? as u32,
                None => 0,
            };
            if i.ops.len() > at + 2 {
                cx.error(i.ops[at + 2].span, "too many operands");
                return None;
            }
            return one(sf
                | ADDSUB_EXT
                | field(rm.num as u32, 16, 5)
                | field(ext.code(), 13, 3)
                | field(amount, 10, 3)
                | field(rn.num as u32, 5, 5)
                | field(rd.num as u32, 0, 5));
        }
        i.same_width(cx, &[rd, rn, rm])?;
        i.no_sp(cx, rd)?;
        i.no_sp(cx, rn)?;
        let (kind, amount) = shift_operand(
            cx,
            i,
            at + 1,
            rd.class,
            &[ShiftOp::Lsl, ShiftOp::Lsr, ShiftOp::Asr],
        )?;
        return one(sf
            | ADDSUB_SHIFT
            | field(kind, 22, 2)
            | field(rm.num as u32, 16, 5)
            | field(amount, 10, 6)
            | field(rn.num as u32, 5, 5)
            | field(rd.num as u32, 0, 5));
    }

    // `add x0, x0, :lo12:sym`: the second half of an `adrp` pair. Only a plain
    // add makes sense — the linker writes the bits as an unsigned offset.
    if let OperandKind::Reloc(rop, e) = src.kind {
        if rop != RelocOp::Lo12 || op != 0 || s != 0 {
            cx.error(
                src.span,
                format!("`{}` needs a plain `add`, not `{}`", rop.name(), i.mnemonic),
            );
            return None;
        }
        if i.ops.len() > at + 1 {
            cx.error(i.ops[at + 1].span, "a `:lo12:` immediate cannot be shifted");
            return None;
        }
        return one_fixup(
            sf | ADDSUB_IMM | field(rn.num as u32, 5, 5) | field(rd.num as u32, 0, 5),
            e,
            encode::fixup_lo12_add(),
            src.span,
        );
    }

    // An immediate, optionally shifted left by 12.
    let e = src.expr(cx)?;
    let mut shift12 = 0;
    if let Some(next) = i.op(at + 1) {
        match next.kind {
            OperandKind::Shift(ShiftOp::Lsl, amount) => {
                match const_in_range(cx, amount, 0, 12, "an add/sub immediate shift")? {
                    0 => {}
                    12 => shift12 = 1,
                    n => {
                        cx.error(
                            next.span,
                            format!("an add/sub immediate can only be shifted by 0 or 12, not {n}"),
                        );
                        return None;
                    }
                }
            }
            _ => {
                cx.error(next.span, "expected `lsl #0` or `lsl #12`");
                return None;
            }
        }
    }
    if i.ops.len() > at + 2 {
        cx.error(i.ops[at + 2].span, "too many operands");
        return None;
    }
    let Some(v) = cx.constant(e) else {
        cx.error(src.span, "an add/sub immediate must be a constant");
        return None;
    };
    // Both register fields of the immediate form read 31 as the stack pointer,
    // apart from the destination of a flag-setting one.
    i.no_zr(cx, rn)?;
    if s == 0 {
        i.no_zr(cx, rd)?;
    }
    // Without an explicit shift the assembler is free to pick one: a negative
    // immediate flips add into sub and back, and a multiple of 4096 moves into
    // the shifted field. Both are what llvm-mc and GNU as do, and existing
    // code relies on `sub sp, sp, #-16` and `add x0, x0, #0x10000`.
    let (op, mut v) = match (shift12, v.checked_neg()) {
        (0, Some(n)) if v < 0 => (op ^ 1, n),
        _ => (op, v),
    };
    if shift12 == 0 && v > 4095 && v & 0xfff == 0 {
        shift12 = 1;
        v >>= 12;
    }
    if !(0..=4095).contains(&v) {
        cx.error(
            src.span,
            format!(
                "an add/sub immediate must be 0..=4095, or a multiple of 4096 up to {:#x}, but is {}",
                4095 << 12,
                if shift12 == 1 { format!("{v} << 12") } else { v.to_string() }
            ),
        );
        return None;
    }
    one(field(rd.sf(), 31, 1)
        | field(op, 30, 1)
        | field(s, 29, 1)
        | ADDSUB_IMM
        | field(shift12, 22, 1)
        | field(v as u32, 10, 12)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `adc`/`sbc` and their flag-setting forms.
fn addsub_carry(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let op = u32::from(i.mnemonic.starts_with("sbc"));
    let s = u32::from(i.mnemonic.ends_with('s'));
    one(field(rd.sf(), 31, 1)
        | field(op, 30, 1)
        | field(s, 29, 1)
        | 0x1a00_0000
        | field(rm.num as u32, 16, 5)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `ngc`, which is `sbc` from the zero register.
fn ngc(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rm = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rm])?;
    let s = u32::from(i.mnemonic.ends_with('s'));
    one(field(rd.sf(), 31, 1)
        | field(1, 30, 1)
        | field(s, 29, 1)
        | 0x1a00_0000
        | field(rm.num as u32, 16, 5)
        | field(31, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// Reads an optional trailing shift operand, returning `(kind, amount)`.
fn shift_operand(
    cx: &mut AsmCtx<'_>,
    i: &Insn<'_, '_>,
    at: usize,
    class: RegClass,
    allowed: &[ShiftOp],
) -> Option<(u32, u32)> {
    let Some(op) = i.op(at) else {
        return Some((0, 0));
    };
    let OperandKind::Shift(kind, amount) = op.kind else {
        cx.error(op.span, "expected a shift such as `lsl #3`");
        return None;
    };
    if !allowed.contains(&kind) {
        let names: Vec<&str> = allowed.iter().map(|s| s.name()).collect();
        cx.error(
            op.span,
            format!("`{}` only accepts {}", i.mnemonic, names.join(", ")),
        );
        return None;
    }
    let max = if class == RegClass::X { 63 } else { 31 };
    let n = const_in_range(cx, amount, 0, max, "a shift amount")?;
    if i.ops.len() > at + 1 {
        cx.error(i.ops[at + 1].span, "too many operands");
        return None;
    }
    Some((kind.code(), n as u32))
}

// ---- logical ---------------------------------------------------------------

const LOGIC_SHIFT: u32 = 0x0a00_0000;
const LOGIC_IMM: u32 = 0x1200_0000;

/// `(opc, N)` for the logical family. `N` inverts the second operand.
fn logic_bits(mnemonic: &str) -> Option<(u32, u32)> {
    Some(match mnemonic {
        "and" => (0, 0),
        "bic" => (0, 1),
        "orr" => (1, 0),
        "orn" => (1, 1),
        "eor" => (2, 0),
        "eon" => (2, 1),
        "ands" => (3, 0),
        "bics" => (3, 1),
        _ => return None,
    })
}

fn logic(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3, 4]).then_some(())?;
    // Only the immediate forms can write the stack pointer; the register forms
    // check again once they know which form they are.
    let rd = i.gpr_or_sp(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let (opc, n) = logic_bits(i.mnemonic)?;
    encode_logic(cx, i, opc, n, rd, rn, 2)
}

fn encode_logic(
    cx: &mut AsmCtx<'_>,
    i: &Insn<'_, '_>,
    opc: u32,
    n: u32,
    rd: Reg,
    rn: Reg,
    at: usize,
) -> Option<Vec<Variant>> {
    let src = i.op(at)?;
    let head = field(rd.sf(), 31, 1) | field(opc, 29, 2);

    if let Some(rm) = src.reg() {
        if !rm.is_gpr() {
            cx.error(src.span, "expected a general-purpose register");
            return None;
        }
        i.same_width(cx, &[rd, rn, rm])?;
        i.no_sp(cx, rd)?;
        i.no_sp(cx, rn)?;
        i.no_sp(cx, rm)?;
        let (kind, amount) = shift_operand(
            cx,
            i,
            at + 1,
            rd.class,
            &[ShiftOp::Lsl, ShiftOp::Lsr, ShiftOp::Asr, ShiftOp::Ror],
        )?;
        return one(head
            | LOGIC_SHIFT
            | field(kind, 22, 2)
            | field(n, 21, 1)
            | field(rm.num as u32, 16, 5)
            | field(amount, 10, 6)
            | field(rn.num as u32, 5, 5)
            | field(rd.num as u32, 0, 5));
    }

    // The inverted mnemonics have no immediate encoding of their own: with an
    // immediate, `bic`, `bics`, `orn` and `eon` are `and`, `ands`, `orr` and
    // `eor` of the complement, which is what both references assemble them to
    // (GNU as takes only `bic` that way; llvm-mc takes all four).
    let invert = n != 0;
    if i.ops.len() > at + 1 {
        cx.error(i.ops[at + 1].span, "too many operands");
        return None;
    }
    let e = src.expr(cx)?;
    let Some(v) = cx.constant(e) else {
        cx.error(src.span, "a logical immediate must be a constant");
        return None;
    };
    let bits = if rd.class == RegClass::X { 64 } else { 32 };
    let mut masked = if bits == 32 {
        v as u32 as u64
    } else {
        v as u64
    };
    if invert {
        masked = !masked;
        if bits == 32 {
            masked &= 0xffff_ffff;
        }
    }
    let Some((imm_n, immr, imms)) = logical_imm(masked, bits) else {
        cx.error(
            src.span,
            if invert {
                format!(
                    "{v:#x} is not a valid `{}` immediate: its complement must be a repeating run of ones, which the field holds",
                    i.mnemonic
                )
            } else {
                format!(
                    "{v:#x} is not a valid logical immediate: the field holds only a repeating run of ones"
                )
            },
        );
        return None;
    };
    // The immediate forms write to the stack pointer, not the zero register —
    // except `ands`, whose flag-setting destination is the zero register.
    if opc == 3 {
        i.no_sp(cx, rd)?;
    } else {
        i.no_zr(cx, rd)?;
    }
    one(head
        | LOGIC_IMM
        | field(imm_n, 22, 1)
        | field(immr, 16, 6)
        | field(imms, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `tst`, which is `ands` to the zero register.
fn tst(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rn = i.gpr(cx, 0)?;
    encode_logic(cx, i, 3, 0, Reg::zero(rn.class), rn, 1)
}

/// `mvn`, which is `orn` from the zero register.
fn mvn(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    encode_logic(cx, i, 1, 1, rd, Reg::zero(rd.class), 1)
}

// ---- moves -----------------------------------------------------------------

const MOVW: u32 = 0x1280_0000;

fn movw(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let opc = match i.mnemonic {
        "movn" => 0,
        "movz" => 2,
        _ => 3,
    };
    let v = i.imm(cx, 1, -0x8000, 0xffff, "a move-wide immediate")?;
    let mut hw = 0u32;
    if let Some(op) = i.op(2) {
        let OperandKind::Shift(ShiftOp::Lsl, amount) = op.kind else {
            cx.error(op.span, "expected `lsl #0`, `#16`, `#32` or `#48`");
            return None;
        };
        let n = const_in_range(cx, amount, 0, 48, "a move-wide shift")?;
        if n % 16 != 0 {
            cx.error(op.span, "a move-wide shift must be 0, 16, 32 or 48");
            return None;
        }
        hw = n as u32 / 16;
    }
    if rd.class == RegClass::W && hw > 1 {
        cx.error(i.span, "a 32-bit move-wide can only shift by 0 or 16");
        return None;
    }
    one(field(rd.sf(), 31, 1)
        | field(opc, 29, 2)
        | MOVW
        | field(hw, 21, 2)
        | field(v as u32, 5, 16)
        | field(rd.num as u32, 0, 5))
}

/// `mov`, which is four different instructions.
///
/// Between registers it is `orr Rd, ZR, Rm`, except that the stack pointer can
/// only be named by `add Rd, Rn, #0`. With an immediate the assembler picks the
/// first of `movz`, `movn` and `orr Rd, ZR, #imm` that can hold the value —
/// the order GNU as and llvm-mc both use.
fn mov(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr_or_sp(cx, 0)?;
    let src = i.op(1)?;

    if let Some(rm) = src.reg() {
        if !rm.is_gpr() {
            cx.error(src.span, "expected a general-purpose register");
            return None;
        }
        i.same_width(cx, &[rd, rm])?;
        if rd.is_sp() || rm.is_sp() {
            i.no_zr(cx, rd)?;
            i.no_zr(cx, rm)?;
            return one(field(rd.sf(), 31, 1)
                | ADDSUB_IMM
                | field(rm.num as u32, 5, 5)
                | field(rd.num as u32, 0, 5));
        }
        return one(field(rd.sf(), 31, 1)
            | field(1, 29, 2)
            | LOGIC_SHIFT
            | field(rm.num as u32, 16, 5)
            | field(31, 5, 5)
            | field(rd.num as u32, 0, 5));
    }

    i.no_sp(cx, rd)?;
    let e = src.expr(cx)?;
    let Some(v) = cx.constant(e) else {
        cx.error(src.span, "a `mov` immediate must be a constant");
        return None;
    };
    let bits = if rd.class == RegClass::X { 64 } else { 32 };
    let value = if bits == 32 {
        v as u32 as u64
    } else {
        v as u64
    };
    let sf = field(rd.sf(), 31, 1);
    let rd_bits = field(rd.num as u32, 0, 5);

    // movz: the value is one 16-bit chunk, everything else zero.
    for hw in 0..(bits / 16) {
        let shift = hw * 16;
        if value >> shift << shift == value && (value >> shift) <= 0xffff {
            return one(sf
                | field(2, 29, 2)
                | MOVW
                | field(hw, 21, 2)
                | field((value >> shift) as u32, 5, 16)
                | rd_bits);
        }
    }
    // movn: the *inverse* is one 16-bit chunk.
    let inverted = if bits == 32 {
        !value & 0xffff_ffff
    } else {
        !value
    };
    for hw in 0..(bits / 16) {
        let shift = hw * 16;
        if inverted >> shift << shift == inverted && (inverted >> shift) <= 0xffff {
            return one(sf
                | MOVW
                | field(hw, 21, 2)
                | field((inverted >> shift) as u32, 5, 16)
                | rd_bits);
        }
    }
    // orr Rd, ZR, #imm, for the bitmask patterns.
    if let Some((imm_n, immr, imms)) = logical_imm(value, bits) {
        return one(sf
            | field(1, 29, 2)
            | LOGIC_IMM
            | field(imm_n, 22, 1)
            | field(immr, 16, 6)
            | field(imms, 10, 6)
            | field(31, 5, 5)
            | rd_bits);
    }
    cx.error(
        src.span,
        format!("{v:#x} cannot be moved in one instruction; use `movz`/`movk`, or a literal pool"),
    );
    None
}

// ---- bitfield --------------------------------------------------------------

const BITFIELD: u32 = 0x1300_0000;

fn bitfield_opc(mnemonic: &str) -> u32 {
    match mnemonic {
        "sbfm" | "sbfx" | "sbfiz" | "asr" | "sxtb" | "sxth" | "sxtw" => 0,
        "bfm" | "bfxil" | "bfi" => 1,
        _ => 2,
    }
}

fn emit_bitfield(rd: Reg, rn: Reg, opc: u32, immr: u32, imms: u32) -> Option<Vec<Variant>> {
    let n = rd.sf();
    one(field(rd.sf(), 31, 1)
        | field(opc, 29, 2)
        | BITFIELD
        | field(n, 22, 1)
        | field(immr, 16, 6)
        | field(imms, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `sbfm`/`ubfm`/`bfm` written out in full.
fn bitfield_raw(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let max = if rd.class == RegClass::X { 63 } else { 31 };
    let immr = i.imm(cx, 2, 0, max, "`immr`")? as u32;
    let imms = i.imm(cx, 3, 0, max, "`imms`")? as u32;
    emit_bitfield(rd, rn, bitfield_opc(i.mnemonic), immr, imms)
}

/// `sbfx`/`ubfx`/`bfxil`: extract `width` bits starting at `lsb`.
fn bitfield_extract(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let max = if rd.class == RegClass::X { 63 } else { 31 };
    let lsb = i.imm(cx, 2, 0, max, "the bit position")?;
    let width = i.imm(cx, 3, 1, max + 1 - lsb, "the field width")?;
    emit_bitfield(
        rd,
        rn,
        bitfield_opc(i.mnemonic),
        lsb as u32,
        (lsb + width - 1) as u32,
    )
}

/// `sbfiz`/`ubfiz`/`bfi`: insert `width` bits *at* `lsb`, which rotates the
/// source down rather than up, so `immr` counts backwards from the register
/// width.
fn bitfield_insert(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let width_bits = if rd.class == RegClass::X { 64 } else { 32 };
    let lsb = i.imm(cx, 2, 0, width_bits - 1, "the bit position")?;
    let width = i.imm(cx, 3, 1, width_bits - lsb, "the field width")?;
    emit_bitfield(
        rd,
        rn,
        bitfield_opc(i.mnemonic),
        ((width_bits - lsb) % width_bits) as u32,
        (width - 1) as u32,
    )
}

/// `sxtb`/`sxth`/`sxtw`/`uxtb`/`uxth`, all bitfield moves from bit 0.
fn extend(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    // The source is always a `w` register: these read the low 8, 16 or 32 bits.
    if rn.class != RegClass::W {
        cx.error(
            i.ops[1].span,
            format!("`{}` reads a 32-bit register", i.mnemonic),
        );
        return None;
    }
    let imms = match i.mnemonic {
        "sxtb" | "uxtb" => 7,
        "sxth" | "uxth" => 15,
        _ => 31,
    };
    if i.mnemonic == "sxtw" && rd.class != RegClass::X {
        cx.error(i.span, "`sxtw` writes a 64-bit register");
        return None;
    }
    if matches!(i.mnemonic, "uxtb" | "uxth") && rd.class != RegClass::W {
        cx.error(i.span, format!("`{}` writes a 32-bit register", i.mnemonic));
        return None;
    }
    // `sbfm` reads its source at the destination's width, so the source
    // register number is reused with the destination's class.
    let src = Reg {
        class: rd.class,
        num: rn.num,
        sp: false,
    };
    emit_bitfield(rd, src, bitfield_opc(i.mnemonic), 0, imms)
}

/// `lsl`/`lsr`/`asr`/`ror`, by an immediate (a bitfield move or `extr`) or by a
/// register (the variable-shift instructions).
fn shift(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    if i.op(2).and_then(|o| o.reg()).is_some() {
        return shift_reg(cx, i, i.mnemonic);
    }
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let width = if rd.class == RegClass::X { 64 } else { 32 };
    let n = i.imm(cx, 2, 0, width - 1, "a shift amount")? as u32;
    let width = width as u32;
    match i.mnemonic {
        // `lsl #n` keeps the low width-n bits and moves them up: ubfm with
        // immr = -n mod width.
        "lsl" => emit_bitfield(rd, rn, 2, (width - n) % width, width - 1 - n),
        "lsr" => emit_bitfield(rd, rn, 2, n, width - 1),
        "asr" => emit_bitfield(rd, rn, 0, n, width - 1),
        _ => one(field(rd.sf(), 31, 1)
            | 0x1380_0000
            | field(rd.sf(), 22, 1)
            | field(rn.num as u32, 16, 5)
            | field(n, 10, 6)
            | field(rn.num as u32, 5, 5)
            | field(rd.num as u32, 0, 5)),
    }
}

/// The variable-shift instructions, whose canonical names end in `v`.
fn shift_reg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, base: &str) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let opcode = match base {
        "lsl" => 0b1000,
        "lsr" => 0b1001,
        "asr" => 0b1010,
        _ => 0b1011,
    };
    one(field(rd.sf(), 31, 1)
        | 0x1ac0_0000
        | field(rm.num as u32, 16, 5)
        | field(opcode, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

fn extr(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let max = if rd.class == RegClass::X { 63 } else { 31 };
    let lsb = i.imm(cx, 3, 0, max, "the rotate amount")? as u32;
    one(field(rd.sf(), 31, 1)
        | 0x1380_0000
        | field(rd.sf(), 22, 1)
        | field(rm.num as u32, 16, 5)
        | field(lsb, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

// ---- multiply and divide ---------------------------------------------------

const DP3: u32 = 0x1b00_0000;

/// `(op31, o0, widening)` for the three-source multiply family.
fn madd_bits(mnemonic: &str) -> Option<(u32, u32, bool)> {
    Some(match mnemonic {
        "madd" | "mul" => (0, 0, false),
        "msub" | "mneg" => (0, 1, false),
        "smaddl" | "smull" => (1, 0, true),
        "smsubl" | "smnegl" => (1, 1, true),
        "smulh" => (2, 0, true),
        "umaddl" | "umull" => (5, 0, true),
        "umsubl" | "umnegl" => (5, 1, true),
        "umulh" => (6, 0, true),
        _ => return None,
    })
}

fn emit_madd(rd: Reg, rn: Reg, rm: Reg, ra: u8, op31: u32, o0: u32) -> Option<Vec<Variant>> {
    one(field(rd.sf(), 31, 1)
        | DP3
        | field(op31, 21, 3)
        | field(rm.num as u32, 16, 5)
        | field(o0, 15, 1)
        | field(ra as u32, 10, 5)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `mul`, `mneg`, `smull` and friends: `madd`-family with `Ra` = zero register.
fn mul_alias(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    let (op31, o0, widening) = madd_bits(i.mnemonic)?;
    let high = matches!(i.mnemonic, "smulh" | "umulh");
    if widening && !high {
        // The `l` forms multiply two 32-bit registers into a 64-bit one.
        if rd.class != RegClass::X || rn.class != RegClass::W || rm.class != RegClass::W {
            cx.error(
                i.span,
                format!(
                    "`{}` takes an `x` destination and two `w` sources",
                    i.mnemonic
                ),
            );
            return None;
        }
    } else {
        i.same_width(cx, &[rd, rn, rm])?;
        if high && rd.class != RegClass::X {
            cx.error(i.span, format!("`{}` is 64-bit only", i.mnemonic));
            return None;
        }
    }
    emit_madd(rd, rn, rm, 31, op31, o0)
}

fn madd(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    let ra = i.gpr(cx, 3)?;
    let (op31, o0, widening) = madd_bits(i.mnemonic)?;
    if widening {
        if rd.class != RegClass::X
            || rn.class != RegClass::W
            || rm.class != RegClass::W
            || ra.class != RegClass::X
        {
            cx.error(
                i.span,
                format!(
                    "`{}` takes `x` for its destination and accumulator and `w` for its multiplicands",
                    i.mnemonic
                ),
            );
            return None;
        }
    } else {
        i.same_width(cx, &[rd, rn, rm, ra])?;
    }
    emit_madd(rd, rn, rm, ra.num, op31, o0)
}

fn div(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let opcode = if i.mnemonic == "sdiv" { 3 } else { 2 };
    one(field(rd.sf(), 31, 1)
        | 0x1ac0_0000
        | field(rm.num as u32, 16, 5)
        | field(opcode, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// The one-source data-processing group: bit and byte reversal, counting.
fn dp1(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let x = rd.class == RegClass::X;
    let opcode = match i.mnemonic {
        "rbit" => 0,
        "rev16" => 1,
        // `rev` reverses the whole register, so it is opcode 2 at 32 bits and
        // opcode 3 at 64; `rev32` is the 64-bit-only "reverse each word" form.
        "rev32" => {
            if !x {
                cx.error(i.span, "`rev32` is 64-bit only");
                return None;
            }
            2
        }
        "rev" => {
            if x {
                3
            } else {
                2
            }
        }
        "rev64" => {
            if !x {
                cx.error(i.span, "`rev64` is 64-bit only");
                return None;
            }
            3
        }
        "clz" => 4,
        _ => 5,
    };
    one(field(rd.sf(), 31, 1)
        | 0x5ac0_0000
        | field(opcode, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

// ---- conditional -----------------------------------------------------------

const CONDSEL: u32 = 0x1a80_0000;

fn emit_condsel(rd: Reg, rn: Reg, rm: Reg, cond: u8, op: u32, op2: u32) -> Option<Vec<Variant>> {
    one(field(rd.sf(), 31, 1)
        | field(op, 30, 1)
        | CONDSEL
        | field(rm.num as u32, 16, 5)
        | field(cond as u32, 12, 4)
        | field(op2, 10, 2)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

fn csel(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let cond = i.cond(cx, 3)?;
    let (op, op2) = match i.mnemonic {
        "csel" => (0, 0),
        "csinc" => (0, 1),
        "csinv" => (1, 0),
        _ => (1, 1),
    };
    emit_condsel(rd, rn, rm, cond, op, op2)
}

/// The condition an alias inverts. `al`/`nv` have no inverse, and the ARM ARM
/// makes that an error rather than silently encoding `nv`.
fn invert(cx: &mut AsmCtx<'_>, span: Span, cond: u8) -> Option<u8> {
    if cond >= 14 {
        cx.error(
            span,
            "`al` and `nv` cannot be inverted, so this alias has no encoding",
        );
        return None;
    }
    Some(cond ^ 1)
}

/// `cset`/`csetm`: set a register from the *inverse* condition of a select
/// between two zero registers.
fn cset(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let cond = i.cond(cx, 1)?;
    let cond = invert(cx, i.ops[1].span, cond)?;
    let zr = Reg::zero(rd.class);
    let (op, op2) = if i.mnemonic == "cset" { (0, 1) } else { (1, 0) };
    emit_condsel(rd, zr, zr, cond, op, op2)
}

/// `cinc`/`cinv`/`cneg`: the two-source selects with both sources the same.
fn cinc(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let cond = i.cond(cx, 2)?;
    let cond = invert(cx, i.ops[2].span, cond)?;
    let (op, op2) = match i.mnemonic {
        "cinc" => (0, 1),
        "cinv" => (1, 0),
        _ => (1, 1),
    };
    emit_condsel(rd, rn, rn, cond, op, op2)
}

fn ccmp(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rn = i.gpr(cx, 0)?;
    let nzcv = i.imm(cx, 2, 0, 15, "the flags value")? as u32;
    let cond = i.cond(cx, 3)?;
    let op = u32::from(i.mnemonic == "ccmp");
    let head = field(rn.sf(), 31, 1)
        | field(op, 30, 1)
        | field(1, 29, 1)
        | 0x1a40_0000
        | field(cond as u32, 12, 4)
        | field(rn.num as u32, 5, 5)
        | nzcv;
    let src = i.op(1)?;
    if src.reg().is_some() {
        let rm = i.gpr(cx, 1)?;
        i.same_width(cx, &[rn, rm])?;
        return one(head | field(rm.num as u32, 16, 5));
    }
    let imm = i.imm(cx, 1, 0, 31, "a conditional-compare immediate")? as u32;
    one(head | field(imm, 16, 5) | field(1, 11, 1))
}

// ---- branches --------------------------------------------------------------

fn branch(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let e = i.expr(cx, 0)?;
    let (base, kind) = if i.mnemonic == "bl" {
        (0x9400_0000, encode::fixup_call())
    } else {
        (0x1400_0000, encode::fixup_b())
    };
    pcrel(cx, base, e, kind, i.ops[0].span)
}

fn branch_cond(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, cond: u8) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let e = i.expr(cx, 0)?;
    pcrel(
        cx,
        0x5400_0000 | field(cond as u32, 0, 4),
        e,
        encode::fixup_b19(),
        i.ops[0].span,
    )
}

fn cbz(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rt = i.gpr(cx, 0)?;
    let e = i.expr(cx, 1)?;
    let base = if i.mnemonic == "cbz" {
        0x3400_0000
    } else {
        0x3500_0000
    };
    pcrel(
        cx,
        field(rt.sf(), 31, 1) | base | field(rt.num as u32, 0, 5),
        e,
        encode::fixup_b19(),
        i.ops[1].span,
    )
}

fn tbz(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rt = i.gpr(cx, 0)?;
    let max = if rt.class == RegClass::X { 63 } else { 31 };
    let bit = i.imm(cx, 1, 0, max, "the bit number")? as u32;
    let e = i.expr(cx, 2)?;
    let base = if i.mnemonic == "tbz" {
        0x3600_0000
    } else {
        0x3700_0000
    };
    // The bit number is split: its top bit doubles as the register-width bit.
    pcrel(
        cx,
        field(bit >> 5, 31, 1) | base | field(bit & 31, 19, 5) | field(rt.num as u32, 0, 5),
        e,
        encode::fixup_b14(),
        i.ops[2].span,
    )
}

fn branch_reg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let default_lr = i.mnemonic == "ret";
    i.arity(cx, if default_lr { &[0, 1] } else { &[1] })
        .then_some(())?;
    let rn = match i.op(0) {
        Some(_) => {
            let r = i.gpr(cx, 0)?;
            if r.class != RegClass::X {
                cx.error(i.ops[0].span, "a branch target register must be 64-bit");
                return None;
            }
            r.num
        }
        None => 30,
    };
    let base = match i.mnemonic {
        "br" => 0xd61f_0000,
        "blr" => 0xd63f_0000,
        _ => 0xd65f_0000,
    };
    one(base | field(rn as u32, 5, 5))
}

fn adr(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    if rd.class != RegClass::X {
        cx.error(i.ops[0].span, "`adr` and `adrp` write a 64-bit register");
        return None;
    }
    let target = i.op(1)?;
    let (e, kind) = match (i.mnemonic, &target.kind) {
        ("adrp", OperandKind::Reloc(RelocOp::Got, e)) => (*e, encode::fixup_got_page()),
        ("adrp", _) => {
            let e = target.expr(cx)?;
            // A bare number is a count of pages from here, to GNU as and
            // llvm-mc alike, rather than an address to take the page of.
            let kind = if names_symbol(cx, e) {
                encode::fixup_adrp()
            } else {
                encode::fixup_adrp().link(LinkValue::Plain)
            };
            (e, kind)
        }
        _ => (target.expr(cx)?, encode::fixup_adr()),
    };
    let base = if i.mnemonic == "adrp" {
        0x9000_0000
    } else {
        0x1000_0000
    };
    if i.mnemonic == "adr" {
        return pcrel(cx, base | field(rd.num as u32, 0, 5), e, kind, target.span);
    }
    one_fixup(base | field(rd.num as u32, 0, 5), e, kind, target.span)
}

/// Whether an expression refers to a symbol or a position, rather than being
/// arithmetic on numbers alone.
fn names_symbol(cx: &AsmCtx<'_>, e: ExprRef) -> bool {
    match cx.exprs.get(e).kind {
        ExprKind::Int(_) => false,
        ExprKind::Unary(_, x) | ExprKind::Modifier(_, x) => names_symbol(cx, x),
        ExprKind::Binary(_, l, r) => names_symbol(cx, l) || names_symbol(cx, r),
        _ => true,
    }
}

// ---- loads and stores ------------------------------------------------------

/// The fields a load or store needs, once the mnemonic and the data register
/// have been looked at together.
struct LdStForm {
    /// The two-bit `size` field.
    size: u32,
    /// SIMD/FP register file.
    v: bool,
    /// The two-bit `opc` field.
    opc: u32,
    /// log2 of the access width, which scales the unsigned immediate offset
    /// and selects the `S` bit of a register offset.
    scale: u32,
    /// Only the unscaled 9-bit form exists (`ldur` and friends).
    unscaled_only: bool,
}

fn ldst_form(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, rt: Option<Reg>) -> Option<LdStForm> {
    let m = i.mnemonic;
    let unscaled_only = m.starts_with("ldur") || m.starts_with("stur") || m == "prfum";
    // `ldur`/`stur` differ from `ldr`/`str` only in the addressing form.
    let base: &str = match m {
        "ldur" => "ldr",
        "stur" => "str",
        "ldurb" => "ldrb",
        "sturb" => "strb",
        "ldurh" => "ldrh",
        "sturh" => "strh",
        "ldursb" => "ldrsb",
        "ldursh" => "ldrsh",
        "ldursw" => "ldrsw",
        "prfum" => "prfm",
        other => other,
    };
    let load = base.starts_with("ld") || base == "prfm";
    let (size, v, opc, scale) = match base {
        "ldrb" | "strb" => (0, false, u32::from(load), 0),
        "ldrh" | "strh" => (1, false, u32::from(load), 1),
        "ldrsb" | "ldrsh" | "ldrsw" => {
            let rt = rt?;
            let size = match base {
                "ldrsb" => 0,
                "ldrsh" => 1,
                _ => 2,
            };
            if base == "ldrsw" && rt.class != RegClass::X {
                cx.error(i.span, "`ldrsw` writes a 64-bit register");
                return None;
            }
            // `opc` bit 0 selects a 32-bit destination for the sign-extending
            // loads, which is the reverse of the usual load/store convention.
            (size, false, 2 | u32::from(rt.class == RegClass::W), size)
        }
        "prfm" => (3, false, 2, 3),
        _ => {
            let rt = rt?;
            match rt.class {
                RegClass::W => (2, false, u32::from(load), 2),
                RegClass::X => (3, false, u32::from(load), 3),
                // A 128-bit access sets the high bit of `opc` instead of using
                // a fourth `size` value, which is already taken.
                RegClass::Q => (0, true, 2 | u32::from(load), 4),
                c => (
                    match c {
                        RegClass::B => 0,
                        RegClass::H => 1,
                        RegClass::S => 2,
                        _ => 3,
                    },
                    true,
                    u32::from(load),
                    c.bytes().trailing_zeros(),
                ),
            }
        }
    };
    Some(LdStForm {
        size,
        v,
        opc,
        scale,
        unscaled_only,
    })
}

fn ldst(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    // `ldr x0, =value` puts the value in a literal pool and loads it from
    // there; `ldr x0, label` is a PC-relative literal load, not an
    // addressing mode.
    if let Some(OperandKind::Literal(e)) = i.op(1).map(|o| &o.kind) {
        return pool_load(cx, i, *e);
    }
    if i.mnemonic == "ldr" && i.op(1).is_some_and(|o| o.mem().is_none()) {
        return ldst_literal(cx, i);
    }
    let prefetch = i.mnemonic.starts_with("prf");
    let (rt_num, rt) = if prefetch {
        (prefetch_hint(cx, i)?, None)
    } else {
        let r = i.any_reg(cx, 0)?;
        if r.is_sp() {
            cx.error(
                i.ops[0].span,
                "the stack pointer cannot be loaded or stored",
            );
            return None;
        }
        (r.num, Some(r))
    };
    let form = ldst_form(cx, i, rt)?;
    let mem = i.mem(cx, 1)?;
    let head = field(form.size, 30, 2)
        | field(u32::from(form.v), 26, 1)
        | field(form.opc, 22, 2)
        | field(mem.base.num as u32, 5, 5)
        | field(rt_num as u32, 0, 5);

    match &mem.kind {
        MemKind::Offset(off) => {
            let value = match off {
                None => Some(0),
                Some(e) => cx.constant(*e),
            };
            // A symbolic offset has no relocation here; `:lo12:` is the way to
            // write one, and `ldr x0, label` is the PC-relative literal form.
            let Some(v) = value else {
                cx.error(mem.span, "a load/store offset must be a constant");
                return None;
            };
            let scaled = v >> form.scale;
            if !form.unscaled_only
                && v >= 0
                && scaled << form.scale == v
                && (0..=4095).contains(&scaled)
            {
                return one(head | 0x3900_0000 | field(scaled as u32, 10, 12));
            }
            if !(-256..=255).contains(&v) {
                cx.error(
                    mem.span,
                    format!(
                        "offset {v} is out of range: it must be -256..=255, \
                         or a multiple of {} up to {}",
                        1 << form.scale,
                        4095u32 << form.scale
                    ),
                );
                return None;
            }
            one(head | 0x3800_0000 | field(v as u32, 12, 9))
        }
        MemKind::OffsetReloc(rop, e) => {
            if form.unscaled_only {
                cx.error(
                    mem.span,
                    format!(
                        "`{}` has no 12-bit offset field for `{}`",
                        i.mnemonic,
                        rop.name()
                    ),
                );
                return None;
            }
            let kind = match rop {
                RelocOp::Lo12 => encode::fixup_lo12_ldst(form.scale),
                RelocOp::GotLo12 if form.scale == 3 && !form.v => encode::fixup_got_lo12(3),
                // Darwin loads a 32-bit slot too.
                RelocOp::GotLo12
                    if form.scale == 2 && !form.v && cx.find_modifier_for(*e).is_some() =>
                {
                    encode::fixup_got_lo12(2)
                }
                _ => {
                    cx.error(
                        mem.span,
                        format!("`{}` is not valid for this load or store", rop.name()),
                    );
                    return None;
                }
            };
            one_fixup(head | 0x3900_0000, *e, kind, mem.span)
        }
        MemKind::PreIndex(e) | MemKind::PostIndex(e) => {
            let pre = matches!(mem.kind, MemKind::PreIndex(_));
            let v = const_in_range(cx, *e, -256, 255, "a pre/post-index offset")?;
            let kind = if pre { 3 } else { 1 };
            one(head | 0x3800_0000 | field(v as u32, 12, 9) | field(kind, 10, 2))
        }
        MemKind::Reg(index) => {
            // `S` picks between shifting by 0 and by the access size. For a
            // byte access both mean 0, and an explicit `lsl #0` still selects
            // `S=1` — so the scale test has to come before the zero test.
            let s = match index.amount {
                None => 0,
                Some(n) if n as u32 == form.scale => 1,
                Some(0) => 0,
                Some(n) => {
                    cx.error(
                        mem.span,
                        format!(
                            "this access can only shift its index by {} (or not at all), not {n}",
                            form.scale
                        ),
                    );
                    return None;
                }
            };
            one(head
                | 0x3820_0000
                | field(index.reg.num as u32, 16, 5)
                | field(index.ext.code(), 13, 3)
                | field(s, 12, 1)
                | field(2, 10, 2))
        }
    }
}

/// `ldr x0, label`: a PC-relative load from a literal pool.
fn ldst_literal(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let rt = i.any_reg(cx, 0)?;
    let (opc, v) = match rt.class {
        RegClass::W => (0, false),
        RegClass::X => (1, false),
        RegClass::S => (0, true),
        RegClass::D => (1, true),
        RegClass::Q => (2, true),
        _ => {
            cx.error(
                i.ops[0].span,
                "this register cannot be loaded from a literal",
            );
            return None;
        }
    };
    let e = i.expr(cx, 1)?;
    pcrel(
        cx,
        field(opc, 30, 2) | 0x1800_0000 | field(u32::from(v), 26, 1) | field(rt.num as u32, 0, 5),
        e,
        encode::fixup_ld_lit(),
        i.ops[1].span,
    )
}

/// `ldr <rt>, =value`: the value goes in the section's literal pool and the
/// instruction loads it from there.
///
/// GNU as, whose source this syntax comes from, always makes an entry, even
/// for a value `movz` could hold; llvm-mc assembles `ldr x0, =1` as
/// `mov x0, #1` instead. rsasm follows GNU as here, as it does for ARM: what
/// a pool holds and where it goes is decided across instructions, and the
/// source that uses pools was written for GNU as.
fn pool_load(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, e: ExprRef) -> Option<Vec<Variant>> {
    if !matches!(i.mnemonic, "ldr" | "ldrsw") {
        cx.error(
            i.ops[1].span,
            format!(
                "`{}` cannot load from a literal pool; only `ldr` and `ldrsw` can",
                i.mnemonic
            ),
        );
        return None;
    }
    let rt = i.any_reg(cx, 0)?;
    // The entry is as wide as the register the load fills, and `ldrsw`
    // sign-extends a word.
    let (opc, v, size) = match (i.mnemonic, rt.class) {
        ("ldrsw", RegClass::X) => (2, false, 4),
        ("ldrsw", _) => {
            cx.error(i.ops[0].span, "`ldrsw` writes a 64-bit register");
            return None;
        }
        (_, RegClass::W) => (0, false, 4),
        (_, RegClass::X) => (1, false, 8),
        (_, RegClass::S) => (0, true, 4),
        (_, RegClass::D) => (1, true, 8),
        (_, RegClass::Q) => (2, true, 16),
        _ => {
            cx.error(
                i.ops[0].span,
                "this register cannot be loaded from a literal pool",
            );
            return None;
        }
    };
    // A constant is kept as a number, so that two uses of the same value
    // share an entry; anything else is an expression the entry relocates.
    let value = match cx.constant(e) {
        Some(n) => crate::arch::Literal::Const(n),
        None => crate::arch::Literal::Expr(e),
    };
    let entry = cx.literal(value, size, i.ops[1].span);
    let kind = encode::fixup_ld_lit()
        .with_range_hint("the literal pool is too far away; put an `.ltorg` nearer");
    one_fixup(
        field(opc, 30, 2) | 0x1800_0000 | field(u32::from(v), 26, 1) | field(rt.num as u32, 0, 5),
        entry,
        kind,
        i.ops[1].span,
    )
}

fn prefetch_hint(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<u8> {
    let op = i.op(0)?;
    if let Some(n) = op.word() {
        let text = cx.name(n).to_ascii_lowercase();
        // `p<type><target><policy>`: load/store/instruction, L1/L2/L3, and
        // keep/strm. The five bits are laid out in exactly that order.
        let ty = match text.get(..3) {
            Some("pld") => Some(0),
            Some("pli") => Some(1),
            Some("pst") => Some(2),
            _ => None,
        };
        let rest = match text.get(3..) {
            Some("l1keep") => Some((0, 0)),
            Some("l1strm") => Some((0, 1)),
            Some("l2keep") => Some((1, 0)),
            Some("l2strm") => Some((1, 1)),
            Some("l3keep") => Some((2, 0)),
            Some("l3strm") => Some((2, 1)),
            _ => None,
        };
        let (Some(ty), Some((level, policy))) = (ty, rest) else {
            cx.error(op.span, format!("`{text}` is not a prefetch hint"));
            return None;
        };
        return Some(ty << 3 | level << 1 | policy);
    }
    let v = i.imm(cx, 0, 0, 31, "a prefetch operation")?;
    Some(v as u8)
}

fn ldst_pair(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rt = i.any_reg(cx, 0)?;
    let rt2 = i.any_reg(cx, 1)?;
    if rt.class != rt2.class {
        cx.error(i.span, "both registers of a pair must be the same width");
        return None;
    }
    if rt.is_sp() || rt2.is_sp() {
        cx.error(i.span, "the stack pointer cannot be loaded or stored");
        return None;
    }
    let load = i.mnemonic.starts_with("ld");
    let (opc, v, scale) = match (i.mnemonic, rt.class) {
        ("ldpsw", RegClass::X) => (1, false, 2),
        ("ldpsw", _) => {
            cx.error(i.span, "`ldpsw` writes 64-bit registers");
            return None;
        }
        (_, RegClass::W) => (0, false, 2),
        (_, RegClass::X) => (2, false, 3),
        (_, RegClass::S) => (0, true, 2),
        (_, RegClass::D) => (1, true, 3),
        (_, RegClass::Q) => (2, true, 4),
        _ => {
            cx.error(i.ops[0].span, "this register cannot be part of a pair");
            return None;
        }
    };
    let mem = i.mem(cx, 2)?;
    let (kind, off) = match &mem.kind {
        MemKind::Offset(off) => (
            if matches!(i.mnemonic, "ldnp" | "stnp") {
                0
            } else {
                2
            },
            *off,
        ),
        MemKind::PreIndex(e) => (3, Some(*e)),
        MemKind::PostIndex(e) => (1, Some(*e)),
        MemKind::Reg(_) => {
            cx.error(mem.span, "a pair access cannot use a register offset");
            return None;
        }
        MemKind::OffsetReloc(rop, _) => {
            cx.error(
                mem.span,
                format!("a pair offset has no relocation for `{}`", rop.name()),
            );
            return None;
        }
    };
    let v_off = match off {
        None => 0,
        Some(e) => {
            let Some(n) = cx.constant(e) else {
                cx.error(mem.span, "a pair offset must be a constant");
                return None;
            };
            n
        }
    };
    let unit = 1i64 << scale;
    if v_off % unit != 0 {
        cx.error(
            mem.span,
            format!("a pair offset must be a multiple of {unit}, but is {v_off}"),
        );
        return None;
    }
    let scaled = v_off / unit;
    if !(-64..=63).contains(&scaled) {
        cx.error(
            mem.span,
            format!(
                "offset {v_off} is out of range: a pair offset must be {}..={} in steps of {unit}",
                -64 * unit,
                63 * unit
            ),
        );
        return None;
    }
    one(field(opc, 30, 2)
        | 0x2800_0000
        | field(u32::from(v), 26, 1)
        | field(kind, 23, 3)
        | field(u32::from(load), 22, 1)
        | field(scaled as u32, 15, 7)
        | field(rt2.num as u32, 10, 5)
        | field(mem.base.num as u32, 5, 5)
        | field(rt.num as u32, 0, 5))
}

// ---- system ----------------------------------------------------------------

/// `hint #n`, the instruction every no-operand alias in the generated table
/// is one of; the named spellings go through [`hint_alias`].
fn hint(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let imm = i.imm(cx, 0, 0, 127, "a hint number")? as u32;
    one(0xd503_201f | field(imm, 5, 7))
}

/// The name of an operand written as a bare word, or as the one register
/// that is a name here (`chkfeat x16`).
fn option_name(cx: &mut AsmCtx<'_>, op: &Operand<'_>) -> Option<String> {
    if let Some(n) = op.word() {
        return Some(cx.name(n).to_ascii_lowercase());
    }
    op.reg().map(|r| r.name())
}

/// The aliases of `hint` and of the barriers: a mnemonic on its own (`esb`,
/// `sb`, `paciasp`) or with one named operand (`psb csync`, `bti c`), whose
/// word the generated table holds.
fn hint_alias(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[0, 1]).then_some(())?;
    let name = match i.op(0) {
        None => String::new(),
        Some(op) => match option_name(cx, op) {
            Some(text) => text,
            None => {
                cx.error(op.span, format!("expected a `{}` operand", i.mnemonic));
                return None;
            }
        },
    };
    match sysreg::hint(i.mnemonic, &name) {
        Some(w) => one(w),
        None => {
            let options = sysreg::hint_options(i.mnemonic);
            let span = i.op(0).map_or(i.span, |op| op.span);
            cx.error(
                span,
                if options.is_empty() {
                    format!("`{}` takes no operand", i.mnemonic)
                } else if name.is_empty() {
                    format!("`{}` takes {}", i.mnemonic, options.join(" or "))
                } else {
                    format!(
                        "`{name}` is not an operand of `{}`; it takes {}",
                        i.mnemonic,
                        options.join(" or ")
                    )
                },
            );
            None
        }
    }
}

/// `dmb`, `dsb`, `isb` and `clrex`, whose option is a name in the table or
/// the `CRm` number itself. `dsb` takes a fifth bit for the nXS variants,
/// which have names of their own and only four values.
fn barrier(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[0, 1]).then_some(())?;
    if i.op(0).is_none_or(|op| option_name(cx, op).is_some()) {
        return hint_alias(cx, i);
    }
    let nxs = i.mnemonic == "dsb";
    let imm = i.imm(cx, 0, 0, if nxs { 31 } else { 15 }, "a barrier option")? as u32;
    if imm > 15 {
        // `dsb #16` and its three neighbours are the nXS barriers; nothing
        // between them is a barrier at all.
        let name = match imm {
            16 => "oshnxs",
            20 => "nshnxs",
            24 => "ishnxs",
            28 => "synxs",
            _ => {
                cx.error(
                    i.ops[0].span,
                    "the nXS barriers are `dsb` 16, 20, 24 and 28",
                );
                return None;
            }
        };
        return one(sysreg::hint("dsb", name)?);
    }
    let op2 = match i.mnemonic {
        "clrex" => 2,
        "dsb" => 4,
        "dmb" => 5,
        _ => 6,
    };
    one(0xd503_301f | field(imm, 8, 4) | field(op2, 5, 3))
}

/// `dc`, `ic`, `at`, `tlbi` and the rest: a name for a `sys` word, which
/// decides whether an address register follows it.
fn sys_alias(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1, 2]).then_some(())?;
    let op = i.op(0)?;
    let Some(name) = op.word().map(|n| cx.name(n).to_ascii_lowercase()) else {
        cx.error(op.span, format!("expected a `{}` operand name", i.mnemonic));
        return None;
    };
    let Some((bits, xt)) = sysreg::sys_ins(i.mnemonic, &name) else {
        cx.error(
            op.span,
            format!("`{name}` is not an operand of `{}`", i.mnemonic),
        );
        return None;
    };
    let rt = match (i.op(1), xt) {
        (Some(second), sysreg::Xt::None) => {
            cx.error(
                second.span,
                format!("`{} {name}` takes no register", i.mnemonic),
            );
            return None;
        }
        (None, sysreg::Xt::Needs) => {
            cx.error(
                i.span,
                format!("`{} {name}` needs an address register", i.mnemonic),
            );
            return None;
        }
        // A name that takes no register, or may have none, has `xzr` in the
        // field, which is what GNU as writes there.
        (None, _) => 31,
        (Some(_), _) => {
            let r = i.gpr(cx, 1)?;
            if r.class != RegClass::X {
                cx.error(
                    i.ops[1].span,
                    "a system instruction takes a 64-bit register",
                );
                return None;
            }
            u32::from(r.num)
        }
    };
    one(bits | field(rt, 0, 5))
}

/// A `CRn`/`CRm` operand: `c0` through `c15`.
fn creg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, at: usize) -> Option<u32> {
    let op = i.op(at)?;
    let name = op.word().map(|n| cx.name(n).to_ascii_lowercase());
    let number = name
        .as_deref()
        .and_then(|t| t.strip_prefix('c'))
        // `c01` is not a name for `c1`, any more than `x01` is for `x1`.
        .filter(|rest| *rest == "0" || !rest.starts_with('0'))
        .and_then(|rest| rest.parse::<u32>().ok())
        .filter(|n| *n < 16);
    match number {
        Some(n) => Some(n),
        None => {
            cx.error(op.span, "expected `c0` through `c15`");
            None
        }
    }
}

/// `sys #op1, Cn, Cm, #op2{, Xt}` and `sysl Xt, #op1, Cn, Cm, #op2`: the
/// system instruction with no name at all, which every `dc`, `ic`, `at` and
/// `tlbi` is an alias of.
fn sys_raw(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let reading = i.mnemonic == "sysl";
    i.arity(cx, if reading { &[5] } else { &[4, 5] })
        .then_some(())?;
    let at = usize::from(reading);
    let op1 = i.imm(cx, at, 0, 7, "`op1`")? as u32;
    let crn = creg(cx, i, at + 1)?;
    let crm = creg(cx, i, at + 2)?;
    let op2 = i.imm(cx, at + 3, 0, 7, "`op2`")? as u32;
    let rt = match i.op(if reading { 0 } else { 4 }) {
        None => 31,
        Some(_) => {
            let r = i.gpr(cx, if reading { 0 } else { 4 })?;
            if r.class != RegClass::X {
                cx.error(i.span, "a system instruction takes a 64-bit register");
                return None;
            }
            u32::from(r.num)
        }
    };
    let base = if reading { sysreg::SYSL } else { sysreg::SYS };
    one(base | field(op1, 16, 3) | field(crn, 12, 4) | field(crm, 8, 4) | field(op2, 5, 3) | rt)
}

fn exception(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let dcps = i.mnemonic.starts_with("dcps");
    i.arity(cx, if dcps { &[0, 1] } else { &[1] })
        .then_some(())?;
    let imm = match i.op(0) {
        Some(_) => i.imm(cx, 0, 0, 0xffff, "an exception number")? as u32,
        None => 0,
    };
    let base = match i.mnemonic {
        "svc" => 0xd400_0001,
        "hvc" => 0xd400_0002,
        "smc" => 0xd400_0003,
        "brk" => 0xd420_0000,
        "hlt" => 0xd440_0000,
        "dcps1" => 0xd4a0_0001,
        "dcps2" => 0xd4a0_0002,
        _ => 0xd4a0_0003,
    };
    one(base | field(imm, 5, 16))
}

fn mrs(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rt = i.gpr(cx, 0)?;
    if rt.class != RegClass::X {
        cx.error(i.ops[0].span, "`mrs` writes a 64-bit register");
        return None;
    }
    let enc = sysreg::register(cx, i.op(1)?, Some(false))?;
    one(0xd530_0000 | enc | field(rt.num as u32, 0, 5))
}

/// `smstart`/`smstop`: the `msr svcr…` writes that enter and leave SME's
/// streaming mode (`sm`), enable its ZA storage (`za`), or with no operand,
/// both. `CRm` holds which in its middle bits and start or stop in its low one.
fn sme_mode(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[0, 1]).then_some(())?;
    let which = match i.op(0) {
        None => 0b11,
        Some(op) => match op
            .word()
            .map(|n| cx.name(n).to_ascii_lowercase())
            .as_deref()
        {
            Some("sm") => 0b01,
            Some("za") => 0b10,
            _ => {
                cx.error(
                    op.span,
                    format!("`{}` takes `sm`, `za` or nothing", i.mnemonic),
                );
                return None;
            }
        },
    };
    let start = u32::from(i.mnemonic == "smstart");
    one(0xd503_407f | field(which << 1 | start, 8, 4))
}

/// `zero {za}`, which clears SME's ZA storage. The list is the whole
/// operand: the older per-tile spellings and SME2's `zt0` are not taken.
pub(crate) fn sme_zero(cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
    let toks = req.operands;
    let za = match toks {
        [open, name, close] if open.is_punct(Punct::LBrace) && close.is_punct(Punct::RBrace) => {
            matches!(name.kind, TokKind::Ident(n) if cx.name(n).eq_ignore_ascii_case("za"))
        }
        _ => false,
    };
    if !za {
        let span = toks
            .first()
            .map_or(req.span, |t| t.span.to(toks[toks.len() - 1].span));
        cx.error(span, "`zero` takes `{za}`");
        return None;
    }
    one(0xc008_00ff)
}

fn msr(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let dst = i.op(0)?;
    // `msr <pstate-field>, #imm` is a different encoding from `msr <sysreg>,
    // Xt`. `spsel` names both a PSTATE field and a system register, so the
    // source operand decides.
    if let Some(n) = dst.word()
        && i.op(1).is_some_and(|o| o.reg().is_none())
    {
        let text = cx.name(n).to_ascii_lowercase();
        if let Some((word, lsb, max)) = sysreg::pstate_field(&text) {
            // The field decides how much of `CRm` the immediate is: a bit
            // for the one-bit fields and for SME's mode switches, all four
            // for `daifset` and `daifclr`.
            let imm = i.imm(cx, 1, 0, max, "a PSTATE value")? as u32;
            return one(word | imm << lsb);
        }
    }
    let enc = sysreg::register(cx, dst, Some(true))?;
    let rt = i.gpr(cx, 1)?;
    if rt.class != RegClass::X {
        cx.error(i.ops[1].span, "`msr` reads a 64-bit register");
        return None;
    }
    one(0xd510_0000 | enc | field(rt.num as u32, 0, 5))
}

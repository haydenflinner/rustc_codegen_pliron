//! Turning matched operands into instruction words.
//!
//! Every MIPS instruction is exactly one 32-bit word, so encoding is a matter
//! of ORing operand fields into the definition's fixed bits. What is not
//! trivial is the values the assembler cannot compute yet: those become
//! [`Fixup`]s carrying a [`FieldEncoding::Scatter`](crate::section::FieldEncoding::Scatter)
//! function that knows where
//! in the word the field lives.

use super::insn::{Def, Form};
use super::operand::{Imm, Operand, RelocMod};
use super::reg::{self, Reg};
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::ExprRef;
use crate::section::{Fixup, FixupKind, LinkValue, Variant};
use crate::source::Span;

// ---- field placement ------------------------------------------------------

pub const fn rs(r: u8) -> u32 {
    (r as u32) << 21
}
pub const fn rt(r: u8) -> u32 {
    (r as u32) << 16
}
pub const fn rd(r: u8) -> u32 {
    (r as u32) << 11
}
pub const fn sa(v: u32) -> u32 {
    (v & 0x1f) << 6
}
pub const fn imm(v: i64) -> u32 {
    (v as u32) & 0xffff
}

// ---- scatter functions ----------------------------------------------------
//
// Each takes the word already emitted and the resolved value, and returns the
// patched word. They are `fn` pointers stored in the fixup, so the bit layout
// stays next to the instruction that uses it.

/// A plain 16-bit immediate in the low half of an I-format word.
fn field_imm16(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | ((v as u64) & 0xffff)
}

/// `%hi`: bits 31..16, biased by 0x8000 so that adding the *sign-extended*
/// `%lo` of the same address gets back to the address itself.
fn field_hi16(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | (((v.wrapping_add(0x8000) >> 16) as u64) & 0xffff)
}

/// I-format branch: a 16-bit field counted in instruction words.
fn field_branch16(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | (((v >> 2) as u64) & 0xffff)
}

/// J-format target: 26 bits of *word index*, not a displacement. The hardware
/// forms the destination as `(delay_slot_pc & 0xf0000000) | (index << 2)`, so
/// the top four bits of the target come from where the jump sits, not from
/// the instruction. Only bits 27..2 are stored.
fn field_target26(word: u64, v: i64) -> u64 {
    (word & 0xfc00_0000) | (((v >> 2) as u64) & 0x03ff_ffff)
}

/// The fixup kind for a conditional branch. Branches are measured from the
/// delay slot, one word past the branch itself, which is what `adjust = 4`
/// says. 18 bits of value: 16 encoded plus the two the word alignment supplies.
///
/// The operand is always a target *address*, even when it is a bare number:
/// `beq $a0, $a1, 8` branches to address 8, as a label at 8 would. llvm-mc
/// reads a bare number as the displacement itself instead, so the corpus only
/// ever branches to labels.
pub fn branch_fixup() -> FixupKind {
    FixupKind::pcrel(4, 4)
        .with_field(18, 4)
        .with_reloc(reloc::PC16)
        .scatter(field_branch16)
}

/// The fixup kind for `j` / `jal`. The value is the absolute target, and it is
/// range-checked as a full 64-bit address: `jal 0x80001000` from kernel code
/// at 0x80000000 is perfectly legal, even though the target does not fit in 28
/// bits. What would *not* be legal is a target in a different 256 MB region
/// from the delay slot, for a label. That check needs the instruction's own
/// address, so rsasm makes it only in a flat image, where it does what GNU ld
/// does at link time; alignment is always checked.
pub fn jump_fixup() -> FixupKind {
    FixupKind::data(4)
        .with_field(64, 4)
        .with_reloc(reloc::R26)
        .link(LinkValue::Region {
            bits: 28,
            numbers: false,
        })
        .scatter(field_target26)
}

fn imm16_fixup(modifier: RelocMod) -> FixupKind {
    let base = FixupKind::data(4).scatter(field_imm16);
    match modifier {
        RelocMod::Hi => FixupKind::data(4)
            .with_reloc(reloc::HI16)
            .scatter(field_hi16),
        // `%lo` deliberately truncates, so the field takes any 32-bit value.
        RelocMod::Lo => base.with_reloc(reloc::LO16),
        // A bare immediate must actually fit; it gets the same relocation as
        // `%lo` because both name the low half of an address.
        RelocMod::None => base.with_field(16, 1).with_reloc(reloc::LO16),
    }
}

// ---- the word builder -----------------------------------------------------

/// The instruction words of one statement, plus the fixups into them.
///
/// Most statements produce one word; macro expansions such as `li` produce
/// two, and the fixup offsets have to follow.
pub struct Words {
    words: Vec<u32>,
    fixups: Vec<Fixup>,
    endian: crate::arch::Endian,
}

impl Words {
    pub fn new(endian: crate::arch::Endian) -> Words {
        Words {
            words: Vec::new(),
            fixups: Vec::new(),
            endian,
        }
    }

    pub fn push(&mut self, word: u32) {
        self.words.push(word);
    }

    /// Emits `word` with `expr` to be patched into it once it is known.
    pub fn push_fixup(&mut self, word: u32, expr: ExprRef, kind: FixupKind, span: Span) {
        let offset = (self.words.len() * 4) as u32;
        self.words.push(word);
        self.fixups.push(Fixup {
            offset,
            expr,
            kind,
            span,
        });
    }

    pub fn finish(self) -> Variant {
        let mut bytes = Vec::with_capacity(self.words.len() * 4);
        for w in &self.words {
            bytes.extend_from_slice(&self.endian.bytes(*w as u64, 4));
        }
        Variant {
            bytes,
            fixups: self.fixups,
        }
    }
}

// ---- immediates -----------------------------------------------------------

/// What a 16-bit immediate field accepts.
///
/// GNU as and llvm-mc both take either spelling of the same bit pattern, so
/// `ori $a0, $a0, 0xffff` and `addiu $a0, $a0, -1` are each fine and mean
/// what they look like.
pub const IMM16_LO: i64 = -0x8000;
pub const IMM16_HI: i64 = 0xffff;

/// Places a 16-bit immediate: folds it into `word` if it is already known,
/// and otherwise records a fixup.
pub fn place_imm16(
    cx: &mut AsmCtx<'_>,
    w: &mut Words,
    word: u32,
    v: Imm,
    what: &str,
) -> Option<()> {
    match v.modifier {
        RelocMod::None => match cx.constant(v.expr) {
            Some(n) => {
                if !(IMM16_LO..=IMM16_HI).contains(&n) {
                    cx.error(
                        v.span,
                        format!("{what} {n} does not fit in a 16-bit field (-32768 to 65535)"),
                    );
                    return None;
                }
                w.push(word | imm(n));
            }
            None => w.push_fixup(word, v.expr, imm16_fixup(RelocMod::None), v.span),
        },
        RelocMod::Hi => match cx.constant(v.expr) {
            Some(n) => w.push(word | imm(n.wrapping_add(0x8000) >> 16)),
            None => w.push_fixup(word, v.expr, imm16_fixup(RelocMod::Hi), v.span),
        },
        RelocMod::Lo => match cx.constant(v.expr) {
            Some(n) => w.push(word | imm(n)),
            None => w.push_fixup(word, v.expr, imm16_fixup(RelocMod::Lo), v.span),
        },
    }
    Some(())
}

/// Places a memory operand's displacement, which defaults to zero.
fn place_disp(cx: &mut AsmCtx<'_>, w: &mut Words, word: u32, disp: Option<Imm>) -> Option<()> {
    match disp {
        None => {
            w.push(word);
            Some(())
        }
        Some(d) => place_imm16(cx, w, word, d, "displacement"),
    }
}

// ---- operand access -------------------------------------------------------

/// The operand list of one statement, with the checks every accessor needs.
pub struct Args<'o> {
    pub mnemonic: &'o str,
    pub ops: &'o [Operand],
    pub span: Span,
}

impl Args<'_> {
    /// Requires an exact operand count.
    pub fn arity(&self, cx: &mut AsmCtx<'_>, n: usize) -> Option<()> {
        self.arity_between(cx, n, n)
    }

    pub fn arity_between(&self, cx: &mut AsmCtx<'_>, lo: usize, hi: usize) -> Option<()> {
        if self.ops.len() < lo || self.ops.len() > hi {
            let want = if lo == hi {
                format!("{lo}")
            } else {
                format!("{lo} to {hi}")
            };
            cx.error(
                self.span,
                format!(
                    "`{}` takes {want} operand(s), but {} were given",
                    self.mnemonic,
                    self.ops.len()
                ),
            );
            return None;
        }
        Some(())
    }

    fn at(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<&Operand> {
        match self.ops.get(i) {
            Some(o) => Some(o),
            None => {
                cx.error(self.span, format!("`{}`: missing operand", self.mnemonic));
                None
            }
        }
    }

    pub fn gpr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let o = self.at(cx, i)?;
        match o.gpr() {
            Some(r) => Some(r),
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be an integer register, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    pub fn fpr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let o = self.at(cx, i)?;
        match o.fpr() {
            Some(r) => Some(r),
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be a floating-point register, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    pub fn imm(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Imm> {
        let o = self.at(cx, i)?;
        match o.imm() {
            Some(v) => Some(v),
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be an immediate, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    pub fn mem(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<super::operand::Mem> {
        let o = self.at(cx, i)?;
        match o.mem() {
            Some(m) => Some(m),
            // `lw $a0, 8` with no base register is the commonest slip here,
            // and `describe` alone would not say what is missing.
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be `offset(base)`, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    /// A constant in `0..=max`, for shift amounts and trap codes.
    pub fn small_const(&self, cx: &mut AsmCtx<'_>, i: usize, max: u32, what: &str) -> Option<u32> {
        let v = self.imm(cx, i)?;
        let Some(n) = cx.constant(v.expr) else {
            cx.error(v.span, format!("{what} must be a constant"));
            return None;
        };
        if n < 0 || n > max as i64 {
            cx.error(v.span, format!("{what} {n} is out of range (0 to {max})"));
            return None;
        }
        Some(n as u32)
    }
}

// ---- the encoder ----------------------------------------------------------

/// Encodes one table-driven instruction.
pub fn encode(
    cx: &mut AsmCtx<'_>,
    def: &Def,
    a: &Args<'_>,
    endian: crate::arch::Endian,
    is64: bool,
) -> Option<Variant> {
    if def.is64 && !is64 {
        cx.error(
            a.span,
            format!(
                "`{}` is a 64-bit instruction; this target is 32-bit MIPS",
                a.mnemonic
            ),
        );
        return None;
    }
    let mut w = Words::new(endian);
    let base = def.word;
    match def.form {
        Form::RdRsRt => {
            a.arity(cx, 3)?;
            let (d, s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?, a.gpr(cx, 2)?);
            w.push(base | rd(d.num) | rs(s.num) | rt(t.num));
        }
        Form::RdRtSa => {
            a.arity(cx, 3)?;
            let (d, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let n = a.small_const(cx, 2, 31, "shift amount")?;
            w.push(base | rd(d.num) | rt(t.num) | sa(n));
        }
        Form::RdRtRs => {
            a.arity(cx, 3)?;
            let (d, t, s) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?, a.gpr(cx, 2)?);
            w.push(base | rd(d.num) | rt(t.num) | rs(s.num));
        }
        Form::RsRt => {
            a.arity(cx, 2)?;
            let (s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            w.push(base | rs(s.num) | rt(t.num));
        }
        Form::Div => {
            a.arity_between(cx, 2, 3)?;
            // Two operands: a plain divide. llvm-mc instead expands this
            // spelling into a divide guarded by a trap on a zero divisor. That
            // guard is a policy choice rather than an encoding, and silently
            // adding instructions is exactly what this backend avoids, so the
            // corpus compares only the three-operand form.
            let skip = if a.ops.len() == 3 {
                let z = a.gpr(cx, 0)?;
                if z != reg::ZERO {
                    let span = a.ops.first().map_or(a.span, |o| o.span);
                    cx.error(
                        span,
                        format!(
                            "`{}` leaves its result in HI and LO, so a destination \
                             operand must be $zero",
                            a.mnemonic
                        ),
                    );
                    return None;
                }
                1
            } else {
                0
            };
            let (s, t) = (a.gpr(cx, skip)?, a.gpr(cx, skip + 1)?);
            w.push(base | rs(s.num) | rt(t.num));
        }
        Form::Trap => {
            a.arity_between(cx, 2, 3)?;
            let (s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let code = match a.ops.len() {
                3 => a.small_const(cx, 2, 0x3ff, "trap code")?,
                _ => 0,
            };
            w.push(base | rs(s.num) | rt(t.num) | (code << 6));
        }
        Form::Rd => {
            a.arity(cx, 1)?;
            w.push(base | rd(a.gpr(cx, 0)?.num));
        }
        Form::Rs => {
            a.arity(cx, 1)?;
            w.push(base | rs(a.gpr(cx, 0)?.num));
        }
        Form::Jalr => {
            a.arity_between(cx, 1, 2)?;
            let (d, s) = if a.ops.len() == 1 {
                (reg::RA, a.gpr(cx, 0)?)
            } else {
                (a.gpr(cx, 0)?, a.gpr(cx, 1)?)
            };
            w.push(base | rd(d.num) | rs(s.num));
        }
        Form::RtRsImm => {
            a.arity(cx, 3)?;
            let (t, s) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let v = a.imm(cx, 2)?;
            place_imm16(cx, &mut w, base | rt(t.num) | rs(s.num), v, "immediate")?;
        }
        Form::RtImm => {
            a.arity(cx, 2)?;
            let t = a.gpr(cx, 0)?;
            let v = a.imm(cx, 1)?;
            place_imm16(cx, &mut w, base | rt(t.num), v, "immediate")?;
        }
        Form::RtMem => {
            a.arity(cx, 2)?;
            let t = a.gpr(cx, 0)?;
            let m = a.mem(cx, 1)?;
            place_disp(cx, &mut w, base | rt(t.num) | rs(m.base.num), m.disp)?;
        }
        Form::FtMem => {
            a.arity(cx, 2)?;
            let t = a.fpr(cx, 0)?;
            let m = a.mem(cx, 1)?;
            place_disp(cx, &mut w, base | rt(t.num) | rs(m.base.num), m.disp)?;
        }
        Form::RsRtOff => {
            a.arity(cx, 3)?;
            let (s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let target = a.imm(cx, 2)?;
            w.push_fixup(
                base | rs(s.num) | rt(t.num),
                target.expr,
                branch_fixup(),
                target.span,
            );
        }
        Form::RsOff => {
            a.arity(cx, 2)?;
            let s = a.gpr(cx, 0)?;
            let target = a.imm(cx, 1)?;
            w.push_fixup(base | rs(s.num), target.expr, branch_fixup(), target.span);
        }
        Form::Off => {
            a.arity(cx, 1)?;
            let target = a.imm(cx, 0)?;
            w.push_fixup(base, target.expr, branch_fixup(), target.span);
        }
        Form::Off26 => {
            a.arity(cx, 1)?;
            let target = a.imm(cx, 0)?;
            w.push_fixup(base, target.expr, jump_fixup(), target.span);
        }
        Form::Nullary => {
            a.arity(cx, 0)?;
            w.push(base);
        }
        Form::Break => {
            a.arity_between(cx, 0, 2)?;
            // `break` splits its 20 bits into two fields, and a single operand
            // fills only the upper one; that is what debuggers look at.
            let code1 = match a.ops.len() {
                0 => 0,
                _ => a.small_const(cx, 0, 0x3ff, "break code")?,
            };
            let code2 = match a.ops.len() {
                2 => a.small_const(cx, 1, 0x3ff, "break code")?,
                _ => 0,
            };
            w.push(base | (code1 << 16) | (code2 << 6));
        }
        Form::Code20 => {
            a.arity_between(cx, 0, 1)?;
            let code = match a.ops.len() {
                0 => 0,
                _ => a.small_const(cx, 0, 0xf_ffff, "code")?,
            };
            w.push(base | (code << 6));
        }
        Form::Sync => {
            a.arity_between(cx, 0, 1)?;
            let stype = match a.ops.len() {
                0 => 0,
                _ => a.small_const(cx, 0, 31, "sync type")?,
            };
            w.push(base | sa(stype));
        }
        Form::FdFsFt => {
            a.arity(cx, 3)?;
            let (fd, fs, ft) = (a.fpr(cx, 0)?, a.fpr(cx, 1)?, a.fpr(cx, 2)?);
            w.push(base | sa(fd.num as u32) | rd(fs.num) | rt(ft.num));
        }
        Form::FdFs => {
            a.arity(cx, 2)?;
            let (fd, fs) = (a.fpr(cx, 0)?, a.fpr(cx, 1)?);
            w.push(base | sa(fd.num as u32) | rd(fs.num));
        }
        Form::FsFt => {
            a.arity(cx, 2)?;
            let (fs, ft) = (a.fpr(cx, 0)?, a.fpr(cx, 1)?);
            w.push(base | rd(fs.num) | rt(ft.num));
        }
        Form::RtFs => {
            a.arity(cx, 2)?;
            let t = a.gpr(cx, 0)?;
            let fs = a.fpr(cx, 1)?;
            w.push(base | rt(t.num) | rd(fs.num));
        }
        Form::RtRdSel => {
            a.arity_between(cx, 2, 3)?;
            let t = a.gpr(cx, 0)?;
            let d = a.gpr(cx, 1)?;
            let sel = match a.ops.len() {
                3 => a.small_const(cx, 2, 7, "coprocessor register select")?,
                _ => 0,
            };
            w.push(base | rt(t.num) | rd(d.num) | sel);
        }
    }
    Some(w.finish())
}

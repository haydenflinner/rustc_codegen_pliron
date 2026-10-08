//! A64 operand parsing.
//!
//! A64 operands are separated by commas all the way down, which makes the
//! grammar much flatter than x86's: a shift or extend applied to a register is
//! written as its *own* comma-separated operand (`add x0, x1, x2, lsl #3`), so
//! the parser can treat the operand list as a flat sequence and let each
//! instruction decide what the pieces mean.
//!
//! The one nested construct is the addressing mode, `[base, ...]`, with the
//! writeback marker `!` and the post-index offset sitting outside the
//! brackets.
//!
//! Every operand keeps the token slice it came from so that an instruction can
//! re-read a piece as an expression. That matters because A64 keywords are not
//! reserved: `b eq` is a branch to a label called `eq`, and only the
//! instruction knows which reading is wanted.

use super::reg::{self, Reg, RegClass, VecReg};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::intern::Name;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// A shift applied to a register operand or an immediate.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ShiftOp {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

impl ShiftOp {
    /// The two-bit `shift` field of the shifted-register data-processing
    /// forms. `ror` is only legal for the logical group.
    pub fn code(self) -> u32 {
        match self {
            ShiftOp::Lsl => 0,
            ShiftOp::Lsr => 1,
            ShiftOp::Asr => 2,
            ShiftOp::Ror => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ShiftOp::Lsl => "lsl",
            ShiftOp::Lsr => "lsr",
            ShiftOp::Asr => "asr",
            ShiftOp::Ror => "ror",
        }
    }

    pub fn parse(name: &str) -> Option<ShiftOp> {
        Some(match name {
            "lsl" => ShiftOp::Lsl,
            "lsr" => ShiftOp::Lsr,
            "asr" => ShiftOp::Asr,
            "ror" => ShiftOp::Ror,
            _ => return None,
        })
    }
}

/// A sign- or zero-extension of a narrow register, as used by the extended
/// add/subtract forms and by register-offset addressing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExtendOp {
    Uxtb,
    Uxth,
    Uxtw,
    Uxtx,
    Sxtb,
    Sxth,
    Sxtw,
    Sxtx,
}

impl ExtendOp {
    /// The three-bit `option` field.
    pub fn code(self) -> u32 {
        match self {
            ExtendOp::Uxtb => 0,
            ExtendOp::Uxth => 1,
            ExtendOp::Uxtw => 2,
            ExtendOp::Uxtx => 3,
            ExtendOp::Sxtb => 4,
            ExtendOp::Sxth => 5,
            ExtendOp::Sxtw => 6,
            ExtendOp::Sxtx => 7,
        }
    }

    /// Which register width the extension reads from. `uxtx`/`sxtx` take the
    /// full 64-bit register; everything else takes a `w` register.
    pub fn source_class(self) -> RegClass {
        match self {
            ExtendOp::Uxtx | ExtendOp::Sxtx => RegClass::X,
            _ => RegClass::W,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ExtendOp::Uxtb => "uxtb",
            ExtendOp::Uxth => "uxth",
            ExtendOp::Uxtw => "uxtw",
            ExtendOp::Uxtx => "uxtx",
            ExtendOp::Sxtb => "sxtb",
            ExtendOp::Sxth => "sxth",
            ExtendOp::Sxtw => "sxtw",
            ExtendOp::Sxtx => "sxtx",
        }
    }

    pub fn parse(name: &str) -> Option<ExtendOp> {
        Some(match name {
            "uxtb" => ExtendOp::Uxtb,
            "uxth" => ExtendOp::Uxth,
            "uxtw" => ExtendOp::Uxtw,
            "uxtx" => ExtendOp::Uxtx,
            "sxtb" => ExtendOp::Sxtb,
            "sxth" => ExtendOp::Sxth,
            "sxtw" => ExtendOp::Sxtw,
            "sxtx" => ExtendOp::Sxtx,
            _ => return None,
        })
    }
}

/// The index register of a register-offset addressing mode.
#[derive(Copy, Clone, Debug)]
pub struct IndexReg {
    pub reg: Reg,
    pub ext: ExtendOp,
    /// The shift amount written after the extend, if any. `None` and
    /// `Some(0)` are different: for a byte access an explicit `lsl #0` sets
    /// the `S` bit and an absent one does not.
    pub amount: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum MemKind {
    /// `[x0]` or `[x0, #off]`.
    Offset(Option<ExprRef>),
    /// `[x0, #off]!` — writes the new address back to the base.
    PreIndex(ExprRef),
    /// `[x0], #off` — uses the old address, then writes back.
    PostIndex(ExprRef),
    /// `[x0, x1]`, `[x0, x1, lsl #3]`, `[x0, w1, uxtw #2]`.
    Reg(IndexReg),
    /// `[x0, :lo12:sym]` — an offset the linker fills in.
    OffsetReloc(RelocOp, ExprRef),
}

/// A `:name:` relocation operator written in front of an expression.
///
/// These select *which part* of a symbol's address a field receives, which is
/// how A64 builds a full address out of instructions that each hold only a
/// piece of one: `adrp x0, sym` then `add x0, x0, :lo12:sym`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RelocOp {
    /// The low 12 bits of the address.
    Lo12,
    /// The page of the symbol's GOT entry.
    Got,
    /// The low 12 bits of the symbol's GOT entry.
    GotLo12,
}

impl RelocOp {
    pub fn name(self) -> &'static str {
        match self {
            RelocOp::Lo12 => ":lo12:",
            RelocOp::Got => ":got:",
            RelocOp::GotLo12 => ":got_lo12:",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Mem {
    pub base: Reg,
    pub kind: MemKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum OperandKind {
    Reg(Reg),
    #[allow(dead_code)]
    Vec(VecReg),
    /// A vector element: `v0.s[2]`.
    #[allow(dead_code)]
    VecElem(VecReg, u64),
    Imm(ExprRef),
    /// `ldr x0, =expr`: the value goes in a literal pool and the
    /// instruction loads it from there.
    Literal(ExprRef),
    /// An expression under a relocation operator: `:lo12:sym`.
    Reloc(RelocOp, ExprRef),
    Mem(Mem),
    /// A shift written as its own operand: `lsl #3`.
    Shift(ShiftOp, ExprRef),
    /// An extend written as its own operand: `sxtw`, `uxtb #2`.
    Extend(ExtendOp, Option<ExprRef>),
    Cond(u8),
    /// A bare identifier that is none of the above: a barrier option (`ish`),
    /// a system register (`nzcv`), a prefetch hint (`pldl1keep`).
    Word(Name),
}

#[derive(Clone, Debug)]
pub struct Operand<'t> {
    pub kind: OperandKind,
    /// The tokens this operand was parsed from, so that a keyword-shaped
    /// operand can be re-read as an expression when the instruction wants one.
    pub toks: &'t [Token],
    pub span: Span,
}

impl Operand<'_> {
    pub fn reg(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) => Some(r),
            _ => None,
        }
    }

    pub fn cond(&self) -> Option<u8> {
        match self.kind {
            OperandKind::Cond(c) => Some(c),
            _ => None,
        }
    }

    pub fn mem(&self) -> Option<&Mem> {
        match &self.kind {
            OperandKind::Mem(m) => Some(m),
            _ => None,
        }
    }

    pub fn word(&self) -> Option<Name> {
        match self.kind {
            OperandKind::Word(n) => Some(n),
            _ => None,
        }
    }

    /// Reads this operand as an expression, re-parsing its tokens when the
    /// eager pass classified it as something else.
    pub fn expr(&self, cx: &mut AsmCtx<'_>) -> Option<ExprRef> {
        match self.kind {
            OperandKind::Imm(e) => return Some(e),
            // An instruction that understands relocation operators matches on
            // `Reloc` itself; reaching here means this one does not.
            // Except Darwin's spelling, which llvm-mc takes anywhere a label
            // can go and leaves to the relocation to make sense of; see
            // `Architecture::modifier_class`.
            OperandKind::Reloc(_, e) if cx.find_modifier_for(e).is_some() => return Some(e),
            OperandKind::Reloc(op, _) => {
                cx.error(
                    self.span,
                    format!("`{}` is not valid in this operand", op.name()),
                );
                return None;
            }
            // Register names are reserved, unlike condition and option names:
            // `b x0` is a mistake for `br x0`, not a branch to a label.
            OperandKind::Literal(_) => {
                cx.error(
                    self.span,
                    "`=` puts a value in a literal pool, which only `ldr` loads from",
                );
                return None;
            }
            OperandKind::Reg(_)
            | OperandKind::Vec(_)
            | OperandKind::VecElem(..)
            | OperandKind::Mem(_) => {
                cx.error(
                    self.span,
                    format!("expected an expression, found {}", self.describe()),
                );
                return None;
            }
            _ => {}
        }
        let mut cur = Cursor::new(self.toks);
        // A `#` sigil is optional in A64 and never changes the meaning.
        cur.eat_punct(Punct::Hash);
        let e = cx.expr_parser().parse(&mut cur)?;
        if !cur.at_end() {
            cx.error(cur.peek().span, "unexpected token after an expression");
            return None;
        }
        Some(e)
    }

    pub fn describe(&self) -> String {
        match &self.kind {
            OperandKind::Reg(r) => format!("register `{}`", r.name()),
            OperandKind::Vec(_) | OperandKind::VecElem(..) => "a vector operand".into(),
            OperandKind::Imm(_) => "an immediate".into(),
            OperandKind::Literal(_) => "a literal-pool value".into(),
            OperandKind::Reloc(op, _) => format!("a `{}` expression", op.name()),
            OperandKind::Mem(_) => "a memory operand".into(),
            OperandKind::Shift(s, _) => format!("a `{}` shift", s.name()),
            OperandKind::Extend(e, _) => format!("an `{}` extend", e.name()),
            OperandKind::Cond(c) => format!("condition `{}`", reg::cond_name(*c)),
            OperandKind::Word(_) => "a keyword".into(),
        }
    }
}

/// Splits an operand list and parses each piece.
pub fn parse_list<'t>(cx: &mut AsmCtx<'_>, cur: &Cursor<'t>) -> Option<Vec<Operand<'t>>> {
    let pieces = cur.split_commas();
    let mut out = Vec::with_capacity(pieces.len());
    // A post-index offset (`[x0], #8`) is a separate comma piece that belongs
    // to the address before it, so the memory parser may claim two pieces.
    let mut i = 0;
    while i < pieces.len() {
        let piece = pieces[i];
        if piece.is_empty() {
            cx.error(cur.remaining_span(), "empty operand");
            return None;
        }
        if piece.first().is_some_and(|t| t.is_punct(Punct::LBracket)) {
            let (op, used) = parse_memory(cx, &pieces[i..])?;
            out.push(op);
            i += used;
            continue;
        }
        out.push(parse_one(cx, piece)?);
        i += 1;
    }
    Some(out)
}

fn ident_text(cx: &AsmCtx<'_>, t: &Token) -> Option<String> {
    match t.kind {
        TokKind::Ident(n) => Some(cx.name(n).to_ascii_lowercase()),
        _ => None,
    }
}

/// Parses one comma-separated operand that is not an addressing mode.
fn parse_one<'t>(cx: &mut AsmCtx<'_>, toks: &'t [Token]) -> Option<Operand<'t>> {
    let span = span_of(toks);
    let mk = |kind| Some(Operand { kind, toks, span });

    // `ldr x0, =0x12345678`: the value belongs in a literal pool.
    if toks[0].is_punct(Punct::Eq) {
        let mut cur = Cursor::new(&toks[1..]);
        let e = cx.expr_parser().parse(&mut cur)?;
        if !cur.at_end() {
            cx.error(cur.peek().span, "unexpected token after a literal value");
            return None;
        }
        return mk(OperandKind::Literal(e));
    }

    if let Some(first) = toks.first()
        && let Some(word) = ident_text(cx, first)
    {
        // A single bare word: register, condition, extend or option name.
        if toks.len() == 1 {
            if let Some(r) = reg::lookup(&word) {
                return mk(OperandKind::Reg(r));
            }
            if let Some(v) = reg::vector(&word) {
                return mk(OperandKind::Vec(v));
            }
            if let Some(c) = reg::cond(&word) {
                return mk(OperandKind::Cond(c));
            }
            if let Some(e) = ExtendOp::parse(&word) {
                return mk(OperandKind::Extend(e, None));
            }
            // `lsl` with no amount is not a legal operand, but `nzcv`, `ish`
            // and the prefetch hints are; leave the word for the instruction.
            if ShiftOp::parse(&word).is_none()
                && let TokKind::Ident(n) = first.kind
            {
                return mk(OperandKind::Word(n));
            }
        }

        // `v0.s[2]`: an indexed vector element.
        if let Some(v) = reg::vector(&word)
            && toks.len() == 4
        {
            let mut cur = Cursor::new(&toks[1..]);
            if cur.eat_punct(Punct::LBracket).is_some()
                && let TokKind::Int(i) = cur.peek().kind
            {
                cur.advance();
                if cur.eat_punct(Punct::RBracket).is_some() {
                    return mk(OperandKind::VecElem(v, i));
                }
            }
        }

        // `lsl #3` / `asr #7`, and `sxtw #2`.
        if let Some(s) = ShiftOp::parse(&word) {
            let e = trailing_expr(cx, &toks[1..], first.span)?;
            return mk(OperandKind::Shift(s, e));
        }
        if let Some(x) = ExtendOp::parse(&word) {
            let e = trailing_expr(cx, &toks[1..], first.span)?;
            return mk(OperandKind::Extend(x, Some(e)));
        }
    }

    match immediate(cx, toks)? {
        (None, e) => mk(OperandKind::Imm(e)),
        (Some(op), e) => mk(OperandKind::Reloc(op, e)),
    }
}

/// Parses `[#][:op:]expr` covering all of `toks`.
fn immediate(cx: &mut AsmCtx<'_>, toks: &[Token]) -> Option<(Option<RelocOp>, ExprRef)> {
    let mut cur = Cursor::new(toks);
    cur.eat_punct(Punct::Hash);
    let mut op = None;
    if let Some(colon) = cur.eat_punct(Punct::Colon) {
        let name = cur.peek();
        let text = ident_text(cx, &name);
        op = match text.as_deref() {
            Some("lo12") => Some(RelocOp::Lo12),
            Some("got") => Some(RelocOp::Got),
            Some("got_lo12") => Some(RelocOp::GotLo12),
            Some(other) => {
                cx.error(
                    colon.span.to(name.span),
                    format!("unsupported relocation operator `:{other}:`"),
                );
                return None;
            }
            None => {
                cx.error(
                    colon.span.to(name.span),
                    "expected a relocation operator name after `:`",
                );
                return None;
            }
        };
        cur.advance();
        if cur.eat_punct(Punct::Colon).is_none() {
            cx.error(
                cur.peek().span,
                "expected `:` to close a relocation operator",
            );
            return None;
        }
    }
    let e = cx.expr_parser().parse(&mut cur)?;
    if !cur.at_end() {
        cx.error(cur.peek().span, "unexpected token after an operand");
        return None;
    }
    // Darwin spells `:lo12:sym` as `sym@PAGEOFF`, `:got_lo12:sym` as
    // `sym@GOTPAGEOFF` and `:got:sym` as `sym@GOTPAGE`; `sym@PAGE` is what
    // `adrp` takes anyway. The modifier stays on the expression, so the
    // relocation can still be told which spelling it came from.
    if op.is_none()
        && let Some(m) = cx.find_modifier_for(e)
    {
        op = match cx.name(m) {
            "pageoff" => Some(RelocOp::Lo12),
            "gotpageoff" => Some(RelocOp::GotLo12),
            "gotpage" => Some(RelocOp::Got),
            _ => None,
        };
    }
    Some((op, e))
}

/// Parses the `#imm` that follows a shift or extend keyword.
fn trailing_expr(cx: &mut AsmCtx<'_>, toks: &[Token], kw: Span) -> Option<ExprRef> {
    if toks.is_empty() {
        cx.error(kw, "expected a shift amount");
        return None;
    }
    let mut cur = Cursor::new(toks);
    cur.eat_punct(Punct::Hash);
    let e = cx.expr_parser().parse(&mut cur)?;
    if !cur.at_end() {
        cx.error(cur.peek().span, "unexpected token after a shift amount");
        return None;
    }
    Some(e)
}

/// Parses `[base, ...]` plus whatever follows it, returning how many
/// comma-separated pieces it consumed.
///
/// `Cursor::split_commas` keeps bracketed text together, so the whole
/// `[...]` — writeback `!` included — arrives as one piece. A post-index
/// offset is written *after* the `]`, though, so it arrives as the next piece
/// and has to be claimed here.
fn parse_memory<'t>(cx: &mut AsmCtx<'_>, pieces: &[&'t [Token]]) -> Option<(Operand<'t>, usize)> {
    let toks = pieces[0];
    let Some(close) = toks.iter().position(|t| t.is_punct(Punct::RBracket)) else {
        cx.error(span_of(toks), "unterminated `[` in an address");
        return None;
    };
    let after = &toks[close + 1..];
    let writeback = after.first().is_some_and(|t| t.is_punct(Punct::Bang));
    if after.len() > usize::from(writeback) {
        cx.error(span_of(after), "unexpected token after `]`");
        return None;
    }

    let parts = split_commas(&toks[1..close]);
    let Some(base) = (match parts.first().copied() {
        Some([t]) => ident_text(cx, t).and_then(|w| reg::lookup(&w)),
        _ => None,
    }) else {
        cx.error(span_of(toks), "expected a base register after `[`");
        return None;
    };
    if base.class != RegClass::X {
        cx.error(span_of(toks), "an address base must be a 64-bit register");
        return None;
    }

    let span = span_of(toks);
    let mut used = 1;
    let kind = if parts.len() == 1 {
        if writeback {
            cx.error(span, "`!` needs an offset to write back");
            return None;
        }
        // `[x0], #off` — the offset is the next comma-separated piece.
        match pieces.get(1) {
            Some(next) if !next.is_empty() => {
                used += 1;
                let mut cur = Cursor::new(next);
                cur.eat_punct(Punct::Hash);
                let e = cx.expr_parser().parse(&mut cur)?;
                if !cur.at_end() {
                    cx.error(cur.peek().span, "unexpected token in a post-index offset");
                    return None;
                }
                MemKind::PostIndex(e)
            }
            _ => MemKind::Offset(None),
        }
    } else {
        parse_inner(cx, span, &parts[1..], writeback)?
    };

    Some((
        Operand {
            kind: OperandKind::Mem(Mem { base, kind, span }),
            toks,
            span,
        },
        used,
    ))
}

/// Splits a bracket's contents on commas. Nothing inside `[...]` nests, so a
/// depth counter would be wasted here.
fn split_commas(toks: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.is_punct(Punct::Comma) {
            out.push(&toks[start..i]);
            start = i + 1;
        }
    }
    out.push(&toks[start..]);
    out
}

/// Parses what sits between `[x0,` and `]`.
fn parse_inner(
    cx: &mut AsmCtx<'_>,
    span: Span,
    parts: &[&[Token]],
    writeback: bool,
) -> Option<MemKind> {
    if parts.is_empty() || parts[0].is_empty() {
        cx.error(span, "expected an offset after `,`");
        return None;
    }
    // A register index: `[x0, x1]`, `[x0, w1, uxtw #2]`.
    if parts[0].len() == 1
        && let Some(word) = ident_text(cx, &parts[0][0])
        && let Some(index) = reg::lookup(&word)
        && index.is_gpr()
    {
        if writeback {
            cx.error(span, "a register offset cannot be written back");
            return None;
        }
        // Register 31 in the index field is the zero register, so `sp` would
        // silently mean something else.
        if index.is_sp() {
            cx.error(span, "the stack pointer cannot be an index register");
            return None;
        }
        let (ext, amount) = match parts.get(1) {
            None => (
                // A bare `x` index means `lsl #0`, which shares an encoding
                // with `uxtx`; a bare `w` index is not encodable.
                if index.class == RegClass::X {
                    ExtendOp::Uxtx
                } else {
                    cx.error(span, "a 32-bit index needs `uxtw` or `sxtw`");
                    return None;
                },
                None,
            ),
            Some(rest) => {
                let op = parse_one(cx, rest)?;
                match op.kind {
                    OperandKind::Shift(ShiftOp::Lsl, e) => {
                        (ExtendOp::Uxtx, Some(shift_amount(cx, e)?))
                    }
                    OperandKind::Extend(x, e) => {
                        let n = match e {
                            Some(e) => Some(shift_amount(cx, e)?),
                            None => None,
                        };
                        (x, n)
                    }
                    _ => {
                        cx.error(op.span, "expected `lsl`, `uxtw`, `sxtw`, `uxtx` or `sxtx`");
                        return None;
                    }
                }
            }
        };
        if parts.len() > 2 {
            cx.error(span, "too many parts in an address");
            return None;
        }
        if ext.source_class() != index.class {
            cx.error(
                span,
                format!(
                    "`{}` needs a {} index register",
                    ext.name(),
                    ext.source_class().letter()
                ),
            );
            return None;
        }
        return Some(MemKind::Reg(IndexReg {
            reg: index,
            ext,
            amount,
        }));
    }

    if parts.len() > 1 {
        cx.error(span, "too many parts in an address");
        return None;
    }
    Some(match (immediate(cx, parts[0])?, writeback) {
        ((None, e), false) => MemKind::Offset(Some(e)),
        ((None, e), true) => MemKind::PreIndex(e),
        ((Some(op), e), false) => MemKind::OffsetReloc(op, e),
        ((Some(op), _), true) => {
            cx.error(
                span,
                format!("a `{}` offset cannot be written back", op.name()),
            );
            return None;
        }
    })
}

/// A shift amount inside an address must be known now: it selects between two
/// encodings rather than being placed in a relocatable field.
fn shift_amount(cx: &mut AsmCtx<'_>, e: ExprRef) -> Option<u64> {
    match cx.constant(e) {
        Some(v) if (0..=64).contains(&v) => Some(v as u64),
        _ => {
            let span = cx.exprs.span(e);
            cx.error(span, "a shift amount in an address must be a constant 0-64");
            None
        }
    }
}

pub fn span_of(toks: &[Token]) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => Span::DUMMY,
    }
}

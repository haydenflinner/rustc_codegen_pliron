//! SPARC operand parsing.
//!
//! SPARC operands are simple enough that one grammar covers all of them:
//!
//! ```text
//! %g1                 a register
//! 42                  an expression
//! %hi(sym) %lo(sym)   the two halves of a 32-bit constant
//! [%g1 + %g2]         a memory address
//! [%g1 - 8]  [%g1]    ... with an immediate, or none at all
//! %o7 + 8             the same address unbracketed, which is how `jmpl`,
//!                     `call`, `flush` and `return` spell their target
//! ```
//!
//! The one thing worth knowing is that `%hi(x)` is *not* the generic `x@hi`
//! modifier the shared expression parser understands: the sigil comes first
//! and the argument is parenthesised, so it has to be recognised here, before
//! the expression parser gets a chance to read `%` as a remainder operator.

use super::reg::{self, Reg, RegClass};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::{ExprKind, ExprRef, UnOp};
use crate::lexer::{Punct, TokKind};
use crate::source::Span;

/// Which part of a value an immediate refers to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ImmPart {
    /// The value itself.
    Whole,
    /// `%hi(x)`: bits 31-10, which is exactly what `sethi` writes.
    Hi,
    /// `%lo(x)`: bits 9-0, which always fit a 13-bit signed field.
    Lo,
}

#[derive(Copy, Clone, Debug)]
pub struct Imm {
    pub part: ImmPart,
    pub expr: ExprRef,
    pub span: Span,
}

/// What is added to an address's base register.
#[derive(Copy, Clone, Debug)]
pub enum Offset {
    /// `[%g1]`: no offset at all. Encoded as `+ %g0` with the `i` bit clear,
    /// which is what both GNU as and llvm-mc emit, and is deliberately *not*
    /// the same word as the `[%g1 + 0]` an explicit zero produces.
    None,
    Reg(Reg),
    Imm(Imm),
}

#[derive(Copy, Clone, Debug)]
pub struct Addr {
    pub base: Reg,
    pub offset: Offset,
}

#[derive(Copy, Clone, Debug)]
pub enum OperandKind {
    Reg(Reg),
    Imm(Imm),
    /// `[...]`, a load or store address.
    Mem(Addr),
    /// `%o7 + 8`, an unbracketed address.
    Addr(Addr),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    pub span: Span,
}

impl Operand {
    pub fn reg(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) => Some(r),
            _ => None,
        }
    }

    pub fn int_reg(&self) -> Option<Reg> {
        self.reg().filter(Reg::is_int)
    }

    pub fn float_reg(&self) -> Option<Reg> {
        self.reg().filter(Reg::is_float)
    }

    pub fn imm(&self) -> Option<Imm> {
        match self.kind {
            OperandKind::Imm(i) => Some(i),
            _ => None,
        }
    }

    /// The address this operand denotes, if it can be read as one. A bare
    /// integer register is the address `[reg + %g0]`, which is how
    /// `jmpl %o7, %g0` and `flush %g1` are written.
    pub fn as_addr(&self) -> Option<Addr> {
        match self.kind {
            OperandKind::Mem(a) | OperandKind::Addr(a) => Some(a),
            OperandKind::Reg(r) if r.is_int() => Some(Addr {
                base: r,
                offset: Offset::None,
            }),
            _ => None,
        }
    }

    /// True for `%icc` / `%xcc` / `%fccN`, which is how a V9 predicted branch
    /// or conditional move says which condition-code bank it tests.
    pub fn is_cc(&self) -> bool {
        matches!(self.reg(), Some(r) if matches!(r.class, RegClass::Icc | RegClass::Fcc))
    }

    pub fn describe(&self) -> String {
        match self.kind {
            OperandKind::Reg(r) => format!("register `{}`", reg::name_of(r)),
            OperandKind::Imm(i) => match i.part {
                ImmPart::Whole => "an immediate".into(),
                ImmPart::Hi => "a `%hi()` immediate".into(),
                ImmPart::Lo => "a `%lo()` immediate".into(),
            },
            OperandKind::Mem(_) => "a memory operand".into(),
            OperandKind::Addr(_) => "an address".into(),
        }
    }
}

/// Span from `start` through the last token the cursor consumed.
fn consumed(cur: &Cursor<'_>, start: Span) -> Span {
    match cur.pos().checked_sub(1).and_then(|i| cur.all().get(i)) {
        Some(t) => start.to(t.span),
        None => start,
    }
}

pub struct OperandParser<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
}

impl OperandParser<'_, '_> {
    /// Parses every comma-separated operand left in `cur`.
    pub fn parse_list(&mut self, cur: &Cursor<'_>) -> Option<Vec<Operand>> {
        if cur.at_end() {
            return Some(Vec::new());
        }
        let pieces = cur.split_commas();
        let mut out = Vec::with_capacity(pieces.len());
        for piece in pieces {
            let mut pc = Cursor::new(piece);
            if pc.at_end() {
                self.cx.error(cur.remaining_span(), "empty operand");
                return None;
            }
            let o = self.parse(&mut pc)?;
            if !pc.at_end() {
                self.cx
                    .error(pc.peek().span, "unexpected token after operand");
                return None;
            }
            out.push(o);
        }
        Some(out)
    }

    pub fn parse(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;

        if cur.eat_punct(Punct::LBracket).is_some() {
            let a = self.address(cur)?;
            if cur.eat_punct(Punct::RBracket).is_none() {
                let t = cur.peek();
                self.cx.error(t.span, "expected `]` to close the address");
                return None;
            }
            return Some(Operand {
                kind: OperandKind::Mem(a),
                span: consumed(cur, start),
            });
        }

        if let Some(part) = self.peek_part(cur) {
            let imm = self.modifier(cur, part)?;
            return Some(Operand {
                kind: OperandKind::Imm(imm),
                span: imm.span,
            });
        }

        if self.peek_register(cur) {
            let r = self.register(cur)?;
            // Only an integer register can carry an offset, and only then is
            // this an address rather than a plain register operand.
            let kind =
                if r.is_int() && (cur.check_punct(Punct::Plus) || cur.check_punct(Punct::Minus)) {
                    OperandKind::Addr(Addr {
                        base: r,
                        offset: self.offset(cur)?,
                    })
                } else {
                    OperandKind::Reg(r)
                };
            return Some(Operand {
                kind,
                span: consumed(cur, start),
            });
        }

        let e = self.expr(cur)?;
        Some(Operand {
            kind: OperandKind::Imm(Imm {
                part: ImmPart::Whole,
                expr: e,
                span: consumed(cur, start),
            }),
            span: consumed(cur, start),
        })
    }

    /// `base` followed by an optional `+ offset` or `- offset`.
    fn address(&mut self, cur: &mut Cursor<'_>) -> Option<Addr> {
        let start = cur.peek().span;
        if !self.peek_register(cur) {
            self.cx
                .error(start, "an address must start with a base register");
            return None;
        }
        let base = self.register(cur)?;
        if !base.is_int() {
            self.cx.error(
                start,
                format!(
                    "`{}` cannot be an address base; only the integer registers can",
                    reg::name_of(base)
                ),
            );
            return None;
        }
        Some(Addr {
            base,
            offset: self.offset(cur)?,
        })
    }

    fn offset(&mut self, cur: &mut Cursor<'_>) -> Option<Offset> {
        let negate = if cur.eat_punct(Punct::Plus).is_some() {
            false
        } else if cur.eat_punct(Punct::Minus).is_some() {
            true
        } else {
            return Some(Offset::None);
        };
        let start = cur.peek().span;

        if let Some(part) = self.peek_part(cur) {
            if negate {
                self.cx
                    .error(start, "`%hi()` and `%lo()` cannot be negated here");
                return None;
            }
            return Some(Offset::Imm(self.modifier(cur, part)?));
        }
        if self.peek_register(cur) {
            let r = self.register(cur)?;
            if negate {
                self.cx
                    .error(start, "an index register cannot be subtracted");
                return None;
            }
            if !r.is_int() {
                self.cx.error(
                    start,
                    format!(
                        "`{}` cannot be an address index; only the integer registers can",
                        reg::name_of(r)
                    ),
                );
                return None;
            }
            return Some(Offset::Reg(r));
        }

        let e = self.expr(cur)?;
        let span = consumed(cur, start);
        let expr = if negate {
            self.cx.exprs.alloc(ExprKind::Unary(UnOp::Neg, e), span)
        } else {
            e
        };
        Some(Offset::Imm(Imm {
            part: ImmPart::Whole,
            expr,
            span,
        }))
    }

    fn expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let mut p = self.cx.expr_parser();
        p.parse(cur)
    }

    /// The identifier one token past a `%`, lowercased.
    fn sigil_word(&self, cur: &Cursor<'_>) -> Option<String> {
        if !cur.check_punct(Punct::Percent) {
            return None;
        }
        // A register name is never separated from its sigil, which is what
        // keeps the remainder in `x % hi` apart from the modifier in `%hi(x)`.
        let tok = cur.nth(1);
        if tok.preceded_by_space {
            return None;
        }
        let TokKind::Ident(n) = tok.kind else {
            return None;
        };
        Some(self.cx.interner.get(n).to_ascii_lowercase())
    }

    /// True for anything shaped like `%name`. An unrecognised name is still
    /// claimed here so that it is reported as an unknown register rather than
    /// falling through to the expression parser, which would only complain
    /// about the `%`.
    fn peek_register(&self, cur: &Cursor<'_>) -> bool {
        self.sigil_word(cur).is_some() && self.peek_part(cur).is_none()
    }

    /// `%hi(` or `%lo(`, which only count as modifiers when the parenthesis
    /// is actually there.
    fn peek_part(&self, cur: &Cursor<'_>) -> Option<ImmPart> {
        let part = match self.sigil_word(cur)?.as_str() {
            "hi" => ImmPart::Hi,
            "lo" => ImmPart::Lo,
            _ => return None,
        };
        cur.nth(2).is_punct(Punct::LParen).then_some(part)
    }

    fn modifier(&mut self, cur: &mut Cursor<'_>, part: ImmPart) -> Option<Imm> {
        let start = cur.peek().span;
        cur.advance(); // `%`
        cur.advance(); // `hi` / `lo`
        cur.advance(); // `(`
        let e = self.expr(cur)?;
        let close = cur.peek();
        if cur.eat_punct(Punct::RParen).is_none() {
            self.cx
                .error(close.span, "expected `)` to close `%hi(` or `%lo(`");
            return None;
        }
        Some(Imm {
            part,
            expr: e,
            span: start.to(close.span),
        })
    }

    fn register(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let pct = cur.advance(); // `%`
        let tok = cur.peek();
        let TokKind::Ident(n) = tok.kind else {
            self.cx
                .error(pct.span.to(tok.span), "expected a register name after `%`");
            return None;
        };
        cur.advance();
        let text = self.cx.interner.get(n).to_ascii_lowercase();
        match reg::lookup(&text) {
            Some(r) => Some(r),
            None => {
                self.cx
                    .error(pct.span.to(tok.span), format!("unknown register `%{text}`"));
                None
            }
        }
    }
}

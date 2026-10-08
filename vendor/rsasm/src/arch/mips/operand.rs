//! MIPS operand parsing.
//!
//! The grammar is small: a register, an expression, or an expression followed
//! by a base register in parentheses.
//!
//! ```text
//! $t0            register        ($sp)          memory, zero displacement
//! 8($sp)         memory          %hi(sym)       relocation modifier
//! -4             immediate       %lo(sym)($gp)  modifier plus base
//! ```
//!
//! The shared lexer has no notion of a register sigil, so `$t0` arrives as
//! `Punct::Dollar` followed by `Ident("t0")`, and `$8` as `Punct::Dollar`
//! followed by `Int(8)`.

use super::reg::{self, Reg};
use crate::arch::AsmCtx;
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

/// A `%hi` / `%lo` wrapper around an expression.
///
/// These are not general expression operators: they select which half of a
/// 32-bit address a 16-bit field gets, and which relocation carries it.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum RelocMod {
    #[default]
    None,
    /// `%hi(x)` — bits 31..16 of `x`, biased by 0x8000 so that adding the
    /// sign-extended `%lo(x)` reconstructs `x`.
    Hi,
    /// `%lo(x)` — bits 15..0 of `x`.
    Lo,
}

#[derive(Copy, Clone, Debug)]
pub struct Imm {
    pub expr: ExprRef,
    pub modifier: RelocMod,
    pub span: Span,
}

#[derive(Copy, Clone, Debug)]
pub struct Mem {
    pub base: Reg,
    /// Absent means a zero displacement, as in `lw $a0, ($sp)`.
    pub disp: Option<Imm>,
}

#[derive(Copy, Clone, Debug)]
pub enum OperandKind {
    Reg(Reg),
    Imm(Imm),
    Mem(Mem),
}

#[derive(Copy, Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    pub span: Span,
}

impl Operand {
    pub fn gpr(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) if r.is_gpr() => Some(r),
            _ => None,
        }
    }

    pub fn fpr(&self) -> Option<Reg> {
        match self.kind {
            OperandKind::Reg(r) if !r.is_gpr() => Some(r),
            _ => None,
        }
    }

    pub fn imm(&self) -> Option<Imm> {
        match self.kind {
            OperandKind::Imm(i) => Some(i),
            _ => None,
        }
    }

    pub fn mem(&self) -> Option<Mem> {
        match self.kind {
            OperandKind::Mem(m) => Some(m),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self.kind {
            OperandKind::Reg(r) => format!("register `{}`", reg::name_of(r)),
            OperandKind::Imm(_) => "an immediate".into(),
            OperandKind::Mem(_) => "a memory operand".into(),
        }
    }
}

pub struct OperandParser<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
}

impl OperandParser<'_, '_> {
    /// Parses one operand out of `toks`, which must be consumed entirely.
    pub fn parse_all(&mut self, toks: &[Token], whole: Span) -> Option<Operand> {
        if toks.is_empty() {
            self.cx.error(whole, "empty operand");
            return None;
        }
        let mut cur = Cursor::new(toks);
        let o = self.parse(&mut cur)?;
        if !cur.at_end() {
            self.cx
                .error(cur.peek().span, "unexpected token after operand");
            return None;
        }
        Some(o)
    }

    fn parse(&mut self, cur: &mut Cursor<'_>) -> Option<Operand> {
        let start = cur.peek().span;
        if cur.check_punct(Punct::Dollar) {
            let r = self.register(cur)?;
            return Some(Operand {
                kind: OperandKind::Reg(r),
                span: start.to(last_span(cur, start)),
            });
        }
        // `($sp)`: a base register with no displacement written at all.
        if cur.check_punct(Punct::LParen) {
            let base = self.base(cur)?;
            return Some(Operand {
                kind: OperandKind::Mem(Mem { base, disp: None }),
                span: start.to(last_span(cur, start)),
            });
        }
        let imm = self.immediate(cur)?;
        if cur.check_punct(Punct::LParen) {
            let base = self.base(cur)?;
            return Some(Operand {
                kind: OperandKind::Mem(Mem {
                    base,
                    disp: Some(imm),
                }),
                span: start.to(last_span(cur, start)),
            });
        }
        Some(Operand {
            kind: OperandKind::Imm(imm),
            span: imm.span,
        })
    }

    /// `$` followed by a number or an ABI name.
    fn register(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let dollar = cur.advance();
        let tok = cur.peek();
        let span = dollar.span.to(tok.span);
        match tok.kind {
            TokKind::Int(v) => {
                cur.advance();
                if v > 31 {
                    self.cx
                        .error(span, format!("`${v}` is not a register; MIPS has $0-$31"));
                    return None;
                }
                Some(Reg::gpr(v as u8))
            }
            TokKind::Ident(n) => {
                cur.advance();
                let name = self.cx.name(n).to_ascii_lowercase();
                match reg::lookup(&name) {
                    Some(r) => Some(r),
                    None => {
                        self.cx.error(span, format!("unknown register `${name}`"));
                        None
                    }
                }
            }
            _ => {
                self.cx.error(span, "expected a register name after `$`");
                None
            }
        }
    }

    /// `( $reg )`.
    fn base(&mut self, cur: &mut Cursor<'_>) -> Option<Reg> {
        let open = cur.advance();
        if !cur.check_punct(Punct::Dollar) {
            self.cx
                .error(cur.peek().span, "expected a base register after `(`");
            return None;
        }
        let r = self.register(cur)?;
        if !r.is_gpr() {
            self.cx.error(
                open.span,
                format!(
                    "`{}` cannot be a base register; addresses come from the integer file",
                    reg::name_of(r)
                ),
            );
            return None;
        }
        if cur.eat_punct(Punct::RParen).is_none() {
            self.cx
                .error(cur.peek().span, "expected `)` after the base register");
            return None;
        }
        Some(r)
    }

    /// An expression, optionally wrapped in `%hi(...)` or `%lo(...)`.
    fn immediate(&mut self, cur: &mut Cursor<'_>) -> Option<Imm> {
        let start = cur.peek().span;
        if cur.check_punct(Punct::Percent) {
            let pct = cur.advance();
            let tok = cur.peek();
            let name = match tok.kind {
                TokKind::Ident(n) => {
                    cur.advance();
                    self.cx.name(n).to_ascii_lowercase()
                }
                _ => {
                    self.cx.error(
                        pct.span.to(tok.span),
                        "expected a relocation name after `%`",
                    );
                    return None;
                }
            };
            let modifier = match name.as_str() {
                "hi" => RelocMod::Hi,
                "lo" => RelocMod::Lo,
                _ => {
                    self.cx.error(
                        pct.span.to(tok.span),
                        format!("unsupported relocation operator `%{name}`; expected %hi or %lo"),
                    );
                    return None;
                }
            };
            if !cur.check_punct(Punct::LParen) {
                self.cx
                    .error(cur.peek().span, format!("expected `(` after `%{name}`"));
                return None;
            }
            // The argument is parsed as an ordinary parenthesised expression,
            // so `%hi(a + 4)` works and the closing paren is consumed there.
            let expr = self.expr(cur)?;
            return Some(Imm {
                expr,
                modifier,
                span: start.to(last_span(cur, start)),
            });
        }
        let expr = self.expr(cur)?;
        let span = start.to(self.cx.exprs.span(expr));
        Some(Imm {
            expr,
            modifier: RelocMod::None,
            span,
        })
    }

    fn expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let mut p = self.cx.expr_parser();
        p.parse(cur)
    }
}

/// Span of the token the cursor just passed, for closing an operand's span.
fn last_span(cur: &Cursor<'_>, fallback: Span) -> Span {
    match cur.pos().checked_sub(1).and_then(|i| cur.all().get(i)) {
        Some(t) => t.span,
        None => fallback,
    }
}

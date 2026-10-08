//! Turning parsed operands into instruction words.

use super::compress;
use super::encode::{self, Buf, Fix, Insn};
use super::insn;
use super::insn::{Def, Kind, RM, RV64};
use super::operand::{Imm, Mem, Modifier, Operands};
use super::pseudo::AUIPC;
use super::reg::{self, Reg};
use crate::arch::AsmCtx;
use crate::section::{FixupKind, Variant};
use crate::source::Span;

pub struct Asm<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
    pub xlen: u8,
    /// Whether the C extension may shorten what is emitted.
    pub rvc: bool,
    pub span: Span,
    out: Buf,
    /// A shorter candidate for a single relaxable branch or jump. Layout picks
    /// this one unless the displacement turns out not to fit.
    alt: Option<Buf>,
}

impl<'c, 'a> Asm<'c, 'a> {
    pub fn new(cx: &'c mut AsmCtx<'a>, xlen: u8, rvc: bool, span: Span) -> Asm<'c, 'a> {
        Asm {
            cx,
            xlen,
            rvc,
            span,
            out: Buf::default(),
            alt: None,
        }
    }

    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.cx.error(span, msg);
    }

    pub fn rv64(&self) -> bool {
        self.xlen == 64
    }

    /// Emits a 32-bit word, shortening it where the C extension allows.
    pub fn emit(&mut self, word: u32) {
        let insn = match self.rvc.then(|| compress::compress(word, self.xlen)) {
            Some(Some(short)) => Insn::short(short),
            _ => Insn::full(word),
        };
        self.out.push(insn);
    }

    /// Emits a word that must keep its full width, because a fixup will patch
    /// a field the compressed form does not have.
    pub fn emit_fixed(&mut self, insn: Insn) {
        self.out.push(insn);
    }

    pub fn emit_pair(&mut self, first: u32, second: u32, fix: Fix) {
        self.out.push_pair(first, second, fix);
    }

    /// Records the two-byte candidate for a branch that may or may not reach.
    fn set_alt(&mut self, insn: Insn) {
        debug_assert!(self.out.is_empty(), "only a lone branch may relax");
        let mut buf = Buf::default();
        buf.push(insn);
        self.alt = Some(buf);
    }

    pub fn finish(self) -> Vec<Variant> {
        match self.alt {
            Some(short) => vec![short.finish(), self.out.finish()],
            None => vec![self.out.finish()],
        }
    }

    // ---- immediates -------------------------------------------------------

    /// An immediate that has to be known now, with its range checked.
    pub fn constant(&mut self, imm: &Imm, lo: i64, hi: i64, what: &str) -> Option<i64> {
        if imm.modifier.is_some() {
            self.error(imm.span, format!("{what} cannot use a relocation modifier"));
            return None;
        }
        let Some(v) = self.cx.constant(imm.expr) else {
            self.error(imm.span, format!("{what} must be known at assembly time"));
            return None;
        };
        if v < lo || v > hi {
            self.error(
                imm.span,
                format!("{what} must be between {lo} and {hi}, but is {v}"),
            );
            return None;
        }
        Some(v)
    }

    /// Places a 12-bit immediate into an I- or S-type word.
    fn imm12(&mut self, word: u32, imm: &Imm, store: bool) -> Option<Insn> {
        let kind = match imm.modifier {
            Some(Modifier::Lo) => encode::kind_abs_lo12(store),
            Some(Modifier::PcrelLo) => encode::kind_lo12(store),
            Some(m) => {
                self.error(imm.span, format!("{} does not fit a 12-bit field", name(m)));
                return None;
            }
            None => match self.cx.constant(imm.expr) {
                Some(v) => {
                    if !(-2048..=2047).contains(&v) {
                        self.error(
                            imm.span,
                            format!("immediate {v} does not fit a signed 12-bit field"),
                        );
                        return None;
                    }
                    let full = if store {
                        encode::s_imm(word as u64, v)
                    } else {
                        encode::i_imm(word as u64, v)
                    };
                    return Some(Insn::full(full as u32));
                }
                None if store => encode::kind_s(),
                None => encode::kind_i(),
            },
        };
        Some(Insn::full(word).with_fix(imm.expr, kind, imm.span))
    }

    /// Places a 20-bit immediate into a `lui` or `auipc`.
    fn imm20(&mut self, word: u32, imm: &Imm) -> Option<Insn> {
        let kind = match imm.modifier {
            Some(Modifier::Hi) => encode::kind_hi20(false),
            Some(Modifier::PcrelHi) => encode::kind_hi20(true),
            Some(m) => {
                self.error(
                    imm.span,
                    format!("{} belongs in the low half of an address", name(m)),
                );
                return None;
            }
            None => match self.cx.constant(imm.expr) {
                Some(v) => {
                    if !(-(1 << 19)..(1 << 20)).contains(&v) {
                        self.error(
                            imm.span,
                            format!("immediate {v} does not fit the 20-bit field of `lui`"),
                        );
                        return None;
                    }
                    return Some(Insn::full(encode::u_imm(word as u64, v) as u32));
                }
                None => encode::kind_u(),
            },
        };
        Some(Insn::full(word).with_fix(imm.expr, kind, imm.span))
    }

    pub fn no_modifier(&mut self, imm: &Imm, what: &str) -> Option<()> {
        if imm.modifier.is_some() {
            self.error(imm.span, format!("{what} cannot use a relocation modifier"));
            return None;
        }
        Some(())
    }

    // ---- branches and jumps ----------------------------------------------

    /// A conditional branch, with its compressed candidate when it has one.
    pub fn branch(&mut self, base: u32, rs1: Reg, rs2: Reg, target: &Imm) -> Option<()> {
        self.no_modifier(target, "a branch target")?;
        let word = encode::rs2(encode::rs1(base, rs1.bits()), rs2.bits());
        // `c.beqz` and `c.bnez` compare against zero only and reach +-256
        // bytes rather than +-4 KiB, so both forms go to layout, which keeps
        // the short one only if the target turns out to be close enough.
        let funct3 = (base >> 12) & 7;
        if self.rvc && funct3 <= 1 {
            let (test, other) = if rs2 == reg::ZERO {
                (rs1, rs2)
            } else {
                (rs2, rs1)
            };
            if other == reg::ZERO && test.is_popular() {
                let short = compress::c_branch(funct3 == 0, test);
                self.set_alt(Insn::short(short).with_fix(
                    target.expr,
                    encode::kind_cb(),
                    target.span,
                ));
            }
        }
        self.emit_fixed(Insn::full(word).with_fix(target.expr, encode::kind_branch(), target.span));
        Some(())
    }

    /// `jal rd, target`, with `c.j` or `c.jal` where they apply.
    pub fn jal(&mut self, rd: Reg, target: &Imm) -> Option<()> {
        self.no_modifier(target, "a jump target")?;
        let word = encode::rd(0x0000_006f, rd.bits());
        if self.rvc {
            // RV64 reuses the `c.jal` encoding for `c.addiw`, so only RV32 can
            // shorten a call that links through `ra`.
            let link = rd == reg::RA && self.xlen == 32;
            if rd == reg::ZERO || link {
                let short = compress::c_jump(link);
                self.set_alt(Insn::short(short).with_fix(
                    target.expr,
                    encode::kind_cj(),
                    target.span,
                ));
            }
        }
        self.emit_fixed(Insn::full(word).with_fix(target.expr, encode::kind_jal(), target.span));
        Some(())
    }

    /// The `auipc`/`jalr` pair behind `call`, `tail` and `jump`.
    pub fn call_pair(&mut self, first: u32, second: u32, target: &Imm) {
        self.emit_pair(
            first,
            second,
            Fix {
                expr: target.expr,
                kind: encode::kind_call(),
                span: target.span,
            },
        );
    }

    /// An `auipc` and the instruction that takes the low half of the same
    /// address from it, as `la`, `lga` and `lw a0, sym` expand to. Unlike
    /// `call`'s pair, each half has a relocation of its own.
    pub fn auipc_split(
        &mut self,
        auipc: u32,
        second: u32,
        target: &Imm,
        hi: FixupKind,
        lo: FixupKind,
    ) {
        // The low half's relocation names a label at the `auipc`, which the
        // core puts at the start of the fragment.
        debug_assert!(
            self.out.is_empty(),
            "an `auipc` pair must start its fragment"
        );
        self.emit_fixed(Insn::full(auipc).with_fix(target.expr, hi, target.span));
        self.emit_fixed(Insn::full(second).with_fix(target.expr, lo, target.span));
    }

    /// `lw a0, sym` and `sw a0, sym, t0`: an access to a symbol's address
    /// through an `auipc` into `base`, which a load can share with its
    /// destination but a store or a floating-point load cannot.
    ///
    /// llvm-mc takes this form only for a symbol, refusing a plain number
    /// where the `(reg)` was left off, and so does this.
    fn symbol_access(&mut self, word: u32, base: Reg, target: &Imm, store: bool) -> Option<()> {
        self.no_modifier(target, "a symbol address")?;
        if self.cx.constant(target.expr).is_some() {
            self.error(target.span, "expected an address of the form `offset(reg)`");
            return None;
        }
        let auipc = encode::rd(AUIPC, base.bits());
        let second = encode::rs1(word, base.bits());
        self.auipc_split(
            auipc,
            second,
            target,
            encode::kind_hi20(true),
            encode::kind_pair_lo12(store),
        );
        Some(())
    }

    /// `op rd, rs1, rs2`, for pseudo-instructions that expand to one.
    pub fn r_type(&mut self, base: u32, rd: Reg, rs1: Reg, rs2: Reg) {
        let w = encode::rs2(
            encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
            rs2.bits(),
        );
        self.emit(w);
    }

    /// `op rd, rs1, imm` with the immediate already known.
    pub fn i_const(&mut self, base: u32, rd: Reg, rs1: Reg, imm: i64) {
        let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
        self.emit(encode::i_imm(w as u64, imm) as u32);
    }

    /// `op rd, rs1, imm` where the immediate came from the source.
    pub fn i_expr(&mut self, base: u32, rd: Reg, rs1: Reg, imm: &Imm) -> Option<()> {
        let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
        let insn = self.imm12(w, imm, false)?;
        self.emit_or_compress(insn);
        Some(())
    }

    // ---- the table --------------------------------------------------------

    /// `extra` carries bits the mnemonic itself set, such as the `.aq` and
    /// `.rl` ordering flags of an atomic.
    pub fn encode_def(
        &mut self,
        def: &Def,
        name: &str,
        ops: &Operands<'_>,
        extra: u32,
    ) -> Option<()> {
        if def.flags & RV64 != 0 && !self.rv64() {
            self.error(
                self.span,
                format!("`{name}` is an RV64 instruction, but the target is RV32"),
            );
            return None;
        }
        // A rounding mode is an optional extra operand, so it is taken off the
        // end before the fixed operands are counted.
        let (rm, count) = self.rounding_mode(def, ops);
        let base = match rm {
            Some(m) => encode::funct3(def.base, m),
            None => def.base,
        } | extra;
        self.encode_form(def, base, name, ops, count)
    }

    /// Splits a trailing rounding mode off the operand list.
    fn rounding_mode(&mut self, def: &Def, ops: &Operands<'_>) -> (Option<u32>, usize) {
        let n = ops.len();
        if def.flags & RM == 0 || n == 0 {
            return (None, n);
        }
        match ops
            .word(self.cx, n - 1)
            .as_deref()
            .and_then(insn::rounding_mode)
        {
            Some(m) => (Some(m), n - 1),
            None => (None, n),
        }
    }

    fn encode_form(
        &mut self,
        def: &Def,
        base: u32,
        name: &str,
        ops: &Operands<'_>,
        count: usize,
    ) -> Option<()> {
        let want: &[usize] = match def.kind {
            Kind::Nullary => &[0],
            Kind::Fence => &[0, 2],
            Kind::U | Kind::F2 | Kind::FToX | Kind::XToF | Kind::Load | Kind::AmoLoad => &[2],
            // The third operand is the scratch register of the symbol form.
            Kind::Store | Kind::FLoad | Kind::FStore => &[2, 3],
            Kind::Jal => &[1, 2],
            Kind::Jalr => &[1, 2, 3],
            Kind::F4 => &[4],
            _ => &[3],
        };
        if !want.contains(&count) {
            let list: Vec<String> = want.iter().map(|n| n.to_string()).collect();
            self.error(
                ops.span,
                format!(
                    "`{name}` takes {} operand(s), but {count} were given",
                    list.join(" or ")
                ),
            );
            return None;
        }

        match def.kind {
            Kind::R => {
                let (rd, rs1, rs2) = (
                    ops.xreg(self.cx, 0)?,
                    ops.xreg(self.cx, 1)?,
                    ops.xreg(self.cx, 2)?,
                );
                let w = encode::rs2(
                    encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                    rs2.bits(),
                );
                self.emit(w);
            }
            Kind::I => {
                let (rd, rs1) = (ops.xreg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                let imm = ops.imm(self.cx, 2)?;
                let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
                let insn = self.imm12(w, &imm, false)?;
                self.emit_or_compress(insn);
            }
            Kind::Shift | Kind::ShiftW => {
                let (rd, rs1) = (ops.xreg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                let imm = ops.imm(self.cx, 2)?;
                let max = if def.kind == Kind::ShiftW {
                    31
                } else {
                    self.xlen as i64 - 1
                };
                let n = self.constant(&imm, 0, max, "a shift amount")?;
                let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits()) | ((n as u32) << 20);
                self.emit(w);
            }
            Kind::Load | Kind::FLoad => {
                let rd = if def.kind == Kind::Load {
                    ops.xreg(self.cx, 0)?
                } else {
                    ops.freg(self.cx, 0)?
                };
                // `lw a0, sym` loads through its own destination; `flw` needs
                // an integer register to hold the address instead.
                let symbol_form = if def.kind == Kind::Load {
                    !ops.ends_in_group(1)
                } else {
                    count == 3
                };
                if symbol_form {
                    let via = if def.kind == Kind::Load {
                        rd
                    } else {
                        ops.xreg(self.cx, 2)?
                    };
                    let target = ops.imm(self.cx, 1)?;
                    let w = encode::rd(base, rd.bits());
                    return self.symbol_access(w, via, &target, false);
                }
                let mem = ops.mem(self.cx, 1)?;
                let w = encode::rs1(encode::rd(base, rd.bits()), mem.base.bits());
                let insn = self.mem_offset(w, &mem, false)?;
                self.emit_or_compress(insn);
            }
            Kind::Store | Kind::FStore => {
                let rs2 = if def.kind == Kind::Store {
                    ops.xreg(self.cx, 0)?
                } else {
                    ops.freg(self.cx, 0)?
                };
                if count == 3 {
                    let target = ops.imm(self.cx, 1)?;
                    let tmp = ops.xreg(self.cx, 2)?;
                    let w = encode::rs2(base, rs2.bits());
                    return self.symbol_access(w, tmp, &target, true);
                }
                let mem = ops.mem(self.cx, 1)?;
                let w = encode::rs1(encode::rs2(base, rs2.bits()), mem.base.bits());
                let insn = self.mem_offset(w, &mem, true)?;
                self.emit_or_compress(insn);
            }
            Kind::Branch => {
                let (rs1, rs2) = (ops.xreg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                let target = ops.imm(self.cx, 2)?;
                self.branch(base, rs1, rs2, &target)?;
            }
            Kind::U => {
                let rd = ops.xreg(self.cx, 0)?;
                let imm = ops.imm(self.cx, 1)?;
                let insn = self.imm20(encode::rd(base, rd.bits()), &imm)?;
                self.emit_or_compress(insn);
            }
            Kind::Jal => {
                let (rd, target) = if count == 1 {
                    (reg::RA, ops.imm(self.cx, 0)?)
                } else {
                    (ops.xreg(self.cx, 0)?, ops.imm(self.cx, 1)?)
                };
                self.jal(rd, &target)?;
            }
            Kind::Jalr => self.jalr(base, ops, count)?,
            Kind::Amo | Kind::AmoLoad => {
                let rd = ops.xreg(self.cx, 0)?;
                let (rs2, mem) = if def.kind == Kind::Amo {
                    (ops.xreg(self.cx, 1)?, ops.mem(self.cx, 2)?)
                } else {
                    (reg::ZERO, ops.mem(self.cx, 1)?)
                };
                // The A extension has no displacement field at all.
                if let Some(off) = &mem.off
                    && self.cx.constant(off.expr) != Some(0)
                {
                    self.error(
                        off.span,
                        "an atomic instruction addresses `(reg)` with no offset",
                    );
                    return None;
                }
                let w = encode::rs1(
                    encode::rs2(encode::rd(base, rd.bits()), rs2.bits()),
                    mem.base.bits(),
                );
                self.emit(w);
            }
            Kind::F3 => {
                let (rd, rs1, rs2) = (
                    ops.freg(self.cx, 0)?,
                    ops.freg(self.cx, 1)?,
                    ops.freg(self.cx, 2)?,
                );
                let w = encode::rs2(
                    encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                    rs2.bits(),
                );
                self.emit(w);
            }
            Kind::F2 => {
                let (rd, rs1) = (ops.freg(self.cx, 0)?, ops.freg(self.cx, 1)?);
                self.emit(encode::rs1(encode::rd(base, rd.bits()), rs1.bits()));
            }
            Kind::F4 => {
                let (rd, rs1, rs2, rs3) = (
                    ops.freg(self.cx, 0)?,
                    ops.freg(self.cx, 1)?,
                    ops.freg(self.cx, 2)?,
                    ops.freg(self.cx, 3)?,
                );
                let w = encode::rs3(
                    encode::rs2(
                        encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                        rs2.bits(),
                    ),
                    rs3.bits(),
                );
                self.emit(w);
            }
            Kind::FCmp => {
                let (rd, rs1, rs2) = (
                    ops.xreg(self.cx, 0)?,
                    ops.freg(self.cx, 1)?,
                    ops.freg(self.cx, 2)?,
                );
                let w = encode::rs2(
                    encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                    rs2.bits(),
                );
                self.emit(w);
            }
            Kind::FToX => {
                let (rd, rs1) = (ops.xreg(self.cx, 0)?, ops.freg(self.cx, 1)?);
                self.emit(encode::rs1(encode::rd(base, rd.bits()), rs1.bits()));
            }
            Kind::XToF => {
                let (rd, rs1) = (ops.freg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                self.emit(encode::rs1(encode::rd(base, rd.bits()), rs1.bits()));
            }
            Kind::Csr | Kind::CsrI => {
                let rd = ops.xreg(self.cx, 0)?;
                let csr = self.csr(ops, 1)?;
                let w = encode::rd(base, rd.bits()) | (csr << 20);
                let w = if def.kind == Kind::Csr {
                    encode::rs1(w, ops.xreg(self.cx, 2)?.bits())
                } else {
                    let imm = ops.imm(self.cx, 2)?;
                    let v = self.constant(&imm, 0, 31, "a CSR immediate")?;
                    encode::rs1(w, v as u32)
                };
                self.emit(w);
            }
            Kind::Fence => {
                let (pred, succ) = if count == 2 {
                    (self.fence_set(ops, 0)?, self.fence_set(ops, 1)?)
                } else {
                    (0xf, 0xf)
                };
                self.emit(base | (pred << 24) | (succ << 20));
            }
            Kind::Nullary => self.emit(base),
        }
        Some(())
    }

    /// `jalr rs1`, `jalr rd, rs1`, `jalr rd, off(rs1)` and `jalr rd, rs1, off`
    /// are all spellings of the same instruction.
    fn jalr(&mut self, base: u32, ops: &Operands<'_>, count: usize) -> Option<()> {
        let (rd, mem) = match count {
            1 => (reg::RA, self.address_or_reg(ops, 0)?),
            2 => (ops.xreg(self.cx, 0)?, self.address_or_reg(ops, 1)?),
            _ => {
                let rd = ops.xreg(self.cx, 0)?;
                let rs1 = ops.xreg(self.cx, 1)?;
                let off = ops.imm(self.cx, 2)?;
                (
                    rd,
                    Mem {
                        base: rs1,
                        off: Some(off),
                        span: ops.piece_span(2),
                    },
                )
            }
        };
        let w = encode::rs1(encode::rd(base, rd.bits()), mem.base.bits());
        let insn = self.mem_offset(w, &mem, false)?;
        self.emit_or_compress(insn);
        Some(())
    }

    fn address_or_reg(&mut self, ops: &Operands<'_>, i: usize) -> Option<Mem> {
        if ops.looks_like_mem(self.cx, i) {
            return ops.mem(self.cx, i);
        }
        let base = ops.xreg(self.cx, i)?;
        Some(Mem {
            base,
            off: None,
            span: ops.piece_span(i),
        })
    }

    fn mem_offset(&mut self, word: u32, mem: &Mem, store: bool) -> Option<Insn> {
        match &mem.off {
            Some(imm) => self.imm12(word, imm, store),
            None => Some(Insn::full(word)),
        }
    }

    /// Emits an instruction that may still be shortened, which is only true
    /// when its immediate turned out to be a constant.
    fn emit_or_compress(&mut self, insn: Insn) {
        if insn.fix.is_none() {
            self.emit(insn.word);
        } else {
            self.emit_fixed(insn);
        }
    }

    pub fn csr(&mut self, ops: &Operands<'_>, i: usize) -> Option<u32> {
        if let Some(word) = ops.word(self.cx, i)
            && let Some(n) = insn::csr(&word)
        {
            return Some(n);
        }
        let imm = ops.imm(self.cx, i)?;
        let v = self.constant(&imm, 0, 4095, "a CSR number")?;
        Some(v as u32)
    }

    fn fence_set(&mut self, ops: &Operands<'_>, i: usize) -> Option<u32> {
        let span = ops.piece_span(i);
        match ops.word(self.cx, i).as_deref().and_then(insn::fence_set) {
            Some(bits) => Some(bits),
            None => {
                self.error(span, "expected some combination of the letters `iorw`");
                None
            }
        }
    }
}

fn name(m: Modifier) -> &'static str {
    match m {
        Modifier::Hi => "`%hi`",
        Modifier::Lo => "`%lo`",
        Modifier::PcrelHi => "`%pcrel_hi`",
        Modifier::PcrelLo => "`%pcrel_lo`",
    }
}

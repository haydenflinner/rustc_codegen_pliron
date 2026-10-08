//! SPARC V8 and V9. `EM_SPARC` / `EM_SPARCV9`.
//!
//! One backend covers both: V9 is a strict superset of V8's user-mode
//! instruction set, so the difference is the pointer width, the ELF machine
//! number, and a set of mnemonics that a 32-bit target refuses. `--arch
//! sparc` is V8 and `--arch sparcv9` is V9.
//!
//! The interesting parts are split up: [`reg`] for the register file,
//! [`operand`] for the operand grammar (including SPARC's prefix `%hi()` /
//! `%lo()`, which is not the generic `@` modifier), [`insn`] for the opcode
//! table, [`encode`] for the three instruction formats, and [`synth`] for the
//! synthetic instructions that most SPARC assembly is actually written in.

pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;
pub mod synth;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi};
use crate::lexer::{Punct, TokKind};
use crate::section::Variant;
use encode::BranchSuffix;
use insn::Form;
use operand::OperandParser;

pub const NAMES: &[&str] = &["sparc", "sparcv9"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let v9 = match name {
        "sparc" | "sparc32" | "sparcv8" | "v8" => false,
        "sparcv9" | "sparc64" | "v9" => true,
        _ => return None,
    };
    Some(Box::new(Sparc { v9 }))
}

pub struct Sparc {
    v9: bool,
}

impl Architecture for Sparc {
    fn name(&self) -> &'static str {
        if self.v9 { "sparcv9" } else { "sparc" }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["sparc32", "sparc64", "sparcv8", "v8", "v9"]
    }

    /// SPARC is big-endian in every ABI this backend targets.
    fn endian(&self) -> Endian {
        Endian::Big
    }

    fn pointer_bytes(&self, state: &ArchState) -> u8 {
        state.bits / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: if self.v9 { 64 } else { 32 },
            syntax: Syntax::Att,
            features: 0,
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    /// There is no Intel-flavoured SPARC syntax to support.
    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        if self.v9 {
            43 // EM_SPARCV9
        } else {
            2 // EM_SPARC
        }
    }

    /// SPARC comments with `!`; `#` is a comment only in the first column.
    fn comments(&self) -> crate::arch::CommentSyntax {
        crate::arch::CommentSyntax {
            anywhere: &["!", "//"],
            line_start: &["#"],
        }
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    /// llvm-mc writes the unaligned variant of an absolute data relocation
    /// for a field that is not on its width's boundary, as a DWARF section's
    /// fields often are not.
    fn reloc_at(&self, reloc: u32, offset: u64) -> u32 {
        match reloc {
            reloc::ABS32 if !offset.is_multiple_of(4) => reloc::UA32,
            reloc::ABS64 if !offset.is_multiple_of(8) => reloc::UA64,
            r => r,
        }
    }

    /// llvm-mc, the reference, aligns `.text` to 4 bytes; GNU as leaves it
    /// at 1.
    fn section_align(
        &self,
        _state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        if name == ".text" { 4 } else { 1 }
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        if pcrel {
            reloc::pcrel(size)
        } else {
            reloc::abs(size)
        }
    }

    /// llvm-mc's conventions, as for every SPARC encoding. A V9 frame starts
    /// with the CFA 2047 bytes above `%sp`, the stack bias.
    fn dwarf(&self, _state: &ArchState) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: if self.v9 { -8 } else { -4 },
                ra_column: 15,
                initial: vec![cfi::Insn::DefCfa(14, if self.v9 { 2047 } else { 0 })],
                fde_encoding: 0x1b,
                eh_frame_align: if self.v9 { 8 } else { 4 },
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// DWARF numbers the integer registers as the encoding does, and the
    /// floating-point ones from 32.
    fn dwarf_register(&self, _state: &ArchState, name: &str) -> Option<u32> {
        let r = reg::lookup(name.strip_prefix('%')?)?;
        match r.class {
            reg::RegClass::Int => Some(r.num as u32),
            reg::RegClass::Float => Some(32 + r.num as u32),
            reg::RegClass::Asr if r.num == 0 => Some(64),
            _ => None,
        }
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        encode::nop_bytes(len as usize)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let m = cx.name(req.mnemonic).to_ascii_lowercase();
        let mut cur = req.cursor();

        if let Some(def) = insn::lookup(&m) {
            if def.v9 && cx.state.bits < 64 {
                cx.error(
                    req.mnemonic_span,
                    format!("`{m}` is a SPARC V9 instruction; this target is V8"),
                );
                return None;
            }
            // A branch's `,a` / `,pn` / `,pt` suffixes are separate tokens,
            // so they have to come off before the operands are split on
            // commas.
            let sfx = match def.form {
                Form::Branch { .. } | Form::BranchReg(_) => branch_suffix(cx, &mut cur)?,
                _ => BranchSuffix::default(),
            };
            let ops = OperandParser { cx }.parse_list(&cur)?;
            return match def.form {
                Form::Branch { cond, predicted } => {
                    encode::branch(cx, &m, req.span, cond, predicted, sfx, &ops)
                }
                Form::BranchReg(rcond) => encode::branch_reg(cx, &m, req.span, rcond, sfx, &ops),
                form => encode::encode(cx, &m, req.span, form, &ops),
            };
        }

        if synth::is_synthetic(&m) {
            let ops = OperandParser { cx }.parse_list(&cur)?;
            return synth::assemble(cx, &m, req.span, &ops);
        }

        cx.error(req.mnemonic_span, format!("unknown instruction `{m}`"));
        None
    }
}

/// Consumes `,a`, `,pn` and `,pt` from the front of a branch's operands.
fn branch_suffix(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) -> Option<BranchSuffix> {
    let mut sfx = BranchSuffix::default();
    while cur.check_punct(Punct::Comma) {
        let TokKind::Ident(n) = cur.nth(1).kind else {
            break;
        };
        let word = cx.name(n).to_ascii_lowercase();
        match word.as_str() {
            "a" => sfx.annul = true,
            "pn" => sfx.predict = Some(false),
            "pt" => sfx.predict = Some(true),
            _ => break,
        }
        cur.advance();
        cur.advance();
    }
    Some(sfx)
}

//! MIPS, 32- and 64-bit, big and little endian. `EM_MIPS`.
//!
//! Four targets share one backend, differing only in byte order and register
//! width: `mips` and `mipsel` are 32-bit, `mips64` and `mips64el` 64-bit. The
//! instruction words are identical; only how they are laid down in memory and
//! which doubleword instructions are legal change.
//!
//! **Delay slots are the programmer's problem.** GNU as defaults to
//! `.set reorder`, in which the assembler may move an instruction into the
//! slot after a branch or insert a `nop` there. This backend behaves as
//! `.set noreorder` always: it emits exactly the instructions written, in the
//! order written, and never invents a `nop`. Multi-instruction *macros* such
//! as `li` and `la` are still expanded, since almost no real source avoids
//! them.

pub mod encode;
pub mod insn;
pub mod operand;
pub mod pseudo;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::section::Variant;
use encode::Args;
use operand::{Operand, OperandParser};

pub const NAMES: &[&str] = &["mips", "mipsel", "mips64", "mips64el"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let (canonical, endian, bits) = match name {
        "mips" | "mips32" => ("mips", Endian::Big, 32),
        "mipsel" | "mips32el" | "mipsle" => ("mipsel", Endian::Little, 32),
        "mips64" => ("mips64", Endian::Big, 64),
        "mips64el" | "mips64le" => ("mips64el", Endian::Little, 64),
        _ => return None,
    };
    Some(Box::new(Mips {
        name: canonical,
        endian,
        bits,
    }))
}

pub struct Mips {
    name: &'static str,
    endian: Endian,
    bits: u8,
}

impl Architecture for Mips {
    fn name(&self) -> &'static str {
        self.name
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["mips32", "mips32el", "mipsle", "mips64le"]
    }

    fn endian(&self) -> Endian {
        self.endian
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        self.bits / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: self.bits,
            syntax: Syntax::Att,
            features: 0,
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // MIPS assembly has only ever had one operand order.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        8 // EM_MIPS
    }

    /// What llvm-mc writes for the default CPUs: MIPS32 with the o32 ABI and
    /// `EF_MIPS_CPIC`, or MIPS64 (n64 has no ABI bits) with `EF_MIPS_CPIC`.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        if self.bits == 64 {
            0x6000_0004
        } else {
            0x5000_1004
        }
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// llvm-mc, the reference, aligns `.text`, `.data` and `.bss` to 16
    /// bytes, as GNU as does for MIPS outside ELF; for ELF, GNU as aligns
    /// only `.text`, and to 4.
    fn section_align(
        &self,
        _state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        match name {
            ".text" | ".data" | ".bss" => 16,
            _ => 1,
        }
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        if pcrel {
            reloc::pcrel(size)
        } else {
            reloc::abs(size)
        }
    }

    /// llvm-mc's conventions, as for every MIPS encoding. Its FDE addresses
    /// are absolute, a pointer's width, where other targets measure them from
    /// the field.
    fn dwarf(&self, _state: &ArchState) -> DwarfTarget {
        let wide = self.bits == 64;
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: if wide { -8 } else { -4 },
                ra_column: 31,
                initial: vec![cfi::Insn::DefCfaRegister(29)],
                // DW_EH_PE_sdata8 or DW_EH_PE_sdata4.
                fde_encoding: if wide { 0x0c } else { 0x0b },
                eh_frame_align: if wide { 8 } else { 4 },
                cie_version: 1,
            }),
            private_prefix: if wide { ".L" } else { "$" },
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// DWARF numbers MIPS registers by their number, so this is the `$`
    /// syntax's own lookup: `$31`, the O32 names, and for a 64-bit target
    /// the N64 names llvm-mc also takes (`$a4`-`$a7`, and `$t4`-`$t7` for
    /// 12-15 as well as `$t0`-`$t3`).
    fn dwarf_register(&self, _state: &ArchState, name: &str) -> Option<u32> {
        let name = name.strip_prefix('$')?;
        if let Some(n) = numbered_register(name, "", 31) {
            return Some(n);
        }
        if self.bits == 64 {
            if let Some(n) = numbered_register(name, "a", 7).filter(|n| *n >= 4) {
                return Some(4 + n);
            }
            if let Some(n) = numbered_register(name, "t", 3) {
                return Some(12 + n);
            }
        }
        match name {
            "kt0" => Some(26),
            "kt1" => Some(27),
            _ => match reg::lookup(name) {
                Some(r) if r.is_gpr() => Some(r.num as u32),
                _ => None,
            },
        }
    }

    /// MIPS `nop` is `sll $zero, $zero, 0`, whose encoding is the all-zero
    /// word. Zero fill therefore *is* nop fill here, which is why this looks
    /// like the unimplemented default and is not.
    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        vec![0; len as usize]
    }

    /// `.set <option>`.
    ///
    /// rsasm assembles exactly what is written: it never moves an instruction
    /// into a delay slot and never inserts a `nop` after a branch. That is
    /// `.set noreorder`, so that option is accepted and changes nothing.
    ///
    /// `.set reorder` is refused rather than ignored, because the difference
    /// is not cosmetic. Under `reorder` the assembler fills the delay slot,
    /// so in `beq a, b, x` / `addiu t, t, 1` the `addiu` runs only when the
    /// branch falls through. Under `noreorder` that same `addiu` *is* the
    /// delay slot and runs on both paths. Accepting `reorder` while behaving
    /// as `noreorder` would assemble a different program without a word.
    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        if name != ".set" {
            return false;
        }
        let tok = cur.peek();
        let Some(n) = tok.ident() else {
            return false;
        };
        let opt = cx.name(n).to_ascii_lowercase();
        match opt.as_str() {
            "reorder" => {
                cur.advance();
                cx.error(
                    tok.span,
                    "`.set reorder` is not supported: rsasm never fills delay slots, so \
                     code written for it would run the instruction after each branch on \
                     both paths; fill the slots by hand and use `.set noreorder`",
                );
                true
            }
            // Options whose effect rsasm already has, or that only make an
            // assembler stricter about what it accepts and never change an
            // encoding, so accepting them silently is safe.
            "noreorder" | "noat" | "at" | "nomacro" | "macro" | "push" | "pop" | "nomips16"
            | "nomicromips" | "mips1" | "mips2" | "mips3" | "mips4" | "mips5" | "mips32"
            | "mips32r2" | "mips32r6" | "mips64" | "mips64r2" | "mips64r6" | "hardfloat"
            | "softfloat" | "nodsp" => {
                cur.advance();
                true
            }
            _ => false,
        }
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();

        let cur = req.cursor();
        let pieces = cur.split_commas();
        let mut ops: Vec<Operand> = Vec::with_capacity(pieces.len());
        for piece in &pieces {
            let mut p = OperandParser { cx };
            ops.push(p.parse_all(piece, req.span)?);
        }

        let args = Args {
            mnemonic: &mnemonic,
            ops: &ops,
            span: req.span,
        };

        // Macros are tried first: `b` and `move` are not real opcodes, so
        // there is nothing in the table for them to shadow.
        if pseudo::is_pseudo(&mnemonic) {
            return pseudo::expand(cx, &mnemonic, &args, self.endian, self.bits == 64)
                .map(|v| vec![v]);
        }

        let Some(def) = insn::lookup(&mnemonic) else {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        };
        // Every MIPS instruction is one word wide, so there is never more than
        // one candidate for the layout pass to choose between.
        encode::encode(cx, &def, &args, self.endian, self.bits == 64).map(|v| vec![v])
    }
}

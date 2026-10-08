//! ARM 64-bit (A64). `EM_AARCH64`.
//!
//! A64 is fixed-width: every instruction is exactly four bytes, so this
//! backend always returns a single [`Variant`] and never asks the layout pass
//! to choose between encodings. What relaxation buys elsewhere, a fixup's
//! `value_bits` buys here — an out-of-range branch is a diagnostic rather than
//! a longer encoding, because there is no longer encoding.
//!
//! The other consequence of fixed width is that displacements never fit in one
//! contiguous field. Every PC-relative fixup therefore uses
//! [`crate::section::FieldEncoding::Scatter`] to weave its value through the
//! instruction word; the scatter functions live in [`encode`].
//!
//! # Two encoders
//!
//! The general-purpose instruction set is written out family by family in
//! [`insn`], where the interesting work is in the aliases. SIMD, floating
//! point and SVE are thousands of forms that differ in a few opcode bits, and
//! come from a table measured against llvm-mc; see the `table` module and
//! `tools/tables/README.md`. A line goes to the table if only the table has
//! its mnemonic, or if an operand is a register only a table form takes.
//!
//! # The `#` sigil
//!
//! A64 source conventionally writes immediates as `#imm`. `#` is a comment
//! only in the first column (see `comments` below), so both `add x0, x1,
//! #1` and the bare `add x0, x1, 1` that GNU as and llvm-mc also accept work.

pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;
pub mod sysreg;
// Not public API: the generated tables and the table's matcher are
// internals, and `sysreg_data`, `table_data` and `table_names` are generated
// files.
mod sysreg_data;
pub(crate) mod table;
mod table_data;
mod table_names;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, InsnRequest, Syntax};
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::section::Variant;

pub const NAMES: &[&str] = &["aarch64"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    match name {
        "aarch64" | "arm64" | "armv8" | "armv8-a" | "aarch64le" => Some(Box::new(AArch64)),
        _ => None,
    }
}

pub struct AArch64;

/// The canonical `nop`. Alignment padding in an executable section must stay
/// executable, and unlike x86 there is only one no-op worth emitting.
const NOP: u32 = 0xd503_201f;

impl Architecture for AArch64 {
    fn name(&self) -> &'static str {
        "aarch64"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["arm64", "armv8", "armv8-a", "aarch64le"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 64,
            syntax: Syntax::Att,
            features: 0,
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    /// A64 has one operand syntax. `.intel_syntax` in a file that also has x86
    /// in it must not make A64 statements unassemblable, so both spellings are
    /// accepted and neither changes anything.
    fn supports_syntax(&self, _syntax: Syntax) -> bool {
        true
    }

    fn elf_machine(&self) -> u16 {
        183 // EM_AARCH64
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// llvm-mc, the reference, aligns every executable section to the 4
    /// bytes of an instruction, whatever is in it. GNU as instead aligns a
    /// section of any kind once an instruction is assembled into it.
    fn section_align(
        &self,
        _state: &ArchState,
        _name: &str,
        flags: &crate::section::SectionFlags,
    ) -> u64 {
        if flags.exec { 4 } else { 1 }
    }

    /// AArch64 writes immediates as `#1`, so `#` is a comment only in the
    /// first column and `//` is the comment everywhere else.
    fn comments(&self) -> crate::arch::CommentSyntax {
        crate::arch::CommentSyntax {
            anywhere: &["//"],
            line_start: &["#"],
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

    /// Darwin's page modifiers, each valid only on the field its name
    /// describes: `@PAGE` on an `adrp`, `@PAGEOFF` on the offset that
    /// completes it, and the `@GOT` pair for a load through the GOT.
    fn modifier_class(
        &self,
        name: &str,
        kind: &crate::section::FixupKind,
    ) -> Option<crate::reloc::RelocClass> {
        use crate::reloc::RelocClass;
        let class = match name {
            "page" => RelocClass::Page,
            "pageoff" => RelocClass::PageOff,
            "gotpage" => RelocClass::GotPage,
            "gotpageoff" => RelocClass::GotPageOff,
            // In data, `sym@GOT` is the address of the symbol's slot.
            "got" if kind.class == RelocClass::Plain && !kind.pcrel => {
                return Some(RelocClass::Got);
            }
            _ => return None,
        };
        match kind.class {
            // llvm-mc branches to the symbol whatever page modifier it has.
            RelocClass::Branch => Some(RelocClass::Branch),
            k => (k == class).then_some(class),
        }
    }

    /// llvm-mc's conventions, as for every AArch64 encoding: code and
    /// addresses counted in bytes, where GNU as counts instructions.
    fn dwarf(&self, _state: &ArchState) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: -4,
                ra_column: 30,
                initial: vec![cfi::Insn::DefCfa(31, 0)],
                fde_encoding: 0x1b,
                eh_frame_align: 8,
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// The AAPCS64 DWARF numbering of the names llvm-mc accepts: `x`/`w`
    /// registers 0-30, the stack pointer and zero register both 31, and a
    /// vector register as 64 up by whichever width names it.
    fn dwarf_register(&self, _state: &ArchState, name: &str) -> Option<u32> {
        match name {
            "sp" | "wsp" | "xzr" | "wzr" => return Some(31),
            "fp" => return Some(29),
            "lr" => return Some(30),
            _ => {}
        }
        numbered_register(name, "x", 31)
            .or_else(|| numbered_register(name, "w", 30))
            .or_else(|| {
                ["b", "h", "s", "d", "q"]
                    .iter()
                    .find_map(|p| numbered_register(name, p, 31))
                    .map(|n| 64 + n)
            })
    }

    /// `.ltorg` and `.pool` write the section's literal pool out here.
    fn directive(
        &self,
        cx: &mut AsmCtx<'_>,
        name: &str,
        _cur: &mut crate::cursor::Cursor<'_>,
    ) -> bool {
        match name {
            ".ltorg" | ".pool" => {
                cx.requests.push(crate::arch::Request::FlushLiterals);
                true
            }
            _ => false,
        }
    }

    /// A64 code is `$x`, and the literal pools and data in a code section
    /// are `$d`, as GNU as marks them.
    fn code_mapping(&self, _state: &ArchState) -> Option<(&'static str, u64)> {
        Some(("$x", 4))
    }

    /// GNU as's `aarch64_init_frag` marks an alignment fragment in a code
    /// section as instructions, not as data.
    fn align_padding_is_code(&self) -> bool {
        true
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        // Padding to a boundary finer than four bytes cannot be instructions,
        // so the leftover is zeroed rather than pretending otherwise.
        let words = (len / 4) as usize;
        for _ in 0..words {
            out.extend_from_slice(&NOP.to_le_bytes());
        }
        out.resize(len as usize, 0);
        out
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        // A mnemonic only the table has is the table's. One the handwritten
        // encoders also have is theirs until an operand is something only a
        // table form takes: `add x0, x1, x2` is handwritten, `add v0.8b,
        // v1.8b, v2.8b` and `add d0, d1, d2` are not, and `ldr d0, [x0]` is
        // handwritten again, since loads and stores take the scalar SIMD
        // registers themselves.
        // The one SME instruction beyond `smstart`/`smstop`, whose `{za}` the
        // operand grammar has no other use for.
        if mnemonic == "zero" {
            return insn::sme_zero(cx, req);
        }
        if table::knows(&mnemonic)
            && (!insn::handwritten(&mnemonic)
                || table::has_simd_operand(cx, req.operands, !insn::loads(&mnemonic)))
        {
            return table::assemble(cx, &mnemonic, req.mnemonic_span, req.operands);
        }
        if !insn::handwritten(&mnemonic) {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        }
        let cur = req.cursor();
        let ops = operand::parse_list(cx, &cur)?;
        insn::assemble(cx, req, &mnemonic, &ops)
    }
}

/// True if `name` is a register, so the generic parser does not treat a
/// register name as a symbol.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    reg::is_register(name) || table::is_register(name)
}

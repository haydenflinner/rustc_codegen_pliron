//! DWARF debugging information the assembler writes itself: line number
//! tables (`.file`, `.loc`, `.debug_line`) and call frame information
//! (`.cfi_*`, `.eh_frame`, `.debug_frame`).
//!
//! Both are recorded while the source is read, as positions in the sections
//! plus what the directives said, and turned into bytes only once layout has
//! settled. At that point every distance between two positions in a section
//! is a number, which is what decides the width of an address advance, so no
//! fragment has to be revisited; see `Assembler::emit_dwarf`.
//!
//! The two reference assemblers agree on the formats and disagree on almost
//! every detail inside them: the default version, how a file name is split
//! into a directory, whether a column carries over to the next `.loc`, which
//! instructions go in a CIE. Each target follows the one that checks its
//! encodings (see [`Flavor`]), and the differences are written down where they
//! are decided.

pub mod cfi;
pub(crate) mod emit;
mod info;
pub mod line;
mod md5;
pub mod source;

use crate::assembler::Assembler;
use crate::section::SectionId;

/// A position in a section: the section, and the index of the fragment that
/// starts there, as a label records it.
pub type Pos = (SectionId, u32);

/// Whose conventions the DWARF sections follow.
///
/// Each target follows the reference that checks its encodings: GNU as for
/// x86, m68k, SuperH, RX, RL78 and V850, llvm-mc for the others. The formats
/// are the same; the choices inside them are not.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Flavor {
    Gnu,
    Llvm,
}

/// What a target's DWARF sections need from its backend.
#[derive(Clone, Debug)]
pub struct DwarfTarget {
    pub flavor: Flavor,
    /// The line table's `minimum_instruction_length`, which address advances
    /// are counted in. GNU as also counts CFA advances in it.
    pub min_insn_length: u8,
    /// Rows advance with `DW_LNS_fixed_advance_pc` rather than special
    /// opcodes, as GNU as does on targets whose linker relaxes code (RL78),
    /// where no distance is final until link time.
    pub fixed_advance_pc: bool,
    /// The prefix of the labels llvm-mc keeps out of the compilation unit
    /// `-g` makes, as it keeps them out of the symbol table: `.L`, but `$`
    /// for 32-bit MIPS.
    pub private_prefix: &'static str,
    /// Call frame information, or `None` where the reference has none, which
    /// makes every `.cfi_*` directive an error.
    pub cfi: Option<CfiTarget>,
}

impl DwarfTarget {
    /// Line tables only, in GNU as's conventions, with no call frame
    /// information: what a backend gets unless it says otherwise.
    pub const fn lines_only(flavor: Flavor, min_insn_length: u8) -> DwarfTarget {
        DwarfTarget {
            flavor,
            min_insn_length,
            fixed_advance_pc: false,
            private_prefix: ".L",
            cfi: None,
        }
    }
}

/// The per-target constants of a CIE. Its code alignment factor is the line
/// table's [`DwarfTarget::min_insn_length`], in both references.
#[derive(Clone, Debug)]
pub struct CfiTarget {
    pub data_align: i32,
    /// The DWARF register holding the return address.
    pub ra_column: u32,
    /// The instructions every frame starts with, before any directive.
    pub initial: Vec<cfi::Insn>,
    /// `DW_EH_PE_*` encoding of an FDE's address fields in `.eh_frame`.
    pub fde_encoding: u8,
    /// Alignment of `.eh_frame`, and of the last FDE in it: GNU as's
    /// `EH_FRAME_ALIGNMENT`, or llvm-mc's pointer size.
    pub eh_frame_align: u64,
    /// The CIE version in `.eh_frame`. 1 everywhere but GNU as for RISC-V.
    pub cie_version: u8,
}

/// DWARF state gathered while the source is read.
#[derive(Default)]
pub struct DwarfState {
    pub line: line::LineState,
    pub cfi: cfi::CfiState,
}

impl Assembler {
    /// The DWARF conventions of the object being written.
    pub(crate) fn dwarf_target(&self) -> DwarfTarget {
        let (arch, state) = self.target_state();
        arch.dwarf(state)
    }

    /// Pins the current position of the current section for a line table
    /// row or a CFI instruction. Seals the section, so that data emitted next
    /// starts a fragment of its own, as a label does.
    pub(crate) fn dwarf_pos(&mut self) -> Pos {
        self.cur_section().seal();
        (self.cur, self.cur_section().next_frag_index())
    }

    /// The offset of a line table row within its section, once layout has
    /// run.
    pub(crate) fn row_addr(&self, row: &line::Row) -> u64 {
        match row.back {
            None => self.pos_offset(row.pos),
            Some(back) => {
                let s = self.section(row.pos.0);
                let (off, size) = s
                    .frags
                    .get(row.pos.1 as usize)
                    .map_or((s.size, 0), |f| (f.offset, f.size()));
                (off + size).saturating_sub(back as u64)
            }
        }
    }

    /// The offset of a position within its section, once layout has run.
    pub(crate) fn pos_offset(&self, pos: Pos) -> u64 {
        let s = self.section(pos.0);
        match s.frags.get(pos.1 as usize) {
            Some(f) => f.offset,
            None => s.size,
        }
    }
}

/// The number in a register name made of `prefix` and a decimal number up to
/// `max`, such as `x12`: the shape most backends' DWARF register names have.
/// Used only by the backends' `dwarf_register`, so a build with none has no
/// caller for it.
#[allow(dead_code)]
pub fn numbered_register(name: &str, prefix: &str, max: u32) -> Option<u32> {
    let digits = name.strip_prefix(prefix)?;
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (n <= max).then_some(n)
}

// ---- encodings ------------------------------------------------------------

pub(crate) fn push_uleb(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&crate::layout::uleb128(v));
}

pub(crate) fn push_sleb(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&crate::layout::sleb128(v));
}

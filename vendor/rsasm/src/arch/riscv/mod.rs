//! RISC-V, RV32 and RV64. `EM_RISCV`.
//!
//! The backend assembles RV32I/RV64I with the M, A, F, D and C extensions, in
//! the GNU as dialect, and expands the pseudo-instructions that most RISC-V
//! source is actually written in.
//!
//! Two things shape the code more than anything else.
//!
//! *Immediates are scattered.* Almost no field is contiguous, so the word is
//! built with the immediate zeroed and the placement lives in a scatter
//! function (see [`encode`]) that runs whether the value is known now or
//! arrives from the linker.
//!
//! *The C extension is a rewrite, not a set of new mnemonics.* Source says
//! `add a0, a0, a1`; the assembler emits two bytes because a compressed form
//! of exactly that instruction exists. [`compress`] does that as a peephole
//! over finished words, and branches — whose displacement is not known until
//! layout runs — instead offer both widths as relaxation candidates.

pub mod asm;
pub mod compress;
pub mod encode;
pub mod insn;
pub mod matint;
pub mod operand;
pub mod pseudo;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, FlatModifier, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::lexer::TokKind;
use crate::section::Variant;
use asm::Asm;
use insn::Kind;
use operand::Operands;
use pseudo::Handled;

pub const NAMES: &[&str] = &["riscv32", "riscv64"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let xlen = match name {
        "riscv32" | "rv32" | "rv32i" | "rv32g" | "rv32gc" => 32,
        "riscv64" | "rv64" | "rv64i" | "rv64g" | "rv64gc" | "riscv" => 64,
        _ => return None,
    };
    Some(Box::new(Riscv { xlen }))
}

pub struct Riscv {
    xlen: u8,
}

/// `ArchState::features` bit 0 says whether the C extension may shorten what
/// is emitted, and bit 1 whether `la` goes through the GOT.
///
/// `.option push` / `.option pop` need a stack, and `ArchState` has no field
/// for one, so the word doubles as it: each push shifts everything left by a
/// level and copies the current settings into the bottom one, each pop shifts
/// right. A single set bit above the deepest level marks the bottom, which is
/// how an unmatched pop is noticed.
const RVC: u64 = 1;
const PIC: u64 = 2;
const LEVEL: u64 = RVC | PIC;
const LEVEL_BITS: u32 = 2;
const STACK_BOTTOM: u64 = 1 << LEVEL_BITS;

/// How many `.option push` levels are open.
fn option_depth(features: u64) -> u32 {
    ((63 - features.leading_zeros()) / LEVEL_BITS).saturating_sub(1)
}

fn rvc_enabled(state: &ArchState) -> bool {
    state.features & RVC != 0
}

/// Whether `.option pic` is in effect.
pub(super) fn pic_enabled(state: &ArchState) -> bool {
    state.features & PIC != 0
}

impl Architecture for Riscv {
    fn name(&self) -> &'static str {
        if self.xlen == 64 {
            "riscv64"
        } else {
            "riscv32"
        }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["rv32", "rv64", "riscv", "rv32gc", "rv64gc"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        self.xlen / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: self.xlen,
            syntax: Syntax::Att,
            // `rv32gc` / `rv64gc` is what toolchains default to, so compressed
            // instructions are on unless `.option norvc` turns them off.
            features: STACK_BOTTOM | RVC,
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // There is only one RISC-V operand syntax; Intel mode is meaningless
        // here, so it is not offered.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        243 // EM_RISCV
    }

    /// `EF_RISCV_RVC | EF_RISCV_FLOAT_ABI_DOUBLE`: the `rv32gc`/`rv64gc`
    /// defaults with their `ilp32d`/`lp64d` ABIs. GNU ld refuses to link
    /// objects whose float ABIs differ, and the flag records the ISA the file
    /// started with, so `.option norvc` leaves it set, as in GNU as.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        0x5
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// llvm-mc, the reference, aligns `.text` to the size of the shortest
    /// instruction in effect when the file starts: 2 bytes with compressed
    /// instructions, 4 without. GNU as does the same.
    fn section_align(
        &self,
        state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        match name {
            ".text" if rvc_enabled(state) => 2,
            ".text" => 4,
            _ => 1,
        }
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    fn difference_relocs(&self, size: u8) -> Option<(u32, u32)> {
        reloc::difference(size)
    }

    fn modifier_reloc(&self, name: &str, size: u8, pcrel: bool) -> Option<u32> {
        // `call foo@plt` is accepted for compatibility; it only renames the
        // relocation on the `auipc`/`jalr` pair.
        (name == "plt" && size == 8 && pcrel).then_some(reloc::CALL_PLT)
    }

    /// A static image has no PLT, so `call foo@plt` calls `foo`.
    fn flat_modifier(&self, name: &str) -> FlatModifier {
        if name == "plt" {
            FlatModifier::Plain
        } else {
            FlatModifier::LinkerOnly
        }
    }

    /// llvm-mc's conventions, as for every RISC-V encoding.
    ///
    /// Without linker relaxation, which rsasm does not do, llvm-mc knows
    /// every distance and writes plain address advances. With it (`-mattr=+relax`)
    /// it writes each one as a `R_RISCV_ADD`/`R_RISCV_SUB` pair for the linker
    /// to fix up, as GNU as does even with `-mno-relax`.
    fn dwarf(&self, _state: &ArchState) -> DwarfTarget {
        let wide = self.xlen == 64;
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: if wide { -8 } else { -4 },
                ra_column: 1,
                initial: vec![cfi::Insn::DefCfa(2, 0)],
                fde_encoding: 0x1b,
                eh_frame_align: if wide { 8 } else { 4 },
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// The psABI DWARF numbering: integer registers 0-31 and floating-point
    /// registers 32-63, under their numbers or ABI names.
    fn dwarf_register(&self, _state: &ArchState, name: &str) -> Option<u32> {
        const ABI: [&str; 32] = [
            "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3",
            "a4", "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11",
            "t3", "t4", "t5", "t6",
        ];
        const FABI: [&str; 32] = [
            "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7", "fs0", "fs1", "fa0", "fa1",
            "fa2", "fa3", "fa4", "fa5", "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7",
            "fs8", "fs9", "fs10", "fs11", "ft8", "ft9", "ft10", "ft11",
        ];
        if name == "fp" {
            return Some(8);
        }
        if let Some(i) = ABI.iter().position(|r| *r == name) {
            return Some(i as u32);
        }
        if let Some(i) = FABI.iter().position(|r| *r == name) {
            return Some(32 + i as u32);
        }
        numbered_register(name, "x", 31)
            .or_else(|| numbered_register(name, "f", 31).map(|n| 32 + n))
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        encode::nop_bytes(len as usize)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        let rvc = rvc_enabled(cx.state);
        let cur = req.cursor();
        let ops = Operands::parse(&cur, req.span);
        let mut a = Asm::new(cx, self.xlen, rvc, req.span);

        match pseudo::expand(&mut a, &mnemonic, &ops) {
            Handled::Done => return Some(a.finish()),
            Handled::Failed => return None,
            Handled::No => {}
        }

        let (base, ordering) = split_ordering(&mnemonic);
        let Some(def) = insn::lookup(base) else {
            a.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        };
        a.encode_def(def, base, &ops, ordering)?;
        Some(a.finish())
    }

    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        if name != ".option" {
            return false;
        }
        let tok = cur.peek();
        cur.set_pos(cur.all().len());
        let TokKind::Ident(n) = tok.kind else {
            cx.error(tok.span, "`.option` needs a name");
            return true;
        };
        let word = cx.name(n).to_ascii_lowercase();
        let features = cx.state.features;
        match word.as_str() {
            "rvc" => cx.state.features |= RVC,
            "norvc" => cx.state.features &= !RVC,
            "pic" => cx.state.features |= PIC,
            "nopic" => cx.state.features &= !PIC,
            "push" if option_depth(features) >= 30 => {
                cx.error(tok.span, "`.option push` nested more than 30 deep");
            }
            "push" => cx.state.features = (features << LEVEL_BITS) | (features & LEVEL),
            "pop" if option_depth(features) == 0 => {
                cx.error(tok.span, "`.option pop` with no `.option push`");
            }
            "pop" => cx.state.features = features >> LEVEL_BITS,
            // Linker relaxation is not implemented, so objects come out as
            // llvm-mc writes them without it (see README.md), and `.option
            // arch` carries an extension list this backend does not track.
            "relax" | "norelax" | "arch" => {}
            _ => cx.error(tok.span, format!("unknown `.option {word}`")),
        }
        true
    }
}

/// Splits the `.aq` / `.rl` ordering suffix off an atomic mnemonic.
///
/// The suffix is part of the mnemonic rather than an operand, and it sets two
/// bits that sit immediately below the `funct5` already in the base word.
fn split_ordering(mnemonic: &str) -> (&str, u32) {
    for (suffix, bits) in [(".aqrl", 3u32), (".aq", 2), (".rl", 1)] {
        if let Some(base) = mnemonic.strip_suffix(suffix)
            && insn::lookup(base).is_some_and(|d| matches!(d.kind, Kind::Amo | Kind::AmoLoad))
        {
            return (base, bits << 25);
        }
    }
    (mnemonic, 0)
}

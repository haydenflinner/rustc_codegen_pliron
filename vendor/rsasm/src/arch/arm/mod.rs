//! ARM 32-bit: the A32 (ARM) and T32 (Thumb) instruction sets. `EM_ARM`.
//!
//! The two instruction sets share a register file, a condition-code table and
//! an operand grammar, but not an encoding, so they share everything up to
//! [`Insn`] and then split into [`encode`] (A32) and [`thumb`] (T32). Which
//! one runs is a property of the *state*, not of the backend object: `.arm`
//! and `.thumb` switch between them mid-file, exactly as `.code32` and
//! `.code16` do on x86, and they are spelled that way too.
//!
//! The state's `bits` field carries the choice — 32 for ARM, 16 for Thumb —
//! because that is what `.code 16` and `.code 32` already mean in ARM sources.

pub mod encode;
pub(crate) mod generic;
pub mod imm;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;
#[doc(hidden)]
pub mod table;
pub mod thumb;

use crate::arch::{
    ArchState, Architecture, AsmCtx, Endian, InsnRequest, Interwork, InterworkTarget, Request,
    Syntax,
};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::lexer::TokKind;
use crate::section::{FixupKind, LinkValue, Variant};
use crate::source::Span;
use crate::symbol::SymType;
use insn::{Mnem, Width};
use operand::Operand;

pub const NAMES: &[&str] = &["arm", "thumb"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let thumb = match name {
        "arm" | "armv7" | "armv7-a" | "arm32" => false,
        "thumb" | "thumbv7" | "thumb2" => true,
        _ => return None,
    };
    Some(Box::new(Arm { thumb }))
}

/// One instruction, after the mnemonic has been taken apart and the operands
/// parsed. Both encoders work from this.
#[derive(Copy, Clone)]
pub struct Insn<'o> {
    /// In a Thumb `it` block, whose condition the instruction takes: the
    /// 16-bit data-processing forms then leave the flags alone.
    pub in_it: bool,
    pub mnem: Mnem,
    pub cond: u8,
    /// Whether the source wrote a condition, as opposed to defaulting to `al`.
    /// Some encodings exist only unconditionally and need to tell the two
    /// apart.
    pub cond_written: bool,
    pub set_flags: bool,
    pub width: Width,
    /// The mnemonic as written, for diagnostics.
    pub text: &'o str,
    pub ops: &'o [Operand],
    pub span: Span,
}

pub struct Arm {
    /// Whether a fresh state starts in Thumb mode.
    thumb: bool,
}

/// The state's `bits` value that means Thumb.
const THUMB_BITS: u8 = 16;

/// Label flag: defined in Thumb code.
const LABEL_THUMB: u8 = 1;
/// Label flag: named by `.thumb_func`.
const LABEL_THUMB_FUNC: u8 = 2;

/// `ArchState::private` bit: a `.thumb_func` is waiting for its label.
const PENDING_THUMB_FUNC: u64 = 1;

/// Whether a label is a Thumb function, the way GNU as's `THUMB_IS_FUNC`
/// decides it for EABI objects: named by `.thumb_func`, or a function by
/// `.type` and defined in Thumb code.
pub fn thumb_is_func(flags: u8, ty: SymType) -> bool {
    flags & LABEL_THUMB_FUNC != 0 || (flags & LABEL_THUMB != 0 && ty == SymType::Func)
}

/// Whether a label is an ARM function: a function by `.type`, defined in ARM
/// code.
pub fn arm_is_func(flags: u8, ty: SymType) -> bool {
    flags & LABEL_THUMB == 0 && ty == SymType::Func
}

// The interwork classes of branch fixups; see `Arm::interwork`.
/// ARM `bl label`.
pub const IW_ARM_BL: u8 = 1;
/// ARM `blx label`.
pub const IW_ARM_BLX: u8 = 2;
/// ARM `b`, `b<cond>` and `bl<cond>`.
pub const IW_ARM_JUMP: u8 = 3;
/// Thumb `bl label`.
pub const IW_THUMB_BL: u8 = 4;
/// Thumb `blx label`.
pub const IW_THUMB_BLX: u8 = 5;
/// Thumb 32-bit `b.w` and `b<cond>.w`.
pub const IW_THUMB_JUMP: u8 = 6;
/// Thumb 16-bit `b` and `b<cond>`, which have no relocation.
pub const IW_THUMB_JUMP16: u8 = 7;
/// The 16-bit form of a Thumb `adr` layout may widen.
pub const IW_THUMB_ADR16: u8 = 8;
/// The 32-bit form of a Thumb `adr` layout chose between the two.
pub const IW_THUMB_ADR: u8 = 9;

impl Architecture for Arm {
    fn name(&self) -> &'static str {
        if self.thumb { "thumb" } else { "arm" }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["armv7", "armv7-a", "arm32", "thumbv7", "thumb2"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: if self.thumb { THUMB_BITS } else { 32 },
            syntax: Syntax::Att,
            features: 0,
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // ARM has one operand syntax; the x86 Intel/AT&T split does not apply.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        40
    }

    /// `EF_ARM_EABI_VER5`, as llvm-mc writes for `arm-linux-gnueabi`; GNU ld
    /// refuses to mix EABI versions, and version 0 is not an EABI object.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        0x0500_0000
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// GNU as, the reference for ARM objects, gives a section no alignment
    /// of its own: the first instruction assembled into it raises it, to 4
    /// bytes for ARM and 2 for Thumb (see `code_mapping`), where llvm-mc
    /// aligns `.text` to 4 bytes from the start. `tools/mc-diff` records
    /// the difference.
    fn section_align(
        &self,
        _state: &ArchState,
        _name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        1
    }

    /// GNU as relocates a branch to a global or weak symbol in the same
    /// section, but resolves a field that has no relocation even then: a
    /// literal load, an `adr`, and a 16-bit branch to a symbol no other
    /// object can replace (`interwork` widens the others).
    fn defers_to_linker(&self, r: &crate::arch::SameSectionRef<'_>) -> bool {
        r.reloc != 0 && r.binding != crate::symbol::Binding::Local
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    /// ARM writes immediates as `#1`, so `#` is a comment only in the first column and `@` takes its place elsewhere.
    fn comments(&self) -> crate::arch::CommentSyntax {
        crate::arch::CommentSyntax {
            anywhere: &["@", "//"],
            line_start: &["#"],
        }
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    /// llvm-mc names a local label in every relocation but these two (its
    /// `ARMELFObjectWriter::needsRelocateWithSymbol`).
    fn relocates_with_label(&self, reloc: u32) -> bool {
        !matches!(reloc, reloc::ABS32 | reloc::PREL31)
    }

    /// llvm-mc's conventions, as for every ARM encoding, in either
    /// instruction set.
    fn dwarf(&self, _state: &ArchState) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: -4,
                ra_column: 14,
                initial: vec![cfi::Insn::DefCfa(13, 0)],
                fde_encoding: 0x1b,
                eh_frame_align: 4,
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// The AAPCS DWARF numbering of the names llvm-mc accepts: the core
    /// registers under their numbers, APCS names and aliases, and the 64-bit
    /// VFP registers from 256. `fp` is `r11` in both instruction sets.
    fn dwarf_register(&self, _state: &ArchState, name: &str) -> Option<u32> {
        const APCS: [&str; 15] = [
            "a1", "a2", "a3", "a4", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "ip", "sp",
            "lr",
        ];
        match name {
            "sb" => return Some(9),
            "sl" => return Some(10),
            "fp" => return Some(11),
            "pc" => return Some(15),
            _ => {}
        }
        if let Some(i) = APCS.iter().position(|r| *r == name) {
            return Some(i as u32);
        }
        numbered_register(name, "r", 15)
            .or_else(|| numbered_register(name, "d", 31).map(|n| 256 + n))
    }

    /// Alignment padding has to stay executable, and the two instruction sets
    /// have different no-ops, so the current mode picks.
    fn nop_fill(&self, state: &ArchState, len: u64) -> Vec<u8> {
        let len = len as usize;
        let thumb_nop = thumb::NOP.to_le_bytes();
        let arm_nop = encode::NOP.to_le_bytes();
        let nop: &[u8] = if state.bits == THUMB_BITS {
            &thumb_nop
        } else {
            &arm_nop
        };
        // A misaligned remainder cannot hold an instruction, so it is zeros,
        // and comes first so that the no-ops after it are aligned: GNU as
        // and llvm-mc both pad that way.
        let mut out = vec![0; len % nop.len()];
        while out.len() < len {
            out.extend_from_slice(nop);
        }
        out
    }

    /// GNU as pads the end of a code section to its alignment, up to a word.
    fn pads_section_tail(&self, flags: &crate::section::SectionFlags) -> bool {
        flags.exec
    }

    fn section_tail_align_limit(&self) -> u64 {
        4
    }

    fn pads_as_last_instruction(&self) -> bool {
        true
    }

    /// Thumb branches, literal loads and `adr` are sized as GNU as's
    /// `arm_relax_frag` sizes them.
    fn relaxation(&self) -> crate::arch::Relaxation {
        crate::arch::Relaxation::EachPass
    }

    fn code_mapping(&self, state: &ArchState) -> Option<(&'static str, u64)> {
        Some(if state.bits == THUMB_BITS {
            ("$t", 2)
        } else {
            ("$a", 4)
        })
    }

    /// GNU as's `arm_frob_label`: a label remembers whether it was defined in
    /// Thumb code, and the first after `.thumb_func` in a code section, other
    /// than a `.L` local, is a Thumb function.
    fn label_flags(&self, state: &mut ArchState, name: &str, in_code: bool) -> u8 {
        let mut flags = 0;
        if state.bits == THUMB_BITS {
            flags |= LABEL_THUMB;
        }
        if state.private & PENDING_THUMB_FUNC != 0 && !name.starts_with(".L") && in_code {
            flags |= LABEL_THUMB_FUNC;
            state.private &= !PENDING_THUMB_FUNC;
        }
        flags
    }

    /// A Thumb function is `STT_FUNC`, and its address has the low bit set so
    /// that a call through it lands in Thumb state. Only a label defined in
    /// Thumb code is marked, though: GNU as marks nothing else, even a label
    /// `.thumb_func` names in ARM code.
    fn elf_symbol(&self, flags: u8, ty: SymType, defined: bool, value: u64) -> (SymType, u64) {
        if thumb_is_func(flags, ty) && flags & LABEL_THUMB != 0 {
            (SymType::Func, if defined { value | 1 } else { value })
        } else {
            (ty, value)
        }
    }

    /// GNU as's `arm_fix_adjustable`: a relocation against a function names
    /// it, so the linker knows which instruction set it is in.
    fn keeps_reloc_symbol(&self, flags: u8, ty: SymType) -> bool {
        ty == SymType::Func || thumb_is_func(flags, ty)
    }

    /// A linker sets the low bit of a Thumb function's address in data.
    fn link_bias(&self, reloc: u32, flags: u8, ty: SymType) -> i64 {
        i64::from((reloc == reloc::ABS32 || reloc == reloc::REL32) && thumb_is_func(flags, ty))
    }

    /// The branches between ARM and Thumb code, as GNU as assembles them and,
    /// for the references it leaves to a linker, as GNU ld links them.
    ///
    /// GNU as resolves a branch to a label in its own section that nothing
    /// outside the object can replace, and turns a call into the other
    /// instruction set into `blx`, or a `blx` that stays in its set into a
    /// call; a jump into the other set it leaves to the linker, which builds
    /// a veneer. A branch to a global symbol is always relocated. GNU ld then
    /// makes the same choice of `bl` or `blx` for the calls it resolves.
    fn interwork(&self, class: u8, t: &InterworkTarget) -> Interwork {
        let thumb = thumb_is_func(t.flags, t.ty);
        let arm = arm_is_func(t.flags, t.ty);
        // What the assembler resolves itself.
        let local = t.same_section && !t.global;
        let arm_blx = Interwork::Becomes {
            patch: encode::to_blx,
            kind: encode::blx_kind(),
        };
        let arm_bl = Interwork::Becomes {
            patch: encode::to_bl,
            kind: encode::bl_kind(),
        };
        let thumb_blx = Interwork::Becomes {
            patch: thumb::to_blx,
            kind: thumb::blx_kind(),
        };
        let thumb_bl = Interwork::Becomes {
            patch: thumb::to_bl,
            kind: if t.relocatable || local {
                thumb::blx_as_bl_kind()
            } else {
                thumb::bl_kind()
            },
        };
        // GNU as sets the low bit of a Thumb function's address in an `adr`
        // it relaxed once every symbol is known, which also makes it 32 bits.
        match class {
            IW_THUMB_ADR16 if thumb => return Interwork::Relocate,
            IW_THUMB_ADR if thumb => {
                return Interwork::Becomes {
                    patch: |w| w,
                    kind: FixupKind {
                        link: LinkValue::Split(|v| v | 1),
                        ..thumb::adr32_kind()
                    },
                };
            }
            IW_THUMB_ADR16 | IW_THUMB_ADR => return Interwork::AsWritten,
            _ => {}
        }
        // A 16-bit branch has no relocation, so where the target is not
        // certain to stay put, GNU as takes the 32-bit form.
        if class == IW_THUMB_JUMP16 {
            return if arm || t.preemptible {
                Interwork::Relocate
            } else {
                Interwork::AsWritten
            };
        }
        if t.relocatable || local {
            return match class {
                IW_ARM_BL if local && thumb => arm_blx,
                IW_ARM_BLX if local && arm => arm_bl,
                IW_THUMB_BL if local && arm => thumb_blx,
                IW_THUMB_BLX if local && thumb => thumb_bl,
                IW_ARM_JUMP if thumb => Interwork::Relocate,
                IW_THUMB_JUMP if arm => Interwork::Relocate,
                // A branch to a global symbol is the core's to leave to the
                // linker; see `defers_to_linker`.
                _ => Interwork::AsWritten,
            };
        }
        // What GNU ld does with the relocation.
        match class {
            IW_ARM_BL if thumb => arm_blx,
            // A `blx` against a label reaches the linker as its section
            // symbol, which it takes to be ARM code.
            IW_ARM_BLX if !thumb && (arm || !t.global) => arm_bl,
            IW_THUMB_BL if arm => thumb_blx,
            IW_THUMB_BLX if thumb => thumb_bl,
            IW_ARM_JUMP if thumb => Interwork::LinkerOnly("an ARM-to-Thumb veneer"),
            IW_THUMB_JUMP if arm => Interwork::LinkerOnly("a Thumb-to-ARM veneer"),
            _ => Interwork::AsWritten,
        }
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let text = cx.name(req.mnemonic).to_ascii_lowercase();
        let Some(r) = insn::resolve(&text) else {
            cx.error(req.mnemonic_span, format!("unknown instruction `{text}`"));
            return None;
        };
        let ops = {
            let mut cur = req.cursor();
            let mut p = operand::Parser { cx };
            p.parse_list(&mut cur)?
        };
        // `!` requests writeback, which only the block transfers' base
        // register has, along with `srs` and `rfe` from the table; anywhere
        // else it would be silently meaningless.
        let takes_writeback = matches!(r.mnem, Mnem::Ldm(_) | Mnem::Stm(_) | Mnem::Ext(_));
        if let Some(op) = ops
            .iter()
            .enumerate()
            .find_map(|(i, op)| (op.writeback && !(takes_writeback && i == 0)).then_some(op))
        {
            cx.error(
                op.span,
                "`!` (writeback) is only valid on the base register of `ldm`/`stm`",
            );
            return None;
        }
        let ins = Insn {
            in_it: false,
            mnem: r.mnem,
            cond: r.cond,
            cond_written: r.cond_written,
            set_flags: r.set_flags,
            width: r.width,
            text: &text,
            ops: &ops,
            span: req.span,
        };
        if cx.state.bits == THUMB_BITS {
            thumb::assemble(cx, &ins)
        } else {
            encode::assemble(cx, &ins)
        }
    }

    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        match name {
            ".arm" | ".code32" => {
                set_mode(cx, false);
                true
            }
            ".thumb" | ".code16" => {
                set_mode(cx, true);
                true
            }
            ".code" => {
                // `.code 16` / `.code 32`, the spelling ARM sources use.
                match cur.peek().kind {
                    TokKind::Int(16) => set_mode(cx, true),
                    TokKind::Int(32) => set_mode(cx, false),
                    _ => {
                        let span = cur.peek().span;
                        cx.error(span, "`.code` expects 16 or 32");
                        return true;
                    }
                }
                cur.advance();
                true
            }
            ".ltorg" | ".pool" => {
                cx.requests.push(Request::FlushLiterals);
                true
            }
            // The label after `.thumb_func` is a Thumb function; see
            // `label_flags`. The directive also switches to Thumb.
            ".thumb_func" => {
                set_mode(cx, true);
                cx.state.private |= PENDING_THUMB_FUNC;
                true
            }
            // Unified syntax is the only syntax this backend implements.
            ".syntax" | ".fpu" | ".eabi_attribute" | ".arch_extension" => {
                cur.set_pos(cur.all().len());
                true
            }
            _ => false,
        }
    }
}

/// Switches between ARM and Thumb, as GNU as's `.arm` and `.thumb` do: the
/// section's alignment is raised to two bytes, and ARM code after Thumb
/// starts on a word boundary, padded with zeros rather than no-ops.
fn set_mode(cx: &mut AsmCtx<'_>, thumb: bool) {
    if (cx.state.bits == THUMB_BITS) == thumb {
        return;
    }
    cx.state.bits = if thumb { THUMB_BITS } else { 32 };
    if !thumb {
        cx.requests.push(Request::AlignZero(4));
    }
    cx.requests.push(Request::RecordAlign(2));
}

/// True if `name` is an ARM register, for callers that need to avoid treating
/// register names as symbols.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    reg::is_register(name)
}

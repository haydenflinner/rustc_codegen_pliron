//! Mach-O relocatable object output (`MH_OBJECT`), for x86-64 and arm64.
//!
//! Mach-O is not ELF with other numbers. Three things about it shape both
//! this writer and the few decisions the layout pass has to make differently
//! (all of them behind [`Format::MachO`](crate::output::Format::MachO)):
//!
//! * **Sections belong to segments, and a section is named by the pair.**
//!   `.text` is `__TEXT,__text`, and the source can name any pair with
//!   `.section __DATA,__foo`. All sections of an object share one nameless
//!   segment and one address space, so every section has an address here,
//!   unlike in a relocatable ELF object where each starts at zero.
//!
//! * **A relocation has no addend field.** What ELF puts in `r_addend` goes
//!   into the field being relocated, biased as each relocation type expects,
//!   and a difference of two symbols needs a `SUBTRACTOR`/`UNSIGNED` pair
//!   because one entry can only add. On arm64, where the fields are scattered
//!   through the instruction word and cannot hold an addend at all, the addend
//!   is a relocation of its own (`ARM64_RELOC_ADDEND`).
//!
//! * **Code is made of atoms.** A label whose name does not start with `L` is
//!   a *linker-visible* symbol, and the linker may move the code from it up to
//!   the next such label independently of everything around it. A reference
//!   that crosses from one atom into another therefore cannot be resolved
//!   here, however close the two are and whether or not the target is global;
//!   it is relocated against the target's atom, with the distance from the
//!   atom carried as the addend. That is what the `Atoms` table below is for,
//!   and it is why a Mach-O object has relocations where an ELF one has none.
//!
//! What is written: one `LC_SEGMENT_64` with every section; `LC_BUILD_VERSION`
//! where the source gave `.build_version`, and `LC_DATA_IN_CODE` where it
//! marked data in code; and `LC_SYMTAB` with `LC_DYSYMTAB`, whose three-way
//! split of the symbol table is required, not optional, unless there are no
//! symbols at all. As far as llvm-mc 22 writes the same object, rsasm writes it
//! byte for byte, down to the order of the symbols and the string table's
//! shared tails; `tools/macho-diff` checks that.

mod directives;
mod relocations;

use super::OutputError;
use crate::assembler::{Assembler, Relocation};
use crate::reloc::RelocClass;
use crate::section::SectionId;
use crate::symbol::{Binding, SymbolId, SymbolValue, Visibility};
use std::collections::{HashMap, HashSet};

// ---- the format's constants -------------------------------------------------

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_OBJECT: u32 = 1;
const MH_SUBSECTIONS_VIA_SYMBOLS: u32 = 0x2000;

const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_SUBTYPE_ARM64_ALL: u32 = 0;

const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_SEGMENT_64: u32 = 0x19;
const LC_DATA_IN_CODE: u32 = 0x29;
const LC_BUILD_VERSION: u32 = 0x32;

const SEGMENT_COMMAND_64_SIZE: u32 = 72;
const SECTION_64_SIZE: u32 = 80;
const SYMTAB_COMMAND_SIZE: u32 = 24;
const DYSYMTAB_COMMAND_SIZE: u32 = 80;
const BUILD_VERSION_COMMAND_SIZE: u32 = 24;
const LINKEDIT_DATA_COMMAND_SIZE: u32 = 16;
const DATA_IN_CODE_ENTRY_SIZE: u64 = 8;
const HEADER_SIZE: u32 = 32;
const NLIST_64_SIZE: u64 = 16;
const RELOCATION_SIZE: u64 = 8;

// Section types (the low byte of `flags`).
pub(crate) const S_REGULAR: u32 = 0x0;
pub(crate) const S_ZEROFILL: u32 = 0x1;
pub(crate) const S_CSTRING_LITERALS: u32 = 0x2;
pub(crate) const S_4BYTE_LITERALS: u32 = 0x3;
pub(crate) const S_8BYTE_LITERALS: u32 = 0x4;
pub(crate) const S_LITERAL_POINTERS: u32 = 0x5;
pub(crate) const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
pub(crate) const S_LAZY_SYMBOL_POINTERS: u32 = 0x7;
pub(crate) const S_SYMBOL_STUBS: u32 = 0x8;
pub(crate) const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
pub(crate) const S_MOD_TERM_FUNC_POINTERS: u32 = 0xa;
pub(crate) const S_COALESCED: u32 = 0xb;
pub(crate) const S_GB_ZEROFILL: u32 = 0xc;
pub(crate) const S_INTERPOSING: u32 = 0xd;
pub(crate) const S_16BYTE_LITERALS: u32 = 0xe;
pub(crate) const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
pub(crate) const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
pub(crate) const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
pub(crate) const S_THREAD_LOCAL_VARIABLE_POINTERS: u32 = 0x14;
pub(crate) const S_THREAD_LOCAL_INIT_FUNCTION_POINTERS: u32 = 0x15;

pub(crate) const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
pub(crate) const S_ATTR_NO_TOC: u32 = 0x4000_0000;
pub(crate) const S_ATTR_STRIP_STATIC_SYMS: u32 = 0x2000_0000;
pub(crate) const S_ATTR_NO_DEAD_STRIP: u32 = 0x1000_0000;
pub(crate) const S_ATTR_LIVE_SUPPORT: u32 = 0x0800_0000;
pub(crate) const S_ATTR_SELF_MODIFYING_CODE: u32 = 0x0400_0000;
pub(crate) const S_ATTR_DEBUG: u32 = 0x0200_0000;
pub(crate) const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

// `n_type`.
const N_UNDF: u8 = 0x0;
const N_ABS: u8 = 0x2;
const N_SECT: u8 = 0xe;
const N_EXT: u8 = 0x1;
const N_TYPE: u8 = 0xe;
const N_PEXT: u8 = 0x10;

// `n_desc` bits.
pub(crate) const N_NO_DEAD_STRIP: u16 = 0x0020;
pub(crate) const N_WEAK_REF: u16 = 0x0040;
pub(crate) const N_WEAK_DEF: u16 = 0x0080;
pub(crate) const N_ALT_ENTRY: u16 = 0x0200;

// Relocation types, per machine.
mod x86_64_reloc {
    pub(crate) const UNSIGNED: u8 = 0;
    pub(crate) const SIGNED: u8 = 1;
    pub(crate) const BRANCH: u8 = 2;
    pub(crate) const GOT_LOAD: u8 = 3;
    pub(crate) const GOT: u8 = 4;
    pub(crate) const SUBTRACTOR: u8 = 5;
    pub(crate) const SIGNED_1: u8 = 6;
    pub(crate) const SIGNED_2: u8 = 7;
    pub(crate) const SIGNED_4: u8 = 8;
}

mod arm64_reloc {
    pub(crate) const UNSIGNED: u8 = 0;
    pub(crate) const SUBTRACTOR: u8 = 1;
    pub(crate) const BRANCH26: u8 = 2;
    pub(crate) const PAGE21: u8 = 3;
    pub(crate) const PAGEOFF12: u8 = 4;
    pub(crate) const GOT_LOAD_PAGE21: u8 = 5;
    pub(crate) const GOT_LOAD_PAGEOFF12: u8 = 6;
    pub(crate) const POINTER_TO_GOT: u8 = 7;
    pub(crate) const ADDEND: u8 = 10;
}

/// The machines this writer can produce objects for.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Cpu {
    X86_64,
    Arm64,
}

impl Cpu {
    /// The Mach-O machine an architecture backend targets, if it has one.
    pub(crate) fn for_arch(arch: &dyn crate::arch::Architecture) -> Option<Cpu> {
        match arch.elf_machine() {
            62 => Some(Cpu::X86_64),
            183 => Some(Cpu::Arm64),
            _ => None,
        }
    }

    fn header(self) -> (u32, u32) {
        match self {
            Cpu::X86_64 => (CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL),
            Cpu::Arm64 => (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL),
        }
    }

    /// Whether the machine's PC-relative relocations are trusted to carry a
    /// reference from one atom to another in the same section.
    ///
    /// Only x86-64's are: llvm-mc (`hasReliableSymbolDifference`) resolves
    /// such a reference on every other machine unless the file asks for its
    /// atoms to be kept apart; see [`defers_to_linker`].
    fn reliable_symbol_difference(self) -> bool {
        self == Cpu::X86_64
    }

    /// Whether every section is given a `ltmpN` label at its start, so that a
    /// relocation naming a position with no atom has a symbol to name.
    ///
    /// llvm-mc does this for arm64, whose relocations must all be external.
    fn labels_sections(self) -> bool {
        self == Cpu::Arm64
    }
}

// ---- assembler-side state ---------------------------------------------------

/// The Mach-O platform of a `.build_version`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct BuildVersion {
    pub(crate) platform: u32,
    /// Packed `xxxx.yy.zz`, as Mach-O stores versions.
    pub(crate) minos: u32,
    pub(crate) sdk: u32,
}

/// What one section is in Mach-O's terms, as its directive declared it. Its
/// name holds the segment and section names; see [`split_name`].
#[derive(Clone, Debug)]
pub(crate) struct SectionInfo {
    /// The section type, the low byte of `flags`.
    pub(crate) ty: u32,
    /// `S_ATTR_*` bits.
    pub(crate) attrs: u32,
    /// The stub size of a `symbol_stubs` section.
    pub reserved2: u32,
}

/// A stretch of data in code, from `.data_region` to `.end_data_region`,
/// which `LC_DATA_IN_CODE` tells a disassembler not to decode.
#[derive(Clone, Debug)]
pub(crate) struct DataRegion {
    /// `DICE_KIND_*`: data, or a jump table of 8, 16 or 32-bit entries.
    pub(crate) kind: u16,
    pub(crate) start: SymbolId,
    pub(crate) end: Option<SymbolId>,
    pub(crate) span: crate::source::Span,
}

/// Everything the source told the assembler that only Mach-O output cares
/// about.
#[derive(Default)]
pub(crate) struct State {
    pub(crate) subsections_via_symbols: bool,
    pub(crate) build_version: Option<BuildVersion>,
    pub(crate) sections: HashMap<SectionId, SectionInfo>,
    /// `n_desc` bits from `.weak_definition`, `.weak_reference`,
    /// `.alt_entry` and `.no_dead_strip`.
    pub(crate) symbol_desc: HashMap<SymbolId, u16>,
    /// The symbols `.set` or `.equ` defined, as opposed to `=`.
    pub(crate) set_constants: HashSet<SymbolId>,
    /// The data regions, in the order they were opened.
    pub(crate) data_regions: Vec<DataRegion>,
    /// How many symbols there were when each section was created, which is
    /// where its arm64 section label goes among them.
    pub(crate) section_marks: HashMap<SectionId, u32>,
    /// Where every atom starts, once the source has been read; see [`Atoms`].
    pub(crate) atoms: Atoms,
}

impl State {
    pub(crate) fn desc(&self, id: SymbolId) -> u16 {
        self.symbol_desc.get(&id).copied().unwrap_or(0)
    }
}

// ---- atoms ------------------------------------------------------------------

/// Where each atom of each section begins.
///
/// An atom starts at every linker-visible label — one whose name does not
/// start with `L` — and runs to the next. Positions are kept as fragment
/// indices rather than addresses so that the table stays valid while layout is
/// still moving things about, together with the order each label was defined
/// in: of several labels at one position, one defined before the
/// linker-visible label still ends the atom before it, as it does in llvm-mc,
/// which starts a fragment at every linker-visible label.
#[derive(Default)]
pub(crate) struct Atoms {
    /// Per section, `(fragment, definition order, symbol)`, in that order.
    starts: HashMap<SectionId, Vec<(u32, u32, SymbolId)>>,
}

impl Atoms {
    /// The symbol whose atom covers fragment `frag` of `section`, if any.
    /// Every label at that fragment counts as before it.
    pub(crate) fn at(&self, section: SectionId, frag: u32) -> Option<SymbolId> {
        self.before(section, frag, u32::MAX)
    }

    /// The last linker-visible label at or before `(frag, order)`.
    fn before(&self, section: SectionId, frag: u32, order: u32) -> Option<SymbolId> {
        let list = self.starts.get(&section)?;
        let i = list.partition_point(|&(f, o, _)| (f, o) <= (frag, order));
        (i > 0).then(|| list[i - 1].2)
    }

    /// The atom a symbol belongs to: itself when it is linker-visible.
    pub(crate) fn of(&self, asm: &Assembler, id: SymbolId) -> Option<SymbolId> {
        let sym = asm.symbols.get(id);
        if !is_temporary(asm.interner.get(sym.name)) {
            return sym.is_defined().then_some(id);
        }
        let SymbolValue::Label { section, frag } = sym.value else {
            return None;
        };
        // A literal section is cut into atoms by its contents, not by labels,
        // so a label in one names no atom.
        atomizable(asm, section).then(|| self.before(section, frag, sym.def_order))?
    }
}

/// Collects the atom starts of every section. Called once the source has been
/// read, before layout resolves anything.
pub(crate) fn atoms(asm: &Assembler) -> Atoms {
    let mut starts: HashMap<SectionId, Vec<(u32, u32, SymbolId)>> = HashMap::new();
    for (id, sym) in asm.symbols.iter() {
        let SymbolValue::Label { section, frag } = sym.value else {
            continue;
        };
        if is_temporary(asm.interner.get(sym.name)) {
            continue;
        }
        starts
            .entry(section)
            .or_default()
            .push((frag, sym.def_order, id));
    }
    for list in starts.values_mut() {
        list.sort_by_key(|&(f, o, _)| (f, o));
    }
    Atoms { starts }
}

/// Whether a symbol is assembler-local, which in Mach-O is decided by the
/// name alone: Darwin's private label prefix is `L`. rsasm's own made-up
/// labels carry a NUL, which no source can spell, and are local too.
pub(crate) fn is_temporary(name: &str) -> bool {
    name.starts_with('L') || name.contains('\u{0}')
}

/// Whether a section is cut into atoms by the labels in it.
///
/// A literal section is not: its contents are cut up and merged by the linker
/// item by item, so a reference into one has to name the label it refers to
/// rather than a position. This is `MCAsmInfoDarwin::isSectionAtomizableBySymbols`.
pub(crate) fn atomizable(asm: &Assembler, section: SectionId) -> bool {
    let Some(info) = asm.macho.sections.get(&section) else {
        return true;
    };
    !matches!(
        info.ty,
        S_CSTRING_LITERALS
            | S_4BYTE_LITERALS
            | S_8BYTE_LITERALS
            | S_16BYTE_LITERALS
            | S_LITERAL_POINTERS
            | S_NON_LAZY_SYMBOL_POINTERS
            | S_LAZY_SYMBOL_POINTERS
            | S_MOD_INIT_FUNC_POINTERS
            | S_MOD_TERM_FUNC_POINTERS
            | S_INTERPOSING
            | S_THREAD_LOCAL_VARIABLE_POINTERS
    )
}

/// Whether a PC-relative reference from fragment `frag` of `section` to
/// `target`, defined in that same section, still has to reach the linker.
///
/// Within one atom the answer is no: the two move together whatever the
/// linker does. Across atoms it is yes, since either may be dropped or moved
/// on its own — and unlike ELF, that has nothing to do with the symbol's
/// binding. That is all there is to it on x86-64. On arm64 llvm-mc resolves
/// a reference to an assembler-local label anywhere in the section, and one
/// to any label unless the file has `.subsections_via_symbols`, which is
/// what tells the linker it may really take the atoms apart.
pub(crate) fn defers_to_linker(
    asm: &Assembler,
    target: SymbolId,
    section: SectionId,
    frag: u32,
) -> bool {
    let Some(cpu) = Cpu::for_arch(asm.target()) else {
        return false;
    };
    // On a machine without reliable differences, llvm-mc takes any label to
    // be in the atom of whatever refers to it from the same section, unless
    // the file says its atoms are real with `.subsections_via_symbols`; and
    // an assembler-local label to be in it either way.
    if !cpu.reliable_symbol_difference()
        && (!asm.macho.subsections_via_symbols
            || is_temporary(asm.interner.get(asm.symbols.get(target).name)))
    {
        return false;
    }
    asm.macho.atoms.of(asm, target) != asm.macho.atoms.at(section, frag)
}

/// Whether a difference of two symbols in one section is a number the
/// assembler can work out, rather than a pair of relocations.
///
/// Only within an atom, on every machine; a difference that was a fixed
/// distance where the source wrote it was folded then, before there were
/// atoms (see `Assembler::macho_fixed_difference`).
pub(crate) fn folds_difference(asm: &Assembler, plus: SymbolId, minus: SymbolId) -> bool {
    asm.macho.atoms.of(asm, plus) == asm.macho.atoms.of(asm, minus)
}

/// The relocation type a fixup's description maps to, or `None` where the
/// machine has none — which is how `adr x0, sym` and a conditional branch to
/// another atom are refused, as llvm-mc refuses them.
pub(crate) fn reloc_type(cpu: Cpu, r: &Relocation) -> Option<u8> {
    let d = &r.desc;
    match cpu {
        Cpu::X86_64 => Some(match d.class {
            RelocClass::Branch if d.size == 4 => x86_64_reloc::BRANCH,
            // In data, `@GOTPCREL` is the slot relative to the field, with
            // the source supplying any bias itself.
            RelocClass::Got if d.size == 4 || (d.size == 8 && !d.pcrel) => x86_64_reloc::GOT,
            // Only a load of the slot itself can become a `leaq` of the symbol.
            RelocClass::GotLoad if d.pcrel && d.size == 4 && r.addend == 0 => {
                x86_64_reloc::GOT_LOAD
            }
            RelocClass::GotLoad if d.pcrel && d.size == 4 => x86_64_reloc::GOT,
            // llvm-mc picks the `SIGNED_n` variant by what the field holds,
            // which is the addend less the bytes after the field, rather than
            // by those bytes alone: `leaq _x-4(%rip)` is a `SIGNED_4` as well.
            RelocClass::Plain if d.pcrel && d.size == 4 => match r.addend + field_bias(r) {
                -1 => x86_64_reloc::SIGNED_1,
                -2 => x86_64_reloc::SIGNED_2,
                -4 => x86_64_reloc::SIGNED_4,
                _ => x86_64_reloc::SIGNED,
            },
            RelocClass::Plain if !d.pcrel => x86_64_reloc::UNSIGNED,
            // A sign-extended field can hold a difference, which the linker
            // checks, but not an address, which it could not.
            RelocClass::SignExtended if d.subtrahend.is_some() => x86_64_reloc::UNSIGNED,
            _ => return None,
        }),
        Cpu::Arm64 => Some(match d.class {
            RelocClass::Branch if d.size == 4 => arm64_reloc::BRANCH26,
            RelocClass::Page => arm64_reloc::PAGE21,
            RelocClass::PageOff => arm64_reloc::PAGEOFF12,
            RelocClass::GotPage => arm64_reloc::GOT_LOAD_PAGE21,
            RelocClass::GotPageOff => arm64_reloc::GOT_LOAD_PAGEOFF12,
            RelocClass::Got if matches!(d.size, 4 | 8) => arm64_reloc::POINTER_TO_GOT,
            RelocClass::Plain if !d.pcrel => arm64_reloc::UNSIGNED,
            _ => return None,
        }),
    }
}

/// Whether a relocation of type `ty` is PC-relative, which its entry says
/// whatever the fixup it came from was: an `adrp` is a page count to rsasm,
/// not a distance, but its relocation is still measured from the instruction.
fn entry_pcrel(cpu: Cpu, ty: u8, r: &Relocation) -> bool {
    match cpu {
        Cpu::X86_64 => !matches!(ty, x86_64_reloc::UNSIGNED | x86_64_reloc::SUBTRACTOR),
        Cpu::Arm64 => match ty {
            arm64_reloc::BRANCH26 | arm64_reloc::PAGE21 | arm64_reloc::GOT_LOAD_PAGE21 => true,
            // As `sym@GOT - .`.
            arm64_reloc::POINTER_TO_GOT => r.desc.pcrel,
            _ => false,
        },
    }
}

/// Where a relocation of type `ty` keeps what the field would otherwise hold.
#[derive(Copy, Clone, PartialEq, Eq)]
enum AddendPlace {
    /// In the field, as a plain integer.
    Field,
    /// In an `ARM64_RELOC_ADDEND` entry ahead of it, the field holding zero:
    /// the instruction relocations, whose value is scattered through a word
    /// that has no room for more.
    Entry,
    /// Nowhere: an arm64 GOT relocation names the slot, which has no
    /// offset, and llvm-mc drops one written in data.
    None,
}

fn addend_place(cpu: Cpu, ty: u8) -> AddendPlace {
    match (cpu, ty) {
        (Cpu::X86_64, _) => AddendPlace::Field,
        (Cpu::Arm64, arm64_reloc::BRANCH26 | arm64_reloc::PAGE21 | arm64_reloc::PAGEOFF12) => {
            AddendPlace::Entry
        }
        (Cpu::Arm64, arm64_reloc::UNSIGNED) => AddendPlace::Field,
        (Cpu::Arm64, _) => AddendPlace::None,
    }
}

/// What a PC-relative relocation's field holds beyond the addend. Mach-O's
/// PC-relative relocations are measured from the end of the *field*, where
/// x86 measures from the end of the instruction, so the field is short by
/// the bytes of the instruction that follow it; `X86_64_RELOC_SIGNED_1/2/4`
/// exist to tell the linker so, though llvm-mc picks them by the result.
fn field_bias(r: &Relocation) -> i64 {
    if r.desc.pcrel {
        -(r.desc.trailing as i64)
    } else {
        0
    }
}

// ---- section names ----------------------------------------------------------

/// A section a shorthand directive such as `.cstring` switches to.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Shorthand {
    pub(crate) segment: &'static str,
    pub(crate) section: &'static str,
    pub(crate) ty: u32,
    pub(crate) attrs: u32,
    /// The alignment the directive gives the section, in bytes.
    pub(crate) align: u64,
    pub reserved2: u32,
}

/// The section a shorthand directive names, as Darwin's assembler defines
/// them: the pair, its type and attributes, and for the literal and pointer
/// sections an alignment of their element size.
pub(crate) fn shorthand(name: &str) -> Option<Shorthand> {
    let (segment, section, ty, attrs, align, reserved2) = match name {
        ".text" => (
            "__TEXT",
            "__text",
            S_REGULAR,
            S_ATTR_PURE_INSTRUCTIONS,
            1,
            0,
        ),
        ".data" => ("__DATA", "__data", S_REGULAR, 0, 1, 0),
        ".bss" => ("__DATA", "__bss", S_ZEROFILL, 0, 1, 0),
        ".const" => ("__TEXT", "__const", S_REGULAR, 0, 1, 0),
        ".static_const" => ("__TEXT", "__static_const", S_REGULAR, 0, 1, 0),
        ".cstring" => ("__TEXT", "__cstring", S_CSTRING_LITERALS, 0, 1, 0),
        ".literal4" => ("__TEXT", "__literal4", S_4BYTE_LITERALS, 0, 4, 0),
        ".literal8" => ("__TEXT", "__literal8", S_8BYTE_LITERALS, 0, 8, 0),
        ".literal16" => ("__TEXT", "__literal16", S_16BYTE_LITERALS, 0, 16, 0),
        ".constructor" => ("__TEXT", "__constructor", S_REGULAR, 0, 1, 0),
        ".destructor" => ("__TEXT", "__destructor", S_REGULAR, 0, 1, 0),
        ".const_data" => ("__DATA", "__const", S_REGULAR, 0, 1, 0),
        ".static_data" => ("__DATA", "__static_data", S_REGULAR, 0, 1, 0),
        ".mod_init_func" => (
            "__DATA",
            "__mod_init_func",
            S_MOD_INIT_FUNC_POINTERS,
            0,
            4,
            0,
        ),
        ".mod_term_func" => (
            "__DATA",
            "__mod_term_func",
            S_MOD_TERM_FUNC_POINTERS,
            0,
            4,
            0,
        ),
        ".non_lazy_symbol_pointer" => (
            "__DATA",
            "__nl_symbol_ptr",
            S_NON_LAZY_SYMBOL_POINTERS,
            0,
            4,
            0,
        ),
        ".lazy_symbol_pointer" => ("__DATA", "__la_symbol_ptr", S_LAZY_SYMBOL_POINTERS, 0, 4, 0),
        ".tdata" => ("__DATA", "__thread_data", S_THREAD_LOCAL_REGULAR, 0, 1, 0),
        ".tlv" => ("__DATA", "__thread_vars", S_THREAD_LOCAL_VARIABLES, 0, 1, 0),
        ".thread_init_func" => (
            "__DATA",
            "__thread_init",
            S_THREAD_LOCAL_INIT_FUNCTION_POINTERS,
            0,
            1,
            0,
        ),
        _ => return None,
    };
    Some(Shorthand {
        segment,
        section,
        ty,
        attrs,
        align,
        reserved2,
    })
}

/// The section type a `.section` directive's third argument names.
pub(crate) fn section_type(name: &str) -> Option<u32> {
    Some(match name {
        "regular" => S_REGULAR,
        "cstring_literals" => S_CSTRING_LITERALS,
        "4byte_literals" => S_4BYTE_LITERALS,
        "8byte_literals" => S_8BYTE_LITERALS,
        "16byte_literals" => S_16BYTE_LITERALS,
        "literal_pointers" => S_LITERAL_POINTERS,
        "non_lazy_symbol_pointers" => S_NON_LAZY_SYMBOL_POINTERS,
        "lazy_symbol_pointers" => S_LAZY_SYMBOL_POINTERS,
        "symbol_stubs" => S_SYMBOL_STUBS,
        "mod_init_funcs" => S_MOD_INIT_FUNC_POINTERS,
        "mod_term_funcs" => S_MOD_TERM_FUNC_POINTERS,
        "coalesced" => S_COALESCED,
        "zerofill" => S_ZEROFILL,
        "gb_zerofill" => S_GB_ZEROFILL,
        "interposing" => S_INTERPOSING,
        "thread_local_regular" => S_THREAD_LOCAL_REGULAR,
        "thread_local_zerofill" => S_THREAD_LOCAL_ZEROFILL,
        "thread_local_variables" => S_THREAD_LOCAL_VARIABLES,
        "thread_local_variable_pointers" => S_THREAD_LOCAL_VARIABLE_POINTERS,
        "thread_local_init_function_pointers" => S_THREAD_LOCAL_INIT_FUNCTION_POINTERS,
        _ => return None,
    })
}

/// The `S_ATTR_*` bit a `.section` attribute name asks for.
pub(crate) fn section_attribute(name: &str) -> Option<u32> {
    Some(match name {
        "none" => 0,
        "pure_instructions" => S_ATTR_PURE_INSTRUCTIONS,
        "no_toc" => S_ATTR_NO_TOC,
        "strip_static_syms" => S_ATTR_STRIP_STATIC_SYMS,
        "no_dead_strip" => S_ATTR_NO_DEAD_STRIP,
        "live_support" => S_ATTR_LIVE_SUPPORT,
        "self_modifying_code" => S_ATTR_SELF_MODIFYING_CODE,
        "debug" => S_ATTR_DEBUG,
        _ => return None,
    })
}

/// The type and attributes of a section llvm-mc knows before it reads any
/// source, which it keeps whatever a `.section` directive naming it says.
pub(crate) fn precreated(segment: &str, section: &str) -> Option<(u32, u32)> {
    Some(match (segment, section) {
        ("__TEXT", "__text") => (S_REGULAR, S_ATTR_PURE_INSTRUCTIONS),
        ("__TEXT", "__cstring") => (S_CSTRING_LITERALS, 0),
        ("__TEXT", "__literal4") => (S_4BYTE_LITERALS, 0),
        ("__TEXT", "__literal8") => (S_8BYTE_LITERALS, 0),
        ("__TEXT", "__literal16") => (S_16BYTE_LITERALS, 0),
        ("__TEXT", "__const") => (S_REGULAR, 0),
        ("__DATA", "__data") => (S_REGULAR, 0),
        ("__DATA", "__const") => (S_REGULAR, 0),
        ("__DATA", "__bss") => (S_ZEROFILL, 0),
        ("__DATA", "__common") => (S_ZEROFILL, 0),
        ("__DATA", "__mod_init_func") => (S_MOD_INIT_FUNC_POINTERS, 0),
        ("__DATA", "__mod_term_func") => (S_MOD_TERM_FUNC_POINTERS, 0),
        ("__DATA", "__la_symbol_ptr") => (S_LAZY_SYMBOL_POINTERS, 0),
        ("__DATA", "__nl_symbol_ptr") => (S_NON_LAZY_SYMBOL_POINTERS, 0),
        ("__DATA", "__thread_vars") => (S_THREAD_LOCAL_VARIABLES, 0),
        ("__DATA", "__thread_bss") => (S_THREAD_LOCAL_ZEROFILL, 0),
        ("__DATA", "__thread_data") => (S_THREAD_LOCAL_REGULAR, 0),
        ("__DATA", "__thread_init") => (S_THREAD_LOCAL_INIT_FUNCTION_POINTERS, 0),
        _ => return None,
    })
}

/// Whether a linker-visible label starts an atom strictly after `from` and
/// at or before `to` (in either order), so that the two positions are in
/// different atoms. Positions are `(section, fragment, definition order)`;
/// see [`Atoms`].
pub(crate) fn atom_starts_between(
    interner: &crate::intern::Interner,
    symbols: &crate::symbol::SymbolTable,
    from: (SectionId, u32, u32),
    to: (SectionId, u32, u32),
) -> bool {
    let (a, b) = ((from.1, from.2), (to.1, to.2));
    let (lo, hi) = (a.min(b), a.max(b));
    symbols.iter().any(|(_, sym)| match sym.value {
        SymbolValue::Label { section, frag } => {
            let at = (frag, sym.def_order);
            section == from.0 && at > lo && at <= hi && !is_temporary(interner.get(sym.name))
        }
        _ => false,
    })
}

/// Splits a section's rsasm name back into its Mach-O pair. In Mach-O output
/// every section is named `SEGMENT,SECTION`, which is what the directives
/// store, so this only has to fail on a name from somewhere else.
pub(crate) fn split_name(name: &str) -> Option<(&str, &str)> {
    name.split_once(',')
}

// ---- writing ----------------------------------------------------------------

/// A section as the object will hold it.
struct Sec {
    id: SectionId,
    segment: String,
    section: String,
    flags: u32,
    reserved2: u32,
    /// Alignment as a power of two, which is how Mach-O stores it.
    align: u32,
    addr: u64,
    size: u64,
    zerofill: bool,
    bytes: Vec<u8>,
    relocs: Vec<Entry>,
}

/// One `relocation_info`.
struct Entry {
    address: u32,
    symbolnum: u32,
    pcrel: bool,
    length: u8,
    external: bool,
    ty: u8,
}

impl Entry {
    fn word(&self) -> u32 {
        (self.symbolnum & 0x00ff_ffff)
            | ((self.pcrel as u32) << 24)
            | ((self.length as u32) << 25)
            | ((self.external as u32) << 27)
            | ((self.ty as u32) << 28)
    }
}

/// A symbol as the object will hold it.
struct OutSym {
    name: String,
    n_type: u8,
    n_sect: u8,
    n_desc: u16,
    n_value: u64,
}

/// What a relocation entry names, once the atom rules have been applied.
#[derive(Copy, Clone)]
enum Named {
    /// A symbol of the assembler's.
    Symbol(SymbolId),
    /// A section, by its own index: a local relocation, with the target's
    /// address left in the field.
    Section(SectionId),
    /// The `ltmpN` label at the start of a section; see
    /// [`Cpu::labels_sections`].
    SectionLabel(SectionId),
}

/// Section addresses and indices, which everything past layout refers to.
struct Places {
    index: HashMap<SectionId, usize>,
    addr: HashMap<SectionId, u64>,
}

impl Places {
    /// A symbol's address in the object, which unlike its offset counts the
    /// sections before its own.
    fn symbol(&self, asm: &Assembler, id: SymbolId) -> i64 {
        let Some(offset) = asm.symbol_addr(id) else {
            return 0;
        };
        let section = match asm.symbols.get(id).value {
            SymbolValue::Label { section, .. } => Some(section),
            _ => asm.symbol_target_section(id).map(|(s, _)| s),
        };
        offset
            + section
                .and_then(|s| self.addr.get(&s))
                .copied()
                .unwrap_or(0) as i64
    }

    /// What a named symbol or section adds when the linker applies a
    /// relocation against it, as things stand in this object. A local
    /// relocation adds nothing: the linker moves the field by as much as the
    /// section moves instead.
    fn named(&self, asm: &Assembler, n: Named) -> i64 {
        match n {
            Named::Symbol(id) => self.symbol(asm, id),
            Named::Section(_) => 0,
            Named::SectionLabel(s) => self.addr.get(&s).copied().unwrap_or(0) as i64,
        }
    }
}

pub fn build(asm: &Assembler) -> Result<Vec<u8>, OutputError> {
    let cpu = Cpu::for_arch(asm.target()).ok_or_else(|| {
        OutputError::Unsupported(format!(
            "Mach-O output has no machine for `{}`; only x86-64 and arm64 have one",
            asm.target().name()
        ))
    })?;

    if !asm.options.relocatable {
        return Err(OutputError::Unsupported(
            "`--base` lays out a flat binary; a Mach-O object is placed by the linker".into(),
        ));
    }
    if asm.options.dialect == crate::lexer::Dialect::Nasm {
        return Err(OutputError::Unsupported(
            "NASM source is assembled to ELF objects and flat binaries; Mach-O output \
             reads GNU-style source"
                .into(),
        ));
    }

    let mut secs = collect_sections(asm)?;
    assign_addresses(&mut secs);
    let places = Places {
        index: secs.iter().enumerate().map(|(i, s)| (s.id, i)).collect(),
        addr: secs.iter().map(|s| (s.id, s.addr)).collect(),
    };

    // What each relocation names. An assembler-local label can end up in the
    // symbol table because a relocation has to name it, and whether one does
    // depends on the relocations before it, so this runs over all of them in
    // order before the table is built.
    let mut visible: HashSet<SymbolId> = HashSet::new();
    let named: Vec<(Named, Option<Named>)> = asm
        .relocs
        .iter()
        .map(|r| {
            let a = name_target(asm, cpu, r.symbol, r, &mut visible, true);
            let b = r
                .desc
                .subtrahend
                .map(|s| name_target(asm, cpu, Some(s), r, &mut visible, false));
            (a, b)
        })
        .collect();

    let table = collect_symbols(asm, cpu, &secs, &places, &visible);
    let (syms, index, counts) = (&table.syms, &table.index, table.counts);
    let number = |n: Named| -> Result<(u32, bool), OutputError> {
        Ok(match n {
            Named::Symbol(id) => match index.get(&id) {
                Some(&i) => (i, true),
                None => {
                    return Err(OutputError::Unsupported(format!(
                        "`{}` is relocated against, but has no symbol table entry",
                        asm.display_name(id)
                    )));
                }
            },
            Named::Section(s) => (places.index[&s] as u32 + 1, false),
            Named::SectionLabel(s) => (table.labels[&s], true),
        })
    };

    for (r, &(a, b)) in asm.relocs.iter().zip(&named) {
        let Some(&si) = places.index.get(&r.section) else {
            continue;
        };
        let ty = reloc_type(cpu, r).ok_or_else(|| {
            OutputError::Unsupported("a reference here has no Mach-O relocation".into())
        })?;
        let length = match r.desc.size {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => 3,
        };
        let address = r.offset as u32;
        let here = places.addr[&r.section] as i64 + r.offset as i64;
        let target = r.symbol.map_or(0, |t| places.symbol(asm, t)) + r.addend;
        let (symbolnum, external) = number(a)?;

        // The field holds what the linker's arithmetic leaves out: the value
        // less what the named symbols will contribute at their final
        // addresses.
        let mut entries = Vec::new();
        let mut field;
        match b {
            Some(b) => {
                // `A - B`: the `UNSIGNED` naming A and the `SUBTRACTOR` naming
                // B, both over the same field; reversed below, like the rest.
                let sub = places.symbol(asm, r.desc.subtrahend.expect("paired"));
                field = (target - sub) - places.named(asm, a) + places.named(asm, b);
                let (bnum, bext) = number(b)?;
                let subtractor = match cpu {
                    Cpu::X86_64 => x86_64_reloc::SUBTRACTOR,
                    Cpu::Arm64 => arm64_reloc::SUBTRACTOR,
                };
                entries.push(Entry {
                    address,
                    symbolnum,
                    pcrel: false,
                    length,
                    external,
                    ty,
                });
                entries.push(Entry {
                    address,
                    symbolnum: bnum,
                    pcrel: false,
                    length,
                    external: bext,
                    ty: subtractor,
                });
            }
            None => {
                field = target - places.named(asm, a);
                if r.desc.pcrel {
                    field += field_bias(r);
                    if !external {
                        // A local PC-relative field keeps the distance as
                        // the assembler measured it, from the end of the
                        // field.
                        field -= here + r.desc.size as i64;
                    }
                }
                entries.push(Entry {
                    address,
                    symbolnum,
                    pcrel: entry_pcrel(cpu, ty, r),
                    length,
                    external,
                    ty,
                });
                match addend_place(cpu, ty) {
                    AddendPlace::Field => {}
                    AddendPlace::Entry if field != 0 => {
                        if !(-0x0080_0000..0x0080_0000).contains(&field) {
                            return Err(OutputError::Unsupported(format!(
                                "an addend of {field} does not fit in an `ARM64_RELOC_ADDEND`"
                            )));
                        }
                        // The addend takes the entry's symbol number, as
                        // 24-bit two's complement.
                        entries.push(Entry {
                            address,
                            symbolnum: field as u32 & 0x00ff_ffff,
                            pcrel: false,
                            length,
                            external: false,
                            ty: arm64_reloc::ADDEND,
                        });
                        field = 0;
                    }
                    AddendPlace::Entry => {}
                    // llvm-mc keeps the constant of `sym@GOT`, less the offset
                    // of `.` in its section for `sym@GOT - .`.
                    AddendPlace::None if ty == arm64_reloc::POINTER_TO_GOT => {
                        field = r.addend - if r.desc.pcrel { r.offset as i64 } else { 0 };
                    }
                    AddendPlace::None if field != 0 => {
                        return Err(OutputError::Unsupported(format!(
                            "a GOT reference to `{}` lies {field} bytes into the symbol a \
                             Mach-O relocation can name, and a GOT slot has no offset",
                            r.symbol.map_or_else(String::new, |s| asm.display_name(s))
                        )));
                    }
                    AddendPlace::None => field = 0,
                }
            }
        }
        let (off, size) = (r.offset as usize, r.desc.size as usize);
        let in_field =
            addend_place(cpu, ty) == AddendPlace::Field || ty == arm64_reloc::POINTER_TO_GOT;
        if in_field && off + size <= secs[si].bytes.len() {
            crate::arch::Endian::Little.write(&mut secs[si].bytes[off..off + size], field as u64);
        }
        secs[si].relocs.append(&mut entries);
    }
    // llvm-mc writes each section's relocations last first, which also puts
    // the `SUBTRACTOR` of each pair and an `ARM64_RELOC_ADDEND` ahead of the
    // entry they modify.
    for s in &mut secs {
        s.relocs.reverse();
    }

    // Each region as its address and length.
    let regions: Vec<(u32, u16, u16)> = asm
        .macho
        .data_regions
        .iter()
        .filter_map(|d| {
            let start = places.symbol(asm, d.start);
            let end = places.symbol(asm, d.end?);
            Some((start as u32, (end - start) as u16, d.kind))
        })
        .collect();

    Ok(write(asm, cpu, &secs, syms, counts, &regions))
}

/// Every section of the object, in the order the source created them. Unlike
/// ELF, Mach-O keeps a section with nothing in it.
fn collect_sections(asm: &Assembler) -> Result<Vec<Sec>, OutputError> {
    let mut out = Vec::new();
    for s in &asm.sections {
        let name = asm.interner.get(s.name).to_string();
        let (segment, section) = split_name(&name).ok_or_else(|| {
            OutputError::Unsupported(if name.starts_with(".debug_") || name.ends_with("_frame") {
                // What `-g`, `.loc` and `.cfi_*` make, all in ELF's terms.
                "DWARF and call frame information are not written to Mach-O objects yet; \
                 assemble without `-g`, `.loc` and `.cfi_*`"
                    .to_string()
            } else {
                format!(
                    "`{name}` is not a Mach-O section; Mach-O sections are named \
                     `SEGMENT,SECTION`"
                )
            })
        })?;
        let info = asm.macho.sections.get(&s.id);
        let (ty, attrs) = match info {
            Some(i) => (i.ty, i.attrs),
            None => precreated(segment, section).unwrap_or((S_REGULAR, 0)),
        };
        // llvm-mc marks a section that any instruction was assembled into.
        let attrs = attrs
            | if s.has_instructions {
                S_ATTR_SOME_INSTRUCTIONS
            } else {
                0
            };
        let zerofill = matches!(ty, S_ZEROFILL | S_GB_ZEROFILL | S_THREAD_LOCAL_ZEROFILL);
        out.push(Sec {
            id: s.id,
            segment: segment.to_string(),
            section: section.to_string(),
            flags: ty | attrs,
            reserved2: info.map_or(0, |i| i.reserved2),
            align: s.align.max(1).trailing_zeros(),
            addr: 0,
            size: s.size,
            zerofill,
            bytes: if zerofill {
                Vec::new()
            } else {
                asm.section_bytes(s.id)
            },
            relocs: Vec::new(),
        });
    }
    Ok(out)
}

/// Gives every section an address in the object's single address space:
/// those with contents first, in order, then the zero-filled ones, which the
/// segment's file image cannot have in between.
fn assign_addresses(secs: &mut [Sec]) {
    let mut addr = 0u64;
    for zerofill in [false, true] {
        for s in secs.iter_mut().filter(|s| s.zerofill == zerofill) {
            addr = addr.next_multiple_of(1u64 << s.align);
            s.addr = addr;
            addr += s.size;
        }
    }
}

/// What a relocation against `target` names.
///
/// A linker-visible or undefined symbol names itself. An assembler-local
/// label names the atom it is in; with no atom, x86-64 names the section and
/// leaves the address in the field, and arm64 names the label llvm-mc puts at
/// the start of every section. A label in a literal section has to be named
/// itself where a position could not say which item it means, since the
/// linker takes such a section apart, and that puts it in the symbol table:
/// always on arm64, and on x86-64 where the field would hold more than the
/// label's address — llvm-mc's test is a non-zero addend, and once passed it
/// holds for every later relocation too, which is what `visible` carries.
/// Only the added symbol of a pair is subject to that test.
fn name_target(
    asm: &Assembler,
    cpu: Cpu,
    target: Option<SymbolId>,
    r: &Relocation,
    visible: &mut HashSet<SymbolId>,
    added: bool,
) -> Named {
    let Some(target) = target else {
        return Named::Section(r.section);
    };
    let sym = asm.symbols.get(target);
    if !is_temporary(asm.interner.get(sym.name)) || !sym.is_defined() || visible.contains(&target) {
        return Named::Symbol(target);
    }
    let SymbolValue::Label { section, .. } = sym.value else {
        return Named::Symbol(target);
    };
    if let Some(atom) = asm.macho.atoms.of(asm, target) {
        return Named::Symbol(atom);
    }
    let literal = !atomizable(asm, section);
    match cpu {
        Cpu::Arm64 if literal => {
            visible.insert(target);
            Named::Symbol(target)
        }
        Cpu::Arm64 => Named::SectionLabel(section),
        Cpu::X86_64 if literal && added && r.addend + field_bias(r) != 0 => {
            visible.insert(target);
            Named::Symbol(target)
        }
        Cpu::X86_64 => Named::Section(section),
    }
}

/// The symbol table: locals first, then defined externals, then undefined
/// symbols, each of the last two sorted by name as Mach-O requires.
fn collect_symbols(
    asm: &Assembler,
    cpu: Cpu,
    secs: &[Sec],
    places: &Places,
    visible: &HashSet<SymbolId>,
) -> Symtab {
    let mut locals: Vec<(Local, OutSym)> = Vec::new();
    let mut externals: Vec<(SymbolId, OutSym)> = Vec::new();
    let mut undefined: Vec<(SymbolId, OutSym)> = Vec::new();

    // The label arm64 objects carry at the start of each section is a local
    // symbol like any other, created when the section was: it goes among the
    // others where that happened, as llvm-mc has it.
    let mut labels = secs
        .iter()
        .enumerate()
        .filter(|_| cpu.labels_sections())
        .map(|(i, s)| {
            let mark = asm.macho.section_marks.get(&s.id).copied().unwrap_or(0);
            let label = OutSym {
                name: format!("ltmp{i}"),
                n_type: N_SECT,
                n_sect: i as u8 + 1,
                n_desc: 0,
                n_value: s.addr,
            };
            (mark, s.id, label)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .peekable();

    for (id, sym) in asm.symbols.iter() {
        while let Some((_, section, label)) = labels.next_if(|l| l.0 <= id.0) {
            locals.push((Local::SectionLabel(section), label));
        }
        let name = asm.interner.get(sym.name).to_string();
        if sym.ty == crate::symbol::SymType::Section {
            continue;
        }
        if is_temporary(&name) && !visible.contains(&id) {
            continue;
        }
        if !sym.is_defined() && !sym.used {
            continue;
        }
        let desc = asm.macho.desc(id);
        let global = sym.binding != Binding::Local;
        let section_of = |section: SectionId| places.index.get(&section).map(|&i| i as u8 + 1);
        let out = match &sym.value {
            SymbolValue::Undefined => OutSym {
                name,
                n_type: N_UNDF | N_EXT,
                n_sect: 0,
                n_desc: desc | weak_bits(sym.binding, false),
                n_value: 0,
            },
            SymbolValue::Common { size, align } => OutSym {
                name,
                n_type: N_UNDF | N_EXT,
                n_sect: 0,
                // A common symbol's alignment lives in its description.
                n_desc: desc | (((*align).max(1).trailing_zeros() as u16) << 8),
                n_value: *size,
            },
            SymbolValue::Label { section, .. } => {
                let Some(n_sect) = section_of(*section) else {
                    continue;
                };
                OutSym {
                    name,
                    n_type: N_SECT | ext_bits(global, sym.visibility),
                    n_sect,
                    n_desc: desc | weak_bits(sym.binding, true),
                    n_value: places.symbol(asm, id) as u64,
                }
            }
            SymbolValue::Expr(e) => match asm.symbol_target_section(id) {
                Some((section, _)) => {
                    let Some(n_sect) = section_of(section) else {
                        continue;
                    };
                    // An alias of a position inside a label's code, rather
                    // than of the label, is an alternate entry into it.
                    let inside = asm.eval_ref(*e).is_ok_and(|v| v.addend != 0);
                    OutSym {
                        name,
                        n_type: N_SECT | ext_bits(global, sym.visibility),
                        n_sect,
                        n_desc: desc
                            | weak_bits(sym.binding, true)
                            | if inside { N_ALT_ENTRY } else { 0 },
                        n_value: places.symbol(asm, id) as u64,
                    }
                }
                None => OutSym {
                    name,
                    n_type: N_ABS | ext_bits(global, sym.visibility),
                    n_sect: 0,
                    // llvm-mc marks a constant given by `.set` or `.equ`, but
                    // not one given by `=`, as not to be dead-stripped.
                    n_desc: desc
                        | if asm.macho.set_constants.contains(&id) {
                            N_NO_DEAD_STRIP
                        } else {
                            0
                        },
                    n_value: asm.symbol_number(id).unwrap_or(0) as u64,
                },
            },
        };
        if out.n_type & N_EXT == 0 {
            locals.push((Local::Symbol(id), out));
        } else if out.n_type & N_TYPE == N_UNDF {
            undefined.push((id, out));
        } else {
            externals.push((id, out));
        }
    }
    for (_, section, label) in labels {
        locals.push((Local::SectionLabel(section), label));
    }

    externals.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    undefined.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    let mut table = Symtab {
        counts: (
            locals.len() as u32,
            externals.len() as u32,
            undefined.len() as u32,
        ),
        ..Symtab::default()
    };
    for (key, s) in locals {
        let i = table.syms.len() as u32;
        match key {
            Local::Symbol(id) => table.index.insert(id, i),
            Local::SectionLabel(section) => table.labels.insert(section, i),
        };
        table.syms.push(s);
    }
    for (id, s) in externals.into_iter().chain(undefined) {
        table.index.insert(id, table.syms.len() as u32);
        table.syms.push(s);
    }
    table
}

/// A local symbol table entry: one of the assembler's symbols, or a section's
/// `ltmpN` label.
enum Local {
    Symbol(SymbolId),
    SectionLabel(SectionId),
}

/// The symbol table, and where in it each symbol and section label went.
#[derive(Default)]
struct Symtab {
    syms: Vec<OutSym>,
    index: HashMap<SymbolId, u32>,
    labels: HashMap<SectionId, u32>,
    /// How many locals, defined externals and undefined symbols, in order.
    counts: (u32, u32, u32),
}

/// `N_EXT`, plus `N_PEXT` for a `.private_extern` symbol, which rsasm records
/// as hidden visibility since the two mean the same thing.
fn ext_bits(global: bool, visibility: Visibility) -> u8 {
    if !global {
        return 0;
    }
    match visibility {
        Visibility::Hidden | Visibility::Internal => N_EXT | N_PEXT,
        _ => N_EXT,
    }
}

/// The description bits a `.weak` symbol gets: a definition is weak, a
/// reference to one elsewhere may be missing at run time.
fn weak_bits(binding: Binding, defined: bool) -> u16 {
    match (binding, defined) {
        (Binding::Weak, true) => N_WEAK_DEF,
        (Binding::Weak, false) => N_WEAK_REF,
        _ => 0,
    }
}

// ---- the file itself --------------------------------------------------------

/// A growable little-endian byte sink. Both Mach-O machines here are
/// little-endian, and the format's own fields follow the machine.
#[derive(Default)]
struct Buf {
    out: Vec<u8>,
}

impl Buf {
    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    /// A fixed-width name, NUL-padded and truncated as Mach-O stores them.
    fn name16(&mut self, s: &str) {
        let mut buf = [0u8; 16];
        let b = s.as_bytes();
        let n = b.len().min(16);
        buf[..n].copy_from_slice(&b[..n]);
        self.out.extend_from_slice(&buf);
    }
    fn pad_to(&mut self, align: u64) {
        while !(self.out.len() as u64).is_multiple_of(align) {
            self.out.push(0);
        }
    }
    fn len(&self) -> u64 {
        self.out.len() as u64
    }
}

fn write(
    asm: &Assembler,
    cpu: Cpu,
    secs: &[Sec],
    syms: &[OutSym],
    counts: (u32, u32, u32),
    regions: &[(u32, u16, u16)],
) -> Vec<u8> {
    let strings = string_table(syms);

    let build_version = asm.macho.build_version.is_some();
    // An object with no symbols has no symbol table, nor the commands that
    // would describe one.
    let has_symtab = !syms.is_empty();
    let data_in_code = !regions.is_empty();
    let ncmds = 1 + build_version as u32 + data_in_code as u32 + 2 * has_symtab as u32;
    let sizeofcmds = SEGMENT_COMMAND_64_SIZE
        + SECTION_64_SIZE * secs.len() as u32
        + if data_in_code {
            LINKEDIT_DATA_COMMAND_SIZE
        } else {
            0
        }
        + if has_symtab {
            SYMTAB_COMMAND_SIZE + DYSYMTAB_COMMAND_SIZE
        } else {
            0
        }
        + if build_version {
            BUILD_VERSION_COMMAND_SIZE
        } else {
            0
        };

    // The file is laid out before anything is written: every load command
    // holds an offset into what comes after it.
    let data_start = (HEADER_SIZE + sizeofcmds) as u64;
    let file_size: u64 = secs
        .iter()
        .filter(|s| !s.zerofill)
        .map(|s| s.addr + s.size)
        .max()
        .unwrap_or(0);
    let vm_size: u64 = secs.iter().map(|s| s.addr + s.size).max().unwrap_or(0);
    let reloc_start = (data_start + file_size).next_multiple_of(8);
    let mut off = reloc_start;
    let mut reloc_off = Vec::with_capacity(secs.len());
    for s in secs {
        reloc_off.push(off);
        off += s.relocs.len() as u64 * RELOCATION_SIZE;
    }
    let dataoff = off;
    let symoff = dataoff + regions.len() as u64 * DATA_IN_CODE_ENTRY_SIZE;
    let stroff = symoff + syms.len() as u64 * NLIST_64_SIZE;

    let mut b = Buf::default();
    let (cputype, cpusubtype) = cpu.header();
    b.u32(MH_MAGIC_64);
    b.u32(cputype);
    b.u32(cpusubtype);
    b.u32(MH_OBJECT);
    b.u32(ncmds);
    b.u32(sizeofcmds);
    b.u32(if asm.macho.subsections_via_symbols {
        MH_SUBSECTIONS_VIA_SYMBOLS
    } else {
        0
    });
    b.u32(0); // reserved

    // ---- LC_SEGMENT_64, with every section --------------------------------
    b.u32(LC_SEGMENT_64);
    b.u32(SEGMENT_COMMAND_64_SIZE + SECTION_64_SIZE * secs.len() as u32);
    b.name16(""); // an object's one segment has no name
    b.u64(0); // vmaddr
    b.u64(vm_size);
    b.u64(data_start);
    b.u64(file_size);
    b.u32(7); // maxprot: rwx
    b.u32(7); // initprot
    b.u32(secs.len() as u32);
    b.u32(0); // flags
    for (i, s) in secs.iter().enumerate() {
        b.name16(&s.section);
        b.name16(&s.segment);
        b.u64(s.addr);
        b.u64(s.size);
        b.u32(if s.zerofill {
            0
        } else {
            (data_start + s.addr) as u32
        });
        b.u32(s.align);
        b.u32(if s.relocs.is_empty() {
            0
        } else {
            reloc_off[i] as u32
        });
        b.u32(s.relocs.len() as u32);
        b.u32(s.flags);
        b.u32(0); // reserved1
        b.u32(s.reserved2);
        b.u32(0); // reserved3
    }

    if let Some(v) = asm.macho.build_version {
        b.u32(LC_BUILD_VERSION);
        b.u32(BUILD_VERSION_COMMAND_SIZE);
        b.u32(v.platform);
        b.u32(v.minos);
        b.u32(v.sdk);
        b.u32(0); // ntools
    }

    if data_in_code {
        b.u32(LC_DATA_IN_CODE);
        b.u32(LINKEDIT_DATA_COMMAND_SIZE);
        b.u32(dataoff as u32);
        b.u32((regions.len() as u64 * DATA_IN_CODE_ENTRY_SIZE) as u32);
    }

    if has_symtab {
        b.u32(LC_SYMTAB);
        b.u32(SYMTAB_COMMAND_SIZE);
        b.u32(symoff as u32);
        b.u32(syms.len() as u32);
        b.u32(stroff as u32);
        b.u32(strings.bytes.len() as u32);

        b.u32(LC_DYSYMTAB);
        b.u32(DYSYMTAB_COMMAND_SIZE);
        let (nlocal, nextdef, nundef) = counts;
        b.u32(0); // ilocalsym
        b.u32(nlocal);
        b.u32(nlocal); // iextdefsym
        b.u32(nextdef);
        b.u32(nlocal + nextdef); // iundefsym
        b.u32(nundef);
        for _ in 0..12 {
            b.u32(0); // the tables an assembler never writes
        }
    }

    debug_assert_eq!(b.len(), data_start, "load commands must fill the header");

    // ---- section contents -------------------------------------------------
    for s in secs.iter().filter(|s| !s.zerofill) {
        while b.len() < data_start + s.addr {
            b.u8(0);
        }
        b.out.extend_from_slice(&s.bytes);
    }
    b.pad_to(8);

    for s in secs {
        for r in &s.relocs {
            b.u32(r.address);
            b.u32(r.word());
        }
    }

    for &(offset, length, kind) in regions {
        b.u32(offset);
        b.u16(length);
        b.u16(kind);
    }

    for s in syms {
        b.u32(strings.offset(&s.name));
        b.u8(s.n_type);
        b.u8(s.n_sect);
        b.u16(s.n_desc);
        b.u64(s.n_value);
    }
    if has_symtab {
        b.out.extend_from_slice(&strings.bytes);
    }
    b.out
}

/// The string table, built the way llvm-mc builds it: the names sorted so
/// that one which is a suffix of another follows it, and shares its tail.
struct Strings {
    bytes: Vec<u8>,
    offsets: HashMap<String, u32>,
}

impl Strings {
    fn offset(&self, name: &str) -> u32 {
        self.offsets.get(name).copied().unwrap_or(0)
    }
}

fn string_table(syms: &[OutSym]) -> Strings {
    let mut names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    // Descending by the reversed name, which puts every suffix right after
    // the name it can share a tail with.
    names.sort_by(|a, b| {
        let (ra, rb): (Vec<u8>, Vec<u8>) = (a.bytes().rev().collect(), b.bytes().rev().collect());
        rb.cmp(&ra)
    });

    let mut bytes = vec![0u8];
    let mut offsets = HashMap::new();
    let mut previous: Option<(&str, u32)> = None;
    for name in names {
        if name.is_empty() {
            offsets.insert(String::new(), 0);
            continue;
        }
        if let Some((prev, at)) = previous
            && prev.ends_with(name)
        {
            offsets.insert(name.to_string(), at + (prev.len() - name.len()) as u32);
            continue;
        }
        let at = bytes.len() as u32;
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        offsets.insert(name.to_string(), at);
        previous = Some((name, at));
    }
    // Padded to a word, as the table ends the file.
    while !bytes.len().is_multiple_of(8) {
        bytes.push(0);
    }
    Strings { bytes, offsets }
}

//! Pure-Rust replacement for `as` + `ld -r`: assemble `asm!`/`global_asm!`
//! text with rsasm and splice the resulting sections, symbols and relocations
//! into the Cranelift object. The assembler emits the same binary format as
//! the destination, so flags and relocation types pass through verbatim.

use object::write::{self, Relocation, SectionId, SymbolId, SymbolSection};
use object::{
    BinaryFormat, ObjectSection as _, ObjectSymbol as _, RelocationTarget, SectionIndex,
    SectionKind, SymbolIndex, SymbolKind,
};
use rsasm::assembler::{Assembler, Options};
use rsasm::lexer::Dialect;
use rustc_hash::FxHashMap;

// r[impl native.asm.inproc]
pub fn assemble_into(out: &mut write::Object<'static>, asm: &str, x86: bool) {
    let macho = out.format() == BinaryFormat::MachO;
    let coff = out.format() == BinaryFormat::Coff;
    let arch = rsasm::arch::lookup(if x86 { "x86-64" } else { "aarch64" }).unwrap();
    let opts = Options::new()
        .with_dialect(Dialect::Gas)
        .with_format(if macho {
            rsasm::output::Format::MachO
        } else if coff {
            rsasm::output::Format::Coff
        } else {
            rsasm::output::Format::Elf
        });
    let mut a = Assembler::new(arch, opts);
    a.assemble_str("<asm>", asm);
    if !a.finish() || a.diags().has_errors() {
        panic!(
            "assembling asm!/global_asm! failed:\n{}\n--- source ---\n{asm}",
            a.diags().render(a.source_map(), false)
        );
    }
    let bytes = if macho {
        rsasm::output::macho::build(&a).unwrap()
    } else if coff {
        rsasm::output::coff::build(&a).unwrap()
    } else {
        rsasm::output::elf::build(&a).unwrap()
    };
    // `PLIRON_ASMOBJ_DUMP=<dir>`: keep each assembled object (before it is
    // spliced) for inspection with objdump.
    if let Some(dir) = crate::env_get("PLIRON_ASMOBJ_DUMP") {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::write(format!("{dir}/asm-{}-{n}.o", std::process::id()), &*bytes);
    }
    if macho {
        let f = object::read::macho::MachOFile64::<object::Endianness>::parse(&*bytes).unwrap();
        splice(&f, out, x86);
    } else if coff {
        let f = object::read::coff::CoffFile::<&[u8], object::pe::ImageFileHeader>::parse(&*bytes)
            .unwrap();
        splice(&f, out, x86);
    } else {
        let f = object::read::elf::ElfFile64::<object::Endianness>::parse(&*bytes).unwrap();
        splice(&f, out, x86);
    }
}

// r[impl native.asm.splice]
fn splice<'d, O>(f: &O, out: &mut write::Object<'static>, x86: bool)
where
    O: object::Object<'d>,
{
    let mut secs: FxHashMap<SectionIndex, SectionId> = FxHashMap::default();
    for s in f.sections() {
        let name = s.name_bytes().unwrap();
        let kind = if name == b".eh_frame" && x86 {
            SectionKind::Elf(object::elf::SHT_X86_64_UNWIND)
        } else {
            s.kind()
        };
        // `object` calls Mach-O `__DATA,__mod_init_func` Unknown, but the
        // ctor pointer it holds is the reason the asm exists — keep it by
        // name. Its S_MOD_INIT_FUNC_POINTERS flags pass through below.
        // COFF `$`-sections (.tls$AAA/.CRT$XLA link-range markers, COMDAT
        // groups) classify as Other/Unknown — keep them too.
        let coff_named = out.format() == BinaryFormat::Coff && name.contains(&b'$');
        let keep = (matches!(
            kind,
            SectionKind::Text
                | SectionKind::Data
                | SectionKind::ReadOnlyData
                | SectionKind::ReadOnlyDataWithRel
                | SectionKind::ReadOnlyString
                | SectionKind::UninitializedData
                | SectionKind::Tls
                | SectionKind::UninitializedTls
                | SectionKind::Elf(_)
        ) || coff_named
            || (out.format() == BinaryFormat::MachO && name == b"__mod_init_func"))
            && (s.size() > 0 || coff_named);
        if !keep {
            continue;
        }
        let seg = s
            .segment_name_bytes()
            .unwrap_or_default()
            .unwrap_or_default();
        let id = out.add_section(seg.to_vec(), name.to_vec(), kind);
        let sec = out.section_mut(id);
        if matches!(
            kind,
            SectionKind::UninitializedData | SectionKind::UninitializedTls
        ) {
            sec.append_bss(s.size(), s.align());
        } else {
            sec.set_data(s.data().unwrap().to_vec(), s.align());
        }
        sec.flags = s.flags();
        secs.insert(s.index(), id);
    }

    let mut syms: FxHashMap<SymbolIndex, SymbolId> = FxHashMap::default();
    for sym in f.symbols() {
        match sym.kind() {
            // Mach-O has no section symbols: what reads as one is an asm
            // section label (`ltmpN`). A relocation through the writer's
            // section symbol becomes non-external, whose field must hold an
            // absolute address — but the asm field holds `target - label`,
            // so keep the label as a real local symbol (external reloc).
            SymbolKind::Section if out.format() != BinaryFormat::MachO => {
                if let Some(&sid) = sym.section_index().and_then(|i| secs.get(&i)) {
                    syms.insert(sym.index(), out.section_symbol(sid));
                }
                continue;
            }
            SymbolKind::File => continue,
            _ => {}
        }
        let raw = sym.name_bytes().unwrap();
        if raw.is_empty() {
            continue;
        }
        // Mach-O asm names are raw (`_foo` is C's `foo`), while the writer
        // takes unprefixed names and adds `_` itself. Our own stubs
        // (`__pliron_asm_*`) are declared under their asm spelling, so an
        // exact match wins; otherwise drop one `_`. A raw name without the
        // prefix keeps its spelling (fixed up after `add_symbol`).
        let macho = out.format() == BinaryFormat::MachO;
        let mut keep_raw = false;
        let name: &[u8] = if !macho || sym.is_local() || out.symbol_id(raw).is_some() {
            raw
        } else if let Some(stripped) = raw.strip_prefix(b"_") {
            stripped
        } else {
            keep_raw = true;
            raw
        };
        // Read gives the symbol's address; write wants an offset within its
        // section. Mach-O object sections have addresses, ELF ones are zero.
        let (section, value) = match sym.section() {
            object::SymbolSection::Section(i) => match secs.get(&i) {
                Some(&s) => (
                    SymbolSection::Section(s),
                    sym.address() - f.section_by_index(i).unwrap().address(),
                ),
                None => continue,
            },
            object::SymbolSection::Absolute => (SymbolSection::Absolute, sym.address()),
            _ => (SymbolSection::Undefined, sym.address()),
        };
        let flags = match sym.flags() {
            object::SymbolFlags::Elf { st_info, st_other } => {
                object::SymbolFlags::Elf { st_info, st_other }
            }
            object::SymbolFlags::MachO { n_desc } => object::SymbolFlags::MachO { n_desc },
            _ => object::SymbolFlags::None,
        };
        let new = write::Symbol {
            name: name.to_vec(),
            size: sym.size(),
            kind: match sym.kind() {
                // The Mach-O writer only takes Text/Data/Tls symbols.
                SymbolKind::Section => match sym.section_index().map(|i| f.section_by_index(i)) {
                    Some(Ok(s)) if s.kind() == SectionKind::Text => SymbolKind::Text,
                    _ => SymbolKind::Data,
                },
                k => k,
            },
            scope: sym.scope(),
            weak: sym.is_weak(),
            section,
            value,
            flags,
        };
        if !sym.is_local() {
            if let Some(id) = out.symbol_id(name) {
                let s = out.symbol_mut(id);
                if matches!(s.section, SymbolSection::Undefined)
                    && !matches!(section, SymbolSection::Undefined)
                {
                    // Keep the existing spelling: `name` may be the raw asm
                    // name with its Mach-O `_` dropped.
                    let keep = std::mem::take(&mut s.name);
                    *s = new;
                    s.name = keep;
                }
                syms.insert(sym.index(), id);
                continue;
            }
        }
        let id = out.add_symbol(new);
        let n = &mut out.symbol_mut(id).name;
        if keep_raw {
            if n.len() == raw.len() + 1 && n.ends_with(raw) {
                n.remove(0);
            }
        } else if macho && n.as_slice() == name && name.len() + 1 == raw.len() {
            // `add_symbol` only prefixes Text/Data/Tls symbols: an asm
            // reference to an undefined C symbol (`bl _malloc`, kind
            // Unknown) kept the stripped `malloc` and failed to link
            // (tests/ui/allocator/regression-abort-on-free-issue-150898).
            *n = raw.to_vec();
        }
        syms.insert(sym.index(), id);
    }

    for s in f.sections() {
        let Some(&to) = secs.get(&s.index()) else {
            continue;
        };
        for (offset, r) in s.relocations() {
            let symbol = match r.target() {
                RelocationTarget::Symbol(i) => syms[&i],
                RelocationTarget::Section(i) => out.section_symbol(secs[&i]),
                t => panic!("unsupported relocation target {t:?} in asm object"),
            };
            // A Mach-O `A - B` field is a SUBTRACTOR+UNSIGNED pair, which
            // the reader folds into one relocation with `subtractor() = B`;
            // dropping B leaves the field read as an absolute address
            // (rsasm's `.cfi_*` FDEs: "address not in any section").
            let sub = r.subtractor().map(|i| syms[&i]);
            out.add_relocation_with_subtractor(
                to,
                Relocation {
                    offset,
                    symbol,
                    addend: r.addend(),
                    flags: r.flags(),
                },
                sub,
            )
            .unwrap();
        }
    }
}

/// Section that carries the backend's LTO payload (`src/lto.rs`) in a
/// native object, as `(segment, section)`. Like LLVM's embedded bitcode it
/// is for rustc's `-Clto` step only and never reaches a linked image:
/// Mach-O linkers drop the `__LLVM` segment (ld64 and ld-prime, `-r`
/// included), ELF linkers drop `SHF_EXCLUDE` sections, COFF ones drop
/// `IMAGE_SCN_LNK_REMOVE` sections.
pub fn lto_section_name(format: BinaryFormat) -> (&'static [u8], &'static [u8]) {
    match format {
        BinaryFormat::MachO => (b"__LLVM", b"__pliron_lto"),
        _ => (b"", b".pliron.lto"),
    }
}

/// Embed `payload` in `out` (see `lto_section_name`).
// r[impl native.lto.payload]
pub fn add_lto_section(out: &mut write::Object<'static>, payload: &[u8]) {
    let format = out.format();
    let (seg, name) = lto_section_name(format);
    let id = out.add_section(seg.to_vec(), name.to_vec(), SectionKind::Other);
    out.append_section_data(id, payload, 1);
    match format {
        BinaryFormat::Elf => {
            out.section_mut(id).flags = object::SectionFlags::Elf {
                sh_flags: object::elf::SHF_EXCLUDE as u64,
            }
        }
        BinaryFormat::Coff => {
            out.section_mut(id).flags = object::SectionFlags::Coff {
                characteristics: object::pe::IMAGE_SCN_LNK_REMOVE
                    | object::pe::IMAGE_SCN_LNK_INFO
                    | object::pe::IMAGE_SCN_ALIGN_1BYTES,
            }
        }
        _ => {}
    }
}

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
use rustc_data_structures::fx::FxHashMap;

pub fn assemble_into(out: &mut write::Object<'static>, asm: &str, x86: bool) {
    let macho = out.format() == BinaryFormat::MachO;
    let arch = rsasm::arch::lookup(if x86 { "x86-64" } else { "aarch64" }).unwrap();
    let opts = Options::new()
        .with_dialect(Dialect::Gas)
        .with_format(if macho {
            rsasm::output::Format::MachO
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
    } else {
        rsasm::output::elf::build(&a).unwrap()
    };
    if macho {
        let f = object::read::macho::MachOFile64::<object::Endianness>::parse(&*bytes).unwrap();
        splice(&f, out, x86);
    } else {
        let f = object::read::elf::ElfFile64::<object::Endianness>::parse(&*bytes).unwrap();
        splice(&f, out, x86);
    }
}

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
        let keep = matches!(
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
        ) && s.size() > 0;
        if !keep {
            continue;
        }
        let seg = s.segment_name_bytes().unwrap_or_default().unwrap_or_default();
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
            SymbolKind::Section => {
                if let Some(&sid) = sym.section_index().and_then(|i| secs.get(&i)) {
                    syms.insert(sym.index(), out.section_symbol(sid));
                }
                continue;
            }
            SymbolKind::File => continue,
            _ => {}
        }
        let name = sym.name_bytes().unwrap();
        if name.is_empty() {
            continue;
        }
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
            kind: sym.kind(),
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
                    *s = new;
                }
                syms.insert(sym.index(), id);
                continue;
            }
        }
        syms.insert(sym.index(), out.add_symbol(new));
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
            out.add_relocation(
                to,
                Relocation {
                    offset,
                    symbol,
                    addend: r.addend(),
                    flags: r.flags(),
                },
            )
            .unwrap();
        }
    }
}

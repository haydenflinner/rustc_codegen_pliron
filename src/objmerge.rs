//! Pure-Rust replacement for `as` + `ld -r`: assemble `asm!`/`global_asm!`
//! text with rsasm and splice the resulting ELF sections, symbols and
//! relocations into the Cranelift object.

use object::read::elf::ElfFile64;
use object::write::{self, Relocation, SectionId, SymbolId, SymbolSection};
use object::{
    Object as _, ObjectSection as _, ObjectSymbol as _, RelocationFlags, RelocationTarget,
    SectionIndex, SectionKind, SymbolIndex, SymbolKind,
};
use rsasm::assembler::{Assembler, Options};
use rsasm::lexer::Dialect;
use rustc_data_structures::fx::FxHashMap;

pub fn assemble_into(out: &mut write::Object<'static>, asm: &str, x86: bool) {
    let arch = rsasm::arch::lookup(if x86 { "x86-64" } else { "aarch64" }).unwrap();
    let mut a = Assembler::new(arch, Options::new().with_dialect(Dialect::Gas));
    a.assemble_str("<asm>", asm);
    if !a.finish() || a.diags().has_errors() {
        panic!(
            "assembling asm!/global_asm! failed:\n{}\n--- source ---\n{asm}",
            a.diags().render(a.source_map(), false)
        );
    }
    let bytes = rsasm::output::elf::build(&a).unwrap();
    let f = ElfFile64::<object::Endianness>::parse(&*bytes).unwrap();

    let mut secs: FxHashMap<SectionIndex, SectionId> = FxHashMap::default();
    for s in f.sections() {
        let name = s.name_bytes().unwrap();
        let kind = if name == b".eh_frame" {
            if x86 {
                SectionKind::Elf(object::elf::SHT_X86_64_UNWIND)
            } else {
                SectionKind::ReadOnlyData
            }
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
        let id = out.add_section(Vec::new(), name.to_vec(), kind);
        let sec = out.section_mut(id);
        if matches!(
            kind,
            SectionKind::UninitializedData | SectionKind::UninitializedTls
        ) {
            sec.append_bss(s.size(), s.align());
        } else {
            sec.set_data(s.data().unwrap().to_vec(), s.align());
        }
        if let object::SectionFlags::Elf { sh_flags } = s.flags() {
            sec.flags = object::SectionFlags::Elf { sh_flags };
        }
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
        let section = match sym.section() {
            object::SymbolSection::Section(i) => match secs.get(&i) {
                Some(&s) => SymbolSection::Section(s),
                None => continue,
            },
            object::SymbolSection::Absolute => SymbolSection::Absolute,
            _ => SymbolSection::Undefined,
        };
        let flags = match sym.flags() {
            object::SymbolFlags::Elf { st_info, st_other } => {
                object::SymbolFlags::Elf { st_info, st_other }
            }
            _ => object::SymbolFlags::None,
        };
        let new = write::Symbol {
            name: name.to_vec(),
            value: sym.address(),
            size: sym.size(),
            kind: sym.kind(),
            scope: sym.scope(),
            weak: sym.is_weak(),
            section,
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
            let RelocationFlags::Elf { r_type } = r.flags() else {
                unreachable!()
            };
            out.add_relocation(
                to,
                Relocation {
                    offset,
                    symbol,
                    addend: r.addend(),
                    flags: RelocationFlags::Elf { r_type },
                },
            )
            .unwrap();
        }
    }
}

//! Object and flat-binary output.

#![cfg(feature = "x86")]

mod common;
use common::*;

use rsasm::output;

fn elf(src: &str) -> Vec<u8> {
    elf_for("x86-64", src)
}

fn elf_for(arch: &str, src: &str) -> Vec<u8> {
    let asm = assemble_for(arch, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    output::elf::build(&asm).expect("ELF output")
}

fn u16at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn u32at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn u64at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Section headers as (name, type, flags, size).
fn sections_of(b: &[u8]) -> Vec<(String, u32, u64, u64)> {
    let shoff = u64at(b, 0x28) as usize;
    let shnum = u16at(b, 0x3c) as usize;
    let shstrndx = u16at(b, 0x3e) as usize;
    let strtab_off = u64at(b, shoff + shstrndx * 64 + 0x18) as usize;
    (0..shnum)
        .map(|i| {
            let sh = shoff + i * 64;
            let name_off = strtab_off + u32at(b, sh) as usize;
            let end = b[name_off..].iter().position(|&c| c == 0).unwrap() + name_off;
            (
                String::from_utf8_lossy(&b[name_off..end]).into_owned(),
                u32at(b, sh + 4),
                u64at(b, sh + 8),
                u64at(b, sh + 0x20),
            )
        })
        .collect()
}

#[test]
fn elf_header_is_well_formed() {
    let b = elf("nop\n");
    assert_eq!(&b[..4], b"\x7fELF");
    assert_eq!(b[4], 2, "ELFCLASS64");
    assert_eq!(b[5], 1, "little endian");
    assert_eq!(b[6], 1, "version");
    assert_eq!(u16at(&b, 0x10), 1, "ET_REL");
    assert_eq!(u16at(&b, 0x12), 62, "EM_X86_64");
    assert_eq!(u16at(&b, 0x34), 64, "e_ehsize");
    assert_eq!(u16at(&b, 0x3a), 64, "e_shentsize");
    // Section headers must lie inside the file.
    let shoff = u64at(&b, 0x28) as usize;
    let shnum = u16at(&b, 0x3c) as usize;
    assert_eq!(shoff + shnum * 64, b.len());
}

#[test]
fn sections_carry_the_right_types_and_flags() {
    let b =
        elf(".text\nnop\n.data\n.byte 1\n.bss\n.space 8\n.section .note.x,\"\",@note\n.byte 0\n");
    let secs = sections_of(&b);
    let find = |n: &str| {
        secs.iter()
            .find(|s| s.0 == n)
            .unwrap_or_else(|| panic!("no {n}"))
    };

    // SHT_PROGBITS = 1, SHT_NOBITS = 8, SHT_NOTE = 7.
    // SHF_ALLOC = 2, SHF_EXECINSTR = 4, SHF_WRITE = 1.
    assert_eq!(find(".text").1, 1);
    assert_eq!(find(".text").2, 2 | 4);
    assert_eq!(find(".data").2, 2 | 1);
    assert_eq!(find(".bss").1, 8);
    assert_eq!(find(".bss").3, 8, ".bss has a size but no file content");
    assert_eq!(find(".note.x").1, 7);
    assert!(secs.iter().any(|s| s.0 == ".symtab"));
    assert!(secs.iter().any(|s| s.0 == ".strtab"));
    assert!(secs.iter().any(|s| s.0 == ".shstrtab"));
    // The null header comes first and stays empty.
    assert_eq!(secs[0].0, "");
    assert_eq!(secs[0].1, 0);
}

#[test]
fn a_difference_from_a_label_in_the_same_section_is_pc_relative() {
    // GNU as: `x + 0`, `x + 4`, `x + 4` and `.text + a`, all R_X86_64_PC32
    // (type 2), with zeroed fields.
    let asm = assemble(
        ".data\n.long 0\n.long x-.\nL: .long x-.+4\n.long x-L\n.long y-L+2\n\
         .text\ny: .long 0\n",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let got: Vec<(u64, u32, String, i64)> = asm
        .relocs
        .iter()
        .map(|r| {
            (
                r.offset,
                r.kind,
                asm.display_name(r.symbol.unwrap()),
                r.addend,
            )
        })
        .collect();
    let want: Vec<(u64, u32, String, i64)> = vec![
        (4, 2, "x".into(), 0),
        (8, 2, "x".into(), 4),
        (12, 2, "x".into(), 4),
        (16, 2, ".text".into(), 10),
    ];
    assert_eq!(got, want);
    assert_eq!(hex(&section(&asm, ".data")), "00 ".repeat(19) + "00");
    // A label in another section still has no single-relocation form.
    assert!(errors(".data\nL: .long 0\n.text\n.long x-L\n").contains("different sections"));
}

#[test]
fn a_branch_to_a_plain_address_is_relocated_against_no_symbol() {
    // GNU as: both fields zero, relocated against *ABS* with addends 0xffc
    // and 0x1ffc. (GNU as picks R_X86_64_PC32; rsasm's R_X86_64_PLT32 links
    // identically for a symbol that cannot be preempted.)
    let asm = assemble("nop\ncall 0x1000\njmp F\nF = 0x2000\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(
        hex(&text("nop\ncall 0x1000\njmp F\nF = 0x2000\n")),
        "90 e8 00 00 00 00 e9 00 00 00 00"
    );
    let got: Vec<(u64, Option<_>, i64)> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.symbol, r.addend))
        .collect();
    assert_eq!(got, vec![(2, None, 0xffc), (7, None, 0x1ffc)]);
    // A flat image knows where the field is, and resolves it.
    let flat = assemble_flat("nop\ncall 0x1000\n", 0);
    assert!(flat.relocs.is_empty());
}

#[test]
fn relocations_are_emitted_for_unresolved_references() {
    let asm = assemble("call printf@PLT\nmovq gvar(%rip), %rax\n.quad gvar\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(asm.relocs.len(), 3);
    // R_X86_64_PLT32 = 4, PC32 = 2, 64 = 1.
    let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, vec![4, 2, 1]);
    // A PC-relative field is biased by its own width.
    assert_eq!(asm.relocs[0].addend, -4);
    assert_eq!(asm.relocs[1].addend, -4);
    assert_eq!(asm.relocs[2].addend, 0);
    // The object must actually contain a .rela.text.
    let b = output::elf::build(&asm).unwrap();
    assert!(
        sections_of(&b)
            .iter()
            .any(|s| s.0 == ".rela.text" && s.1 == 4)
    );
}

#[test]
fn local_references_relocate_against_their_section() {
    // A reference to a local label in another section goes through that
    // section's symbol, with the label's offset folded into the addend.
    let asm = assemble(".data\n.byte 0\nlocal: .byte 0\n.text\n.quad local\n");
    assert_eq!(asm.relocs.len(), 1);
    let r = &asm.relocs[0];
    assert_eq!(r.addend, 1, "the label's offset becomes the addend");
    assert_eq!(
        rsasm::symbol::SymType::Section,
        asm.symbols.get(r.symbol.unwrap()).ty
    );
}

#[test]
fn same_section_references_need_no_relocation() {
    let asm = assemble("target: nop\njmp target\ncall target\n");
    assert!(asm.relocs.is_empty(), "{:?}", asm.relocs);
}

#[test]
fn flat_binary_places_sections_at_their_addresses() {
    let asm = assemble_flat(".text\nnop\n.data\n.byte 0xaa\n", 0x1000);
    let out = output::raw::build(&asm).unwrap();
    // .text is one byte, .data follows it.
    assert_eq!(out, vec![0x90, 0xaa]);

    // A flat image resolves absolute references rather than deferring them.
    let asm = assemble_flat(".text\nfoo:\n.quad foo\n", 0x1000);
    let out = output::raw::build(&asm).unwrap();
    assert_eq!(u64::from_le_bytes(out[..8].try_into().unwrap()), 0x1000);
    assert!(asm.relocs.is_empty());
}

#[test]
fn flat_binary_resolves_references_across_sections() {
    // A PC-relative reference into another section cannot be resolved while
    // the sections are still floating, but a flat image has real addresses,
    // so it must not be deferred to a relocation that nobody will apply.
    let asm = assemble_flat(
        ".text\nleaq msg(%rip), %rsi\n.section .rodata\nmsg: .ascii \"hi\"\n",
        0,
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert!(asm.relocs.is_empty(), "{:?}", asm.relocs);
    let out = output::raw::build(&asm).unwrap();
    // `lea` is 7 bytes, so the next instruction is at 7 and .rodata at 7 too.
    assert_eq!(&out[..7], &[0x48, 0x8d, 0x35, 0x00, 0x00, 0x00, 0x00]);
    assert_eq!(&out[7..], b"hi");
}

#[test]
fn flat_binary_reports_undefined_symbols() {
    let arch = rsasm::arch::lookup("x86-64").unwrap();
    let options = rsasm::assembler::Options::new().with_relocatable(false);
    let mut asm = rsasm::assembler::Assembler::new(arch, options);
    asm.assemble_str("t.s", ".quad nosuch\n");
    asm.finish();
    let e = asm.diags.render(&asm.sm, false);
    assert!(e.contains("undefined symbol `nosuch`"), "{e}");
}

#[test]
fn empty_input_produces_a_valid_object() {
    let b = elf("");
    assert_eq!(&b[..4], b"\x7fELF");
    // Even with nothing in it, the tables are present and consistent.
    let secs = sections_of(&b);
    assert!(secs.iter().any(|s| s.0 == ".symtab"));
}

// ---- ELF32 ---------------------------------------------------------------
//
// The 32-bit class is exercised through the i386 backend, which is the only
// 32-bit target the crate is guaranteed to have.

#[test]
fn elf32_header_matches_the_class() {
    let b = elf_for("i386", "nop\n");
    assert_eq!(&b[..4], b"\x7fELF");
    assert_eq!(b[4], 1, "ELFCLASS32");
    assert_eq!(u16::from_le_bytes([b[0x10], b[0x11]]), 1, "ET_REL");
    assert_eq!(u16::from_le_bytes([b[0x12], b[0x13]]), 3, "EM_386");
    // ELF32 headers are smaller, and e_shoff is 32 bits at a different offset.
    assert_eq!(u16::from_le_bytes([b[0x28], b[0x29]]), 52, "e_ehsize");
    assert_eq!(u16::from_le_bytes([b[0x2e], b[0x2f]]), 40, "e_shentsize");
    let shoff = u32at(&b, 0x20) as usize;
    let shnum = u16::from_le_bytes([b[0x30], b[0x31]]) as usize;
    assert_eq!(shoff + shnum * 40, b.len(), "section headers end the file");
}

#[test]
fn elf32_symbols_use_the_32_bit_field_order() {
    // Elf32_Sym puts value and size before info/other/shndx, which Elf64_Sym
    // does not. Getting that wrong still produces a parseable file, so check
    // it directly rather than trusting the shape.
    let b = elf_for("i386", ".globl f\n.type f, @function\nf: nop\n.size f, 1\n");
    let shoff = u32at(&b, 0x20) as usize;
    let shnum = u16::from_le_bytes([b[0x30], b[0x31]]) as usize;
    let shstrndx = u16::from_le_bytes([b[0x32], b[0x33]]) as usize;
    let strtab_off = u32at(&b, shoff + shstrndx * 40 + 0x10) as usize;

    let mut symtab = None;
    for i in 0..shnum {
        let sh = shoff + i * 40;
        let name_off = strtab_off + u32at(&b, sh) as usize;
        let end = b[name_off..].iter().position(|&c| c == 0).unwrap() + name_off;
        if &b[name_off..end] == b".symtab" {
            symtab = Some((u32at(&b, sh + 0x10) as usize, u32at(&b, sh + 0x14) as usize));
        }
    }
    let (off, size) = symtab.expect("no .symtab");
    assert_eq!(size % 16, 0, "Elf32_Sym is 16 bytes");
    // The last symbol is the global `f`: a one-byte function at offset 0.
    let last = off + size - 16;
    assert_eq!(u32at(&b, last + 4), 0, "st_value");
    assert_eq!(u32at(&b, last + 8), 1, "st_size");
    assert_eq!(b[last + 12], (1 << 4) | 2, "STB_GLOBAL | STT_FUNC");
}

#[test]
fn i386_relocations_use_rel_and_carry_the_addend_in_the_field() {
    // i386 is a REL psABI: there is no addend field in the relocation, so the
    // addend has to reach the linker inside the instruction itself.
    let asm = assemble_for("i386", ".long sym + 0x1234\n");
    assert_eq!(asm.relocs.len(), 1);
    assert_eq!(asm.relocs[0].addend, 0x1234);
    assert_eq!(section(&asm, ".text"), 0x1234u32.to_le_bytes());

    let b = elf_for("i386", ".long sym + 0x1234\n");
    let secs = sections_of32(&b);
    // SHT_REL = 9, and the entries are 8 bytes rather than 12.
    let rel = secs
        .iter()
        .find(|s| s.0 == ".rel.text")
        .expect("no .rel.text");
    assert_eq!(rel.1, 9, "SHT_REL");
    assert_eq!(rel.3, 8, "one 8-byte Elf32_Rel");
}

#[test]
fn x86_64_still_uses_rela_with_a_clean_field() {
    // The counterpart: a RELA target leaves the field alone, which is what
    // keeps rsasm byte-identical to GNU as here.
    let asm = assemble("call printf@PLT\n");
    assert_eq!(asm.relocs[0].addend, -4);
    assert_eq!(&section(&asm, ".text")[1..5], &[0, 0, 0, 0]);
}

#[test]
// Needs a target narrower than 32 bits, which only the retro backend has.
#[cfg(feature = "retro")]
fn a_target_narrower_than_elf_allows_is_refused_clearly() {
    let asm = assemble_for("z80", "");
    let e = output::elf::build(&asm).expect_err("z80 has no ELF class");
    assert!(e.to_string().contains("-f bin"), "{e}");
}

/// Section headers as (name, type, flags, size), for the 32-bit layout.
fn sections_of32(b: &[u8]) -> Vec<(String, u32, u32, u32)> {
    let shoff = u32at(b, 0x20) as usize;
    let shnum = u16::from_le_bytes([b[0x30], b[0x31]]) as usize;
    let shstrndx = u16::from_le_bytes([b[0x32], b[0x33]]) as usize;
    let strtab_off = u32at(b, shoff + shstrndx * 40 + 0x10) as usize;
    (0..shnum)
        .map(|i| {
            let sh = shoff + i * 40;
            let name_off = strtab_off + u32at(b, sh) as usize;
            let end = b[name_off..].iter().position(|&c| c == 0).unwrap() + name_off;
            (
                String::from_utf8_lossy(&b[name_off..end]).into_owned(),
                u32at(b, sh + 4),
                u32at(b, sh + 8),
                u32at(b, sh + 0x14),
            )
        })
        .collect()
}

#[test]
fn i386_objects_use_i386_relocation_numbers() {
    // They used to carry x86-64 numbers. PC32 and PLT32 happen to coincide,
    // so calls looked fine, but R_386_32 is 1 where R_X86_64_32 is 10, and a
    // 32-bit object with an absolute symbol reference crashed `ld`.
    // Numbers checked against `as --32`.
    let asm = assemble_for(
        "i386",
        "movl $sym, %eax\ncall fn\ncall fn@PLT\n.word sym\n.byte sym\n",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
    // R_386_32, R_386_PC32 (a plain 32-bit call is not routed through the
    // PLT), R_386_PLT32, R_386_16, R_386_8.
    assert_eq!(kinds, vec![1, 2, 4, 20, 22]);
}

#[test]
fn i386_has_no_64_bit_relocation() {
    let e = errors_for("i386", ".quad sym\n");
    assert!(e.contains("no relocation exists"), "{e}");
}

#[test]
fn code32_inside_an_x86_64_object_keeps_x86_64_numbering_and_class() {
    // Numbering follows the object and a plain call follows the mode, both
    // checked against `as --64`: the 64-bit call goes through the PLT, the
    // `.code32` one is PC-relative, and both are R_X86_64_*.
    let src = "call fn\n.code32\nmovl $sym, %eax\ncall fn\n.long sym\n";
    let asm = assemble_for("x86-64", src);
    let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
    // R_X86_64_PLT32, R_X86_64_32, R_X86_64_PC32, R_X86_64_32.
    assert_eq!(kinds, vec![4, 10, 2, 10]);
    // And the file is still ELF64: ending in `.code32` must not turn an
    // x86-64 object into an x32 one.
    let b = output::elf::build(&asm).expect("ELF output");
    assert_eq!(b[4], 2, "ELFCLASS64");
}

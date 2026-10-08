//! Which references a symbol's binding leaves to the linker, what the
//! relocations name, and the alignment sections start with.
//!
//! Every expectation was read from the reference for the target — GNU as
//! 2.47 (or the host's, for x86) or llvm-mc 22, whichever its harness
//! checks — with `llvm-readobj`. The corpora behind them are the
//! `*-relocs.txt` files of tools/gas-diff, tools/mc-diff and tools/xas-diff.

// Each backend's test uses the helpers only when its feature is on.
#![allow(dead_code)]

mod common;
use common::*;

use rsasm::assembler::Assembler;

/// The relocations as (offset, type, symbol, addend), by offset. A symbol
/// is named as `display_name` shows it, so a section symbol is its section.
fn relocs(asm: &Assembler) -> Vec<(u64, u32, String, i64)> {
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let mut v: Vec<_> = asm
        .relocs
        .iter()
        .map(|r| {
            let name = r.symbol.map_or_else(String::new, |s| asm.display_name(s));
            (r.offset, r.kind, name, r.addend)
        })
        .collect();
    v.sort_by_key(|r| r.0);
    v
}

fn reloc(offset: u64, kind: u32, symbol: &str, addend: i64) -> (u64, u32, String, i64) {
    (offset, kind, symbol.to_string(), addend)
}

fn align(asm: &Assembler, name: &str) -> u64 {
    asm.sections
        .iter()
        .find(|s| asm.interner.get(s.name) == name)
        .unwrap_or_else(|| panic!("no section named `{name}`"))
        .align
}

#[cfg(feature = "x86")]
#[test]
fn x86_64_leaves_preemptible_symbols_to_the_linker_as_gnu_as_does() {
    // A call or a RIP-relative load of a global or weak symbol is relocated;
    // a jump GNU as relaxes is resolved for a global symbol but not for a
    // weak one; a local alias of a global symbol is resolved. A difference
    // in one section folds whatever the binding.
    let asm = assemble_for(
        "x86-64",
        "        .globl  glob
        .weak   weak
        .set    alias, glob
        call    local
        call    glob
        call    weak
        call    alias
        jmp     glob
        jz      weak
        lea     glob(%rip), %rax
        call    other
        call    ext
        .long   alias
        .long   local - ., glob - ., weak - .
local:  ret
glob:   ret
weak:   ret
        .section .text.other,\"ax\"
other:  ret
",
    );
    assert_eq!(
        relocs(&asm),
        vec![
            reloc(6, 4, "glob", -4),
            reloc(11, 4, "weak", -4),
            reloc(24, 4, "weak", -4),
            reloc(31, 2, "glob", -4),
            // A call to a local label is PC32, not PLT32.
            reloc(36, 2, ".text.other", -4),
            reloc(41, 4, "ext", -4),
            reloc(45, 10, ".text", 62),
        ]
    );
    assert_eq!(
        hex(&section(&asm, ".text")),
        "e8 38 00 00 00 e8 00 00 00 00 e8 00 00 00 00 e8 2a 00 00 00 eb 28 0f 84 00 00 00 00 \
         48 8d 05 00 00 00 00 e8 00 00 00 00 e8 00 00 00 00 00 00 00 00 0c 00 00 00 09 00 00 \
         00 06 00 00 00 c3 c3 c3"
    );
}

#[cfg(feature = "x86")]
#[test]
fn each_value_of_a_data_directive_has_its_own_location() {
    let asm = assemble_for("x86-64", ".data\n.long 0\n.long ., ., .\n.quad . - 8, .\n");
    assert_eq!(
        relocs(&asm),
        vec![
            reloc(4, 10, ".data", 4),
            reloc(8, 10, ".data", 8),
            reloc(12, 10, ".data", 12),
            reloc(16, 1, ".data", 8),
            reloc(24, 1, ".data", 24),
        ]
    );
}

#[cfg(feature = "x86")]
#[test]
fn an_undefined_symbol_is_global_in_the_object() {
    // GNU as writes `ext` as `GLOBAL UND` without a `.globl`; a local
    // undefined symbol is one GNU ld refuses to link.
    let asm = assemble_for("x86-64", "call ext\n");
    let b = rsasm::output::elf::build(&asm).expect("ELF output");
    let (symbols, first_global) = elf64_symbols(&b);
    let (index, bind) = symbols
        .iter()
        .enumerate()
        .find_map(|(i, (name, bind))| (name == "ext").then_some((i, *bind)))
        .expect("`ext` is in the symbol table");
    assert_eq!(bind, 1, "STB_GLOBAL");
    assert!(index >= first_global, "globals follow the locals");
}

/// The symbol table of a little-endian ELF64 object as (name, binding), and
/// the index of its first global.
#[cfg(feature = "x86")]
fn elf64_symbols(b: &[u8]) -> (Vec<(String, u8)>, usize) {
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    let u64at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) as usize;
    let (shoff, shnum) = (u64at(0x28), u16at(0x3c));
    let sh = (0..shnum)
        .map(|i| shoff + i * 64)
        .find(|&h| u32at(h + 4) == 2)
        .expect("a symbol table");
    let strtab = shoff + u32at(sh + 0x28) * 64;
    let (strings, off, size) = (u64at(strtab + 0x18), u64at(sh + 0x18), u64at(sh + 0x20));
    let symbols = (0..size / 24)
        .map(|i| {
            let s = off + i * 24;
            let name = strings + u32at(s);
            let end = name + b[name..].iter().position(|&c| c == 0).unwrap();
            (
                String::from_utf8_lossy(&b[name..end]).into_owned(),
                b[s + 4] >> 4,
            )
        })
        .collect();
    (symbols, u32at(sh + 0x2c))
}

#[cfg(feature = "m68k")]
#[test]
fn m68k_leaves_only_weak_symbols_to_the_linker() {
    // GNU as for m68k resolves a branch to a global symbol in its section,
    // and relocates data referring to one against the section.
    let mut asm = Assembler::new(
        rsasm::arch::lookup("m68k").unwrap(),
        rsasm::assembler::Options::new().with_dialect(rsasm::lexer::Dialect::Gas),
    );
    asm.assemble_str(
        "test.s",
        "        .globl  glob
        .weak   weak
        bsr     glob
        bsr     weak
        jbsr    weak
        .long   glob, weak
glob:   rts
weak:   rts
        .data
        .byte   1
",
    );
    asm.finish();
    assert_eq!(
        relocs(&asm),
        vec![
            reloc(6, 5, "weak", 0),
            reloc(10, 4, "weak", 0),
            reloc(14, 1, ".text", 22),
            reloc(18, 1, "weak", 0),
        ]
    );
    assert_eq!(
        hex(&section(&asm, ".text")),
        "61 00 00 14 61 00 00 00 61 ff 00 00 00 00 00 00 00 00 00 00 00 00 4e 75 4e 75"
    );
    assert_eq!((align(&asm, ".text"), align(&asm, ".data")), (4, 4));
}

#[cfg(feature = "arm")]
#[test]
fn arm_resolves_branches_to_local_labels_as_gnu_as_does() {
    // GNU as, the reference for ARM objects, resolves a `bl` or `blx` to a
    // local label beside it, and relocates any branch to a global symbol.
    // (llvm-mc relocates every `bl`; see tools/mc-diff/arm-relocs.txt.)
    let asm = assemble_for(
        "arm",
        "        .globl  glob
loc:    nop
        b       loc
        bl      loc
        bleq    loc
        blx     loc
        b       glob
glob:   nop
",
    );
    // ARM objects are REL: the -8 is in the field, where GNU as puts it.
    assert_eq!(relocs(&asm), vec![reloc(20, 29, "glob", -8)]);
    assert_eq!(
        hex(&section(&asm, ".text")),
        "00 f0 20 e3 fd ff ff ea fc ff ff eb fb ff ff 0b fa ff ff fa fe ff ff ea 00 f0 20 e3"
    );
}

#[cfg(feature = "riscv")]
#[test]
fn riscv_relocates_differences_it_cannot_fold_as_pairs() {
    // llvm-mc names a label at `.text+8` where rsasm names the section with
    // an addend of 8, and `base` and `other` likewise; the linker reads both
    // the same way.
    let asm = assemble_for(
        "riscv64",
        "        .globl  glob
        .weak   uweak
base:   call    glob
        .long   ext - .
        .long   base - uweak
        .short  other - base
glob:   ret
        .data
other:  .byte   0
",
    );
    assert_eq!(
        relocs(&asm),
        vec![
            reloc(0, 19, "glob", 0),
            reloc(8, 35, "ext", 0),
            reloc(8, 39, ".text", 8),
            reloc(12, 35, ".text", 0),
            reloc(12, 39, "uweak", 0),
            reloc(16, 34, ".data", 0),
            reloc(16, 38, ".text", 0),
        ]
    );
    assert_eq!(
        hex(&section(&asm, ".text")),
        "97 00 00 00 e7 80 00 00 00 00 00 00 00 00 00 00 00 00 82 80"
    );
}

#[cfg(feature = "v850")]
#[test]
fn v850_writes_the_section_offset_into_a_relocated_branch_as_gnu_as_does() {
    let asm = assemble_for(
        "v850",
        "        .globl  glob
        .weak   weak
        nop
        jarl    glob, lp
        jarl    weak, lp
glob:   nop
weak:   nop
",
    );
    assert_eq!(
        relocs(&asm),
        vec![reloc(2, 70, "glob", 0), reloc(6, 70, "weak", 0)]
    );
    assert_eq!(
        hex(&section(&asm, ".text")),
        "00 00 bf ff fe ff 80 ff 00 00 00 00 00 00"
    );
}

/// Sections as the references align them before anything asks for more.
#[test]
fn sections_start_with_the_references_alignment() {
    let src = "        .text
        .byte   1
        .data
        .byte   2
        .bss
        .space  1
        .section .rodata
        .byte   3
        .section .text.hot,\"ax\"
        .byte   4
";
    // .text, .data, .bss, .rodata, .text.hot
    #[allow(unused_mut)]
    let mut cases: Vec<(&str, [u64; 5])> = Vec::new();
    #[cfg(feature = "x86")]
    cases.push(("x86-64", [1, 1, 1, 1, 1]));
    #[cfg(feature = "aarch64")]
    cases.push(("aarch64", [4, 1, 1, 1, 4]));
    #[cfg(feature = "arm")]
    // GNU as: only an instruction aligns an ARM section.
    cases.push(("arm", [1, 1, 1, 1, 1]));
    #[cfg(feature = "riscv")]
    cases.push(("riscv64", [2, 1, 1, 1, 1]));
    #[cfg(feature = "powerpc")]
    cases.push(("powerpc", [4, 1, 1, 1, 1]));
    #[cfg(feature = "mips")]
    cases.push(("mips", [16, 16, 16, 1, 1]));
    #[cfg(feature = "sparc")]
    cases.push(("sparc", [4, 1, 1, 1, 1]));
    for (arch, want) in cases {
        let asm = assemble_for(arch, src);
        let got = [".text", ".data", ".bss", ".rodata", ".text.hot"].map(|s| align(&asm, s));
        assert_eq!(got, want, "{arch}");
    }
}

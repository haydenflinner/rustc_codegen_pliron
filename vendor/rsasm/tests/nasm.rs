//! The NASM dialect.
//!
//! Every expected value below was produced by `tools/nasm-diff/run.sh`, which
//! assembles the same source with NASM 2.16.03 and compares the bytes; a
//! hermetic test that only checked rsasm against rsasm could never catch a
//! wrong encoding. The differential corpus is the authority, and these are a
//! fast in-tree guard that the wiring stays put.

mod common;

#[cfg(feature = "x86")]
mod nasm {
    use super::common::*;
    use rsasm::lexer::Dialect::Nasm;
    use rsasm::section::SectionId;

    /// The bytes of a NASM flat binary's first section.
    fn flat(src: &str) -> String {
        hex(&text_dialect("x86-64", Nasm, src))
    }

    /// The named section's bytes from a relocatable NASM object.
    fn sect(arch: &str, src: &str, name: &str) -> String {
        let asm = assemble_dialect(arch, Nasm, src);
        assert!(
            !asm.diags.has_errors(),
            "assembly failed:\n{}\nsource:\n{src}",
            asm.diags.render(&asm.sm, false)
        );
        hex(&section(&asm, name))
    }

    #[test]
    fn data_directives_and_numbers() {
        assert_eq!(flat("db 1,2,0xff,-1"), "01 02 ff ff");
        assert_eq!(
            flat("dw 0x1234\ndd 0x11223344\ndq 1"),
            "34 12 44 33 22 11 01 00 00 00 00 00 00 00"
        );
        // The three kinds of string, and a byte-packed number.
        assert_eq!(flat("db 'AB', \"cd\", `e\\n`"), "41 42 63 64 65 0a");
        assert_eq!(flat("dw 'ab'"), "61 62");
        // NASM number spellings.
        assert_eq!(
            flat("db 0ffh, $0f, 0h1f, 17q, 0b1010, 0y11"),
            "ff 0f 1f 0f 0a 03"
        );
    }

    #[test]
    fn equ_and_local_labels() {
        // A `.local` label belongs to the last non-local one.
        assert_eq!(
            flat("base:\n.x: db 1\nn equ 3\n.y: db 2\ndw base.x, base.y, n"),
            "01 02 00 00 01 00 03 00"
        );
    }

    #[test]
    fn single_line_macros_and_assign() {
        assert_eq!(
            flat(
                "%define twice(x) ((x)*2)\n%assign i 0\n%rep 3\ndb twice(i)\n%assign i i+1\n%endrep"
            ),
            "00 02 04"
        );
        // `%xdefine` expands its body once, at definition.
        assert_eq!(
            flat("%define a 1\n%xdefine b a\n%define a 2\ndb b, a"),
            "01 02"
        );
    }

    #[test]
    fn multi_line_macros() {
        // Parameter count with a default, `%0`, and a greedy tail.
        assert_eq!(
            flat("%macro m 1-2 9\ndb %0, %1, %2\n%endmacro\nm 1\nm 1,2"),
            "02 01 09 02 01 02"
        );
        assert_eq!(
            flat("%macro all 1+\ndb %1\n%endmacro\nall 1,2,3"),
            "01 02 03"
        );
        // `%rotate` walks the arguments.
        assert_eq!(
            flat("%macro rev 1-*\n%rep %0\n%rotate -1\ndb %1\n%endrep\n%endmacro\nrev 1,2,3"),
            "03 02 01"
        );
        // `%%` gives each expansion its own label.
        assert_eq!(
            flat("%macro two 0\n%%a: db 1\ndb %%a - $$\n%endmacro\ntwo\ntwo"),
            "01 00 01 02"
        );
    }

    #[test]
    fn conditionals() {
        assert_eq!(
            flat(
                "%define X 1\n%ifdef X\ndb 1\n%endif\n%ifnum 5\ndb 2\n%endif\n\
                 %ifidn a, a\ndb 3\n%endif\n%if X == 1\ndb 4\n%else\ndb 5\n%endif"
            ),
            "01 02 03 04"
        );
    }

    #[test]
    fn times_and_boot_sector_tail() {
        assert_eq!(flat("times 3 db 0x90"), "90 90 90");
        // The classic `times 510-($-$$) db 0` padding to a 512-byte sector.
        let boot = flat("bits 16\ntimes 510-($-$$) db 0\ndw 0xaa55");
        assert_eq!(boot.len(), 512 * 3 - 1); // 512 bytes as "xx " groups
        assert!(boot.ends_with("55 aa"));
    }

    #[test]
    fn struc_lays_out_offsets() {
        // `struc`/`endstruc` define field offsets; `istruc`/`at` fill them.
        assert_eq!(
            flat(
                "struc point\n.x: resd 1\n.y: resw 1\nendstruc\n\
                 dd point.x, point.y, point_size\n\
                 istruc point\nat point.x, dd 1\nat point.y, dw 2\niend"
            ),
            "00 00 00 00 04 00 00 00 06 00 00 00 01 00 00 00 02 00"
        );
    }

    #[test]
    fn sixteen_bit_addressing() {
        // The 8086 addressing modes, which have their own ModRM layout.
        assert_eq!(
            flat("bits 16\nmov ax,[bx+si]\nmov ax,[bp+2]\nlodsw\npush es"),
            "8b 00 8b 46 02 ad 06"
        );
    }

    #[test]
    fn thirty_two_bit_forms() {
        // The accumulator moffs, the one-byte inc, and the segment/flag pushes.
        assert_eq!(
            flat("bits 32\nmov eax,[0x1000]\ninc eax\npush es\npusha"),
            "a1 00 10 00 00 40 06 60"
        );
    }

    #[test]
    fn sixty_four_bit_forms() {
        // `mov r64, imm` is a 32-bit load when the value fits, movabs for a
        // symbol; `[rel]` is RIP-relative.
        assert_eq!(
            flat("mov rax, 1\nmov rax, foo\nlea rax, [rel foo]\nfoo: ret"),
            "b8 01 00 00 00 48 b8 16 00 00 00 00 00 00 00 48 8d 05 00 00 00 00 c3"
        );
    }

    #[test]
    fn reserved_space_and_sections() {
        // `resb` in `.bss` reserves without emitting; the section carries the
        // size but no bytes.
        let asm = assemble_dialect(
            "x86-64",
            Nasm,
            "section .bss\nbuf: resb 16\nsection .text\nret",
        );
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        let bss = asm
            .sections
            .iter()
            .find(|s| asm.interner.get(s.name) == ".bss")
            .expect("a .bss section");
        assert_eq!(bss.size, 16);
        assert_eq!(hex(&asm.section_bytes(SectionId(0))), "c3");
    }

    #[test]
    fn default_rel_makes_symbols_rip_relative() {
        // With `default rel`, a bare `[sym]` is RIP-relative; `[abs sym]`
        // overrides it back to absolute.
        assert_eq!(
            sect(
                "x86-64",
                "default rel\nextern g\nmov eax, [g]\nlea rsi, [g]\nmov rax, [abs g]\nret",
                ".text"
            ),
            "8b 05 00 00 00 00 48 8d 35 00 00 00 00 48 8b 04 25 00 00 00 00 c3"
        );
    }

    #[test]
    fn a_symbol_defined_here_relocates_against_its_section() {
        // Like NASM, a reference to a defined symbol names its section, so a
        // linker sees `.data + offset` rather than the label.
        let asm = assemble_dialect("x86-64", Nasm, "section .data\ndd val\nval: dd 0");
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        assert_eq!(asm.relocs.len(), 1);
        let sym = asm.relocs[0].symbol.expect("a relocation symbol");
        assert_eq!(rsasm::symbol::SymType::Section, asm.symbols.get(sym).ty);
    }

    #[test]
    fn a_label_needs_no_colon_but_an_instruction_is_not_one() {
        // A word that is not an instruction is a label; one that is stays an
        // instruction.
        assert_eq!(flat("start db 1\ndb start"), "01 00");
        assert_eq!(flat("nop\nret"), "90 c3");
    }

    #[test]
    fn undefined_symbols_are_refused() {
        // NASM insists a symbol be defined or `extern`, rather than leaving it
        // to the linker.
        assert!(errors_dialect("x86-64", Nasm, "dd nowhere").contains("not defined"));
        // An `extern` one is fine.
        let asm = assemble_dialect("x86-64", Nasm, "extern ok\ndd ok");
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
    }
}

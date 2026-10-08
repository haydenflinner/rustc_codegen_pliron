//! The Motorola and Renesas dialects.
//!
//! Motorola expectations are the bytes vasm (`-no-opt -devpac`) and GNU as
//! `--mri` produced for the same source; the two agree on every rule tested
//! here. They are assembled on SPARC purely because it is big-endian like the
//! 68000, so the bytes read exactly as the reference assemblers printed them.
//! Nothing below emits an instruction, so no m68k backend is involved.

mod common;

#[cfg(feature = "sparc")]
mod motorola {
    use super::common::*;
    use rsasm::lexer::Dialect::Motorola;

    fn mot(src: &str) -> String {
        hex(&text_dialect("sparc", Motorola, src))
    }

    #[test]
    fn radix_prefixes() {
        assert_eq!(mot(" dc.b $7f,%1010,@17\n"), "7f 0a 0f");
        assert_eq!(mot(" dc.l $DFF096\n"), "00 df f0 96");
        // C-style hex still works alongside.
        assert_eq!(mot(" dc.b 0x10\n"), "10");
    }

    #[test]
    fn a_prefix_only_counts_before_a_digit_of_its_radix() {
        // `%` before something that is not binary is not a number, so it is
        // left as punctuation; here that makes the line an error rather than
        // a silently wrong value.
        assert!(errors_dialect("sparc", Motorola, " dc.b %2\n").contains("error"));
    }

    #[test]
    fn comments() {
        assert_eq!(mot("* a first-column comment\n dc.b 1\n"), "01");
        assert_eq!(mot(" dc.b 1 ; a trailing comment\n"), "01");
    }

    #[test]
    fn a_first_column_word_is_a_label_colon_or_not() {
        assert_eq!(mot("start dc.b 1\n dc.l start\n"), "01 00 00 00 00");
        assert_eq!(mot("start: dc.b 1\n dc.l start\n"), "01 00 00 00 00");
    }

    #[test]
    fn equ_defines_a_symbol() {
        assert_eq!(mot("VAL equ $42\n dc.b VAL\n"), "42");
        assert_eq!(mot("VAL: equ $42\n dc.b VAL\n"), "42");
        assert_eq!(mot("VAL set 1\n dc.b VAL\nVAL set 2\n dc.b VAL\n"), "01 02");
    }

    #[test]
    fn star_is_the_location_counter_in_operand_position() {
        assert_eq!(mot(" dc.b 1\n dc.b *\n"), "01 01");
        // ...and still multiplication between operands.
        assert_eq!(mot(" dc.b 2*3\n"), "06");
    }

    #[test]
    fn sized_data_space_and_fill() {
        assert_eq!(mot(" dc.w $1234\n dc.l $12345678\n"), "12 34 12 34 56 78");
        assert_eq!(mot(" ds.b 2\n dc.b 1\n"), "00 00 01");
        assert_eq!(mot(" dcb.w 3,$abcd\n"), "ab cd ab cd ab cd");
        assert_eq!(
            mot(" DC.W $1234\n"),
            "12 34",
            "directives are case-insensitive"
        );
    }

    #[test]
    fn strings_in_byte_data() {
        assert_eq!(mot(" dc.b \"hi\",0\n"), "68 69 00");
    }

    #[test]
    fn even_and_cnop() {
        assert_eq!(mot(" dc.b 1\n even\n dc.b 2\n"), "01 00 02");
        let asm = assemble_flat_dialect(
            "sparc",
            Motorola,
            " section d,data\n dc.b 1\n cnop 0,4\n dc.b 2\n",
            0,
        );
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        assert_eq!(hex(&section(&asm, "d")), "01 00 00 00 02");
    }

    #[test]
    fn conditionals_in_vendor_spelling() {
        assert_eq!(mot(" if 1\n dc.b 1\n else\n dc.b 2\n endif\n"), "01");
        assert_eq!(mot(" if 0\n dc.b 1\n else\n dc.b 2\n endc\n"), "02");
        assert_eq!(mot("X equ 1\n ifd X\n dc.b 1\n endif\n"), "01");
    }

    #[test]
    fn end_stops_assembly() {
        assert_eq!(mot(" dc.b 1\n end\n dc.b 2\n"), "01");
    }

    #[test]
    fn devpac_macros_take_their_name_from_the_label_and_args_by_position() {
        assert_eq!(
            mot("pair macro\n dc.b \\1,\\2\n endm\n pair 3,4\n"),
            "03 04"
        );
        // A missing positional argument expands to nothing.
        assert_eq!(mot("one macro\n dc.b 9\\1\n endm\n one\n"), "09");
    }

    #[test]
    fn quoted_strings_follow_vasm_and_gnu_mri() {
        assert_eq!(mot(" dc.b 'text',0\n"), "74 65 78 74 00");
        // A doubled quote is a quote; a backslash is just a backslash.
        assert_eq!(mot(" dc.b 'it''s'\n"), "69 74 27 73");
        assert_eq!(mot(" dc.b 'a\\n'\n"), "61 5c 6e");
        assert_eq!(mot(" dc.b \"a\"\"b\"\n"), "61 22 62");
        // In wider data, or inside an expression, a quoted literal is a number.
        assert_eq!(mot(" dc.w 'ab'\n"), "61 62");
        assert_eq!(mot(" dc.l 'abcd'\n"), "61 62 63 64");
        assert_eq!(mot(" dc.b 'a'+1\n"), "62");
    }

    #[test]
    fn rept_in_vendor_spelling() {
        assert_eq!(mot(" rept 3\n dc.b 7\n endr\n"), "07 07 07");
    }
}

#[cfg(feature = "x86")]
mod renesas {
    use super::common::*;
    use rsasm::lexer::Dialect::Renesas;

    fn ren(src: &str) -> String {
        hex(&text_dialect("x86-64", Renesas, src))
    }

    // No Renesas assembler is available as a reference; these follow the
    // RA78K0 language manual (U17198E), DB and DW directives and §2.4.
    #[test]
    fn db_takes_quoted_strings_and_parenthesised_sizes() {
        assert_eq!(ren("DB 'ABC',0\n"), "41 42 43 00");
        assert_eq!(ren("DB 'A''B'\n"), "41 27 42");
        assert_eq!(ren("DB (3+1)\n"), "00 00 00 00");
        assert_eq!(ren("DW (2)\n"), "00 00 00 00");
        // Parentheses that do not wrap the whole operand make a value.
        assert_eq!(ren("DB (1)+1\n"), "02");
    }

    #[test]
    fn radix_suffixes() {
        assert_eq!(ren("DB 0FFH, 1010B, 17O, 99D\n"), "ff 0a 0f 63");
        assert_eq!(ren("DW 1234H\n"), "34 12");
    }

    #[test]
    fn dollar_is_the_location_counter_not_a_hex_prefix() {
        assert_eq!(ren("DB 1\nDB $\n"), "01 01");
    }

    #[test]
    fn equ_without_a_colon() {
        assert_eq!(ren("SYM EQU 10H\nDB SYM\n"), "10");
    }

    #[test]
    fn segments_space_and_module_directives() {
        let asm = assemble_flat_dialect(
            "x86-64",
            Renesas,
            "NAME demo\nPUBLIC START\nEXTRN EXT\nCSEG\nSTART: DB 1\nDSEG\nDS 2\nEND\n",
            0,
        );
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        assert_eq!(section(&asm, ".text"), vec![1]);
        assert_eq!(section(&asm, ".data"), vec![0, 0]);
    }

    #[test]
    fn dotted_uppercase_spellings_work_too() {
        assert_eq!(ren(".DB 1\n.DW 2\n"), "01 02 00");
    }
}

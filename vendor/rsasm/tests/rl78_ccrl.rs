//! RL78 source in the syntax of Renesas CC-RL.
//!
//! No Renesas assembler is available to compare against, so the syntax
//! follows the *CC-RL Compiler User's Manual*, R20UT3123EJ0115, and every
//! encoding comes from GNU as: each program in `PAIRS` is a pair in
//! `tools/xas-diff/rl78-ccrl-pairs.txt`, where it sits beside a GNU-syntax
//! program meaning the same thing, and the expected bytes are what
//! `rl78-elf-as` (GNU binutils 2.47) made of that GNU half.
//! `tools/xas-diff/run.sh rl78-ccrl` checks the pairing again.

#![cfg(feature = "rl78")]

mod common;
use common::*;
use rsasm::lexer::Dialect::CcRl;
use rsasm::section::SectionId;

/// The first section's bytes of a relocatable assembly, or the diagnostics.
fn ccrl(src: &str) -> Result<String, String> {
    let asm = assemble_dialect("rl78", CcRl, src);
    if asm.diags.has_errors() {
        return Err(asm.diags.render(&asm.sm, false));
    }
    Ok(hex(&asm.section_bytes(SectionId(0))))
}

#[track_caller]
fn fails(src: &str, needle: &str) {
    match ccrl(src) {
        Ok(bytes) => panic!("expected an error mentioning `{needle}`, got {bytes}\nsource:\n{src}"),
        Err(e) => assert!(
            e.contains(needle),
            "`{needle}` not in:\n{e}\nsource:\n{src}"
        ),
    }
}

/// (what the pair shows, CC-RL source, the GNU half's bytes from GNU as)
const PAIRS: &[(&str, &str, &str)] = &[
    (
        "comments: `;` anywhere, `#` at the start of a line",
        "# a comment line\n\t.CSEG\tTEXT\nSTART:\tMOV\tA, #1\t\t; trailing comment\n\t# indented comment\n\tNOP\n",
        "51 01 00",
    ),
    (
        "numbers: prefix, suffix and leading-zero octal",
        "\t.DB\t0x1F, 0X1F, 1FH, 0FFh, 0b101, 101B, 074, 74O, 128, 0\n",
        "1f 1f 1f ff 05 05 3c 3c 80 00",
    ),
    (
        "character constants and strings with escapes",
        "\t.DB\t'A', ' ', '\\n', '\\'', \"AB\", \"\\x41\\101\\\\\", \"q\"\"q\"\n",
        "41 20 0a 27 41 42 41 41 5c 71 22 71",
    ),
    (
        "operator precedence: shifts bind like `*`, `+` binds tighter than `&`",
        "\t.DB\t1 + 2 << 1, 2 & 1 + 1, 6 | 1 ^ 3, 2 * 0x0F - 0x0B & 0x0A | 0x0F, 5+8-6*2/4\n",
        "05 02 04 0f 0a",
    ),
    (
        "relational and logical operators yield 0 or 1",
        "\t.DB\t1 == 1, 1 != 1, 2 > 1, 2 >= 3, 1 < 2, 3 <= 2, 1 && 0, 1 || 0, 256 % 50\n",
        "01 00 01 00 01 00 00 01 06",
    ),
    (
        "`>>` shifts the 32-bit value, and past 31 gives 0",
        "\t.DB4\t0xFFFFFFFF >> 4, 1 >> 32\n\t.DB\t0x01AF >> 5\n",
        "ff ff ff 0f 00 00 00 00 0d",
    ),
    (
        "byte and word separators, with and without parentheses",
        "\t.DB\tHIGH(0x1234), LOW(0x1234), HIGH 0x0FFFF, LOW(~3)\n\t.DB2\tHIGHW(0x12345678), LOWW(0x12345678), LOWW 0x12345678 + 1\n",
        "12 34 ff fc 34 12 78 56 79 56",
    ),
    (
        "separators in instruction operands",
        "\tMOVW\tAX, #HIGHW(0x12345678)\n\tMOVW\tBC, #LOWW(0x12345678)\n\tMOV\tA, #LOW(0x1234)\n\tMOV\tA, #HIGH(0x1234)\n",
        "30 34 12 32 78 56 51 34 51 12",
    ),
    (
        ".EQU and .SET",
        "SYM1\t.EQU\t0x10\nCNT\t.SET\t1\n\tMOV\tA, #SYM1\n\tMOV\tA, #CNT\nCNT\t.SET\tCNT + 1\n\tMOV\tA, #CNT\n",
        "51 10 51 01 51 02",
    ),
    (
        "data directives of every width",
        "LABEL:\t.DB\t10, \"ABC\"\n\t.DB2\t0x1234\n\t.DB4\t0x12345678\n\t.DB8\t0x1234567890ABCDEF\n",
        "0a 41 42 43 34 12 78 56 34 12 ef cd ab 90 78 56 34 12",
    ),
    (
        ".DS reserves zeroed bytes",
        "\t.DB\t1\n\t.DS\t3\n\t.DB\t2\n",
        "01 00 00 00 02",
    ),
    (
        ".ALIGN pads with zeros",
        "\t.DB\t1\n\t.ALIGN\t4\n\t.DB\t2\n\t.ALIGN\t2\n\t.DB\t3\n",
        "01 00 00 00 02 00 03 00",
    ),
    (
        ".OFFSET moves to an offset from the section start",
        "\t.DB\t1\n\t.OFFSET\t4\n\t.DB\t2\n",
        "01 00 00 00 02",
    ),
    (
        "$IF, $ELSEIF, $ELSE and $ENDIF",
        "SW\t.SET\t2\n$IF SW == 1\n\t.DB\t1\n$ELSEIF SW == 2\n\t.DB\t2\n$ELSE\n\t.DB\t3\n$ENDIF\n $ IFN SW\n\t.DB\t4\n $ ELSEIFN SW - 2\n\t.DB\t5\n $ ENDIF\n",
        "02 05",
    ),
    (
        "$IFDEF and $IFNDEF, nested inside a false branch",
        "DEFINED\t.EQU\t1\n$IFDEF DEFINED\n\t.DB\t1\n$ENDIF\n$IFNDEF DEFINED\n\t.DB\t2\n$IF 1\n\t.DB\t3\n$ENDIF\n$ELSE\n\t.DB\t4\n$ENDIF\n",
        "01 04",
    ),
    (
        ".MACRO with parameters, and `?` concatenation",
        "LAB5\t.EQU\t0x55\nADMAC\t.MACRO\tPARA1, PARA2\n\tMOV\tA, #PARA1\n\tADD\tA, #PARA2\n\t.DB\tLAB?PARA1\n\t.ENDM\n\tADMAC\t5, 0x20\n",
        "51 05 0c 20 55",
    ),
    (
        "a parameter is a whole word, and strings and comments are left alone",
        "X\t.MACRO\tA1\n\t.DB\tA1, \"A1\"\t; A1\n\t.DB\tA12\n\t.ENDM\nA12\t.EQU\t9\n\tX\t7\n",
        "07 41 31 09",
    ),
    (
        ".LOCAL gives each expansion its own label",
        "M1\t.MACRO\tPAR\n\t.LOCAL\tAA, BB\nAA:\t.DB\tPAR\n\tBR\t$AA\nBB:\t.DB\tBB - AA\n\t.ENDM\n\tM1\t1\n\tM1\t2\n",
        "01 ef fd 03 02 ef fd 03",
    ),
    (
        ".REPT and .IRP end with .ENDM, and nest in a macro",
        "\t.REPT\t2 + 1\n\tINC\tB\n\t.ENDM\n\t.IRP\tPAR 0x10, 0x20\n\tADD\tA, #PAR\n\t.ENDM\nTWICE\t.MACRO\tV\n\t.REPT\t2\n\t.DB\tV\n\t.ENDM\n\t.ENDM\n\tTWICE\t7\n",
        "83 83 83 0c 10 0c 20 07 07",
    ),
    (
        ".EXITM leaves the innermost repeat",
        "\t.REPT\t3\n\t.REPT\t2\n\tINC\tB\n\t.EXITM\n\t.ENDM\n\tDEC\tC\n\t.ENDM\n",
        "83 92 83 92 83 92",
    ),
    (
        "[DE] and [HL] stand for a zero displacement where the manual lists it",
        "\tMOV\t[DE], #1\n\tMOV\t[HL], #2\n\tMOV\tES:[DE], #3\n\tMOVS\t[HL], X\n\tCMPS\tX, [HL]\n\tADDW\tAX, [HL]\n\tSUBW\tAX, ES:[HL]\n\tCMPW\tAX, [HL]\n\tINC\t[HL]\n\tDEC\tES:[HL]\n\tINCW\t[HL]\n\tDECW\t[HL]\n",
        "ca 00 01 cc 00 02 11 ca 00 03 61 ce 00 61 de 00 61 09 00 11 61 29 00 61 49 00 61 59 00 11 61 69 00 61 79 00 61 89 00",
    ),
    (
        "operand sigils: #, !, !!, $, $!, ES: and bit positions",
        "\t.CSEG\tTEXT\nTOP:\tMOV\tA, !0xFE00\n\tMOV\tES:!0x1234, A\n\tMOV\tA, 0xFFE30\n\tMOVW\tAX, 0xFFF90\n\tBR\t!!0x12345\n\tCALL\t!0x1000\n\tBR\t$TOP\n\tBR\t$!TOP\n\tSET1\t0xFFE20.3\n\tCLR1\tA.5\n\tMOV1\tCY, [HL].2\n\tBT\t0xFFE21.0, $TOP\n\tMOV\tA, [HL+B]\n\tMOV\tA, 0x1234[B]\n\tMOV\tA, ES:0x1234[BC]\n",
        "8f 00 fe 11 9f 34 12 8d 30 ae 90 ec 45 23 01 fd 00 10 ef ec ee e9 ff 71 32 20 71 db 71 a4 31 02 21 de 61 c9 09 34 12 11 49 34 12",
    ),
    (
        "symbols may contain @ and $",
        "A@B\t.EQU\t3\nC$D\t.EQU\t4\n@E:\t.DB\tA@B, C$D\n",
        "03 04",
    ),
    (
        "the manual's own examples from the operator sections",
        "\tMOV\tA, #2 * 3\n\tMOV\tA, #250 / 50\n\tMOV\tA, #256 % 50\n\tMOV\tA, #LOW(~3)\n\tMOV\tA, #0x6FA & 0x0F\n\tMOV\tA, #0x0A | 0b1101\n\tMOV\tA, #0x9A ^ 0x9D\n\tMOVW\tAX, #0x01AF >> 5\n\tMOV\tA, #0x21 << 2\n\tMOV\tA, #(4 + 3) * 2\n",
        "51 06 51 05 51 06 51 fc 51 0a 51 0f 51 07 30 0d 00 51 84 51 0e",
    ),
];

#[test]
fn programs_match_their_gnu_equivalents() {
    let mut failures = Vec::new();
    for (name, src, want) in PAIRS {
        let got = ccrl(src).unwrap_or_else(|e| e);
        if got != *want {
            failures.push(format!("{name}\n  want: {want}\n   got: {got}"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

// ---- sections (R20UT3123EJ0115 §5.2.2, pages 485-500) ---------------------

fn sections(src: &str) -> Vec<(String, bool, bool, u64)> {
    let asm = assemble_dialect("rl78", CcRl, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm.sections
        .iter()
        .filter(|s| s.size > 0 || asm.interner.get(s.name) != ".text")
        .map(|s| {
            (
                asm.interner.get(s.name).to_string(),
                s.flags.exec,
                s.kind == rsasm::section::SectionKind::Nobits,
                s.align,
            )
        })
        .collect()
}

#[test]
fn segment_directives_name_sections_after_their_attribute() {
    let got = sections(
        " .CSEG TEXTF\n NOP\n .DSEG SBSS\n .DS 2\n .DSEG\n .DB 1\nCODE .CSEG\n NOP\n .CSEG CONST\n .DB 1\n",
    );
    let names: Vec<&str> = got.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(names, [".textf", ".sbss", ".data", "CODE", ".const"]);
    // `.sbss` holds no file data; `.const` is not code; the data attributes
    // align to 2 (Table 5.17, page 493).
    assert!(got[1].2);
    assert!(!got[4].1);
    assert_eq!(got[2].3, 2);
}

#[test]
fn section_directive_takes_a_name_attribute_and_alignment() {
    let got = sections(" .SECTION \"dat\", DATA, ALIGN=1\n .DB 1\n .SECTION .text, TEXT\n NOP\n");
    let dat = got
        .iter()
        .find(|s| s.0 == "dat")
        .expect("a section called `dat`");
    assert_eq!(dat.3, 1);
    assert!(!dat.1 && !dat.2);
}

#[test]
fn absolute_sections_get_the_manual_s_names() {
    // "name" + "_AT" + the address in uppercase hex (pages 486 and 498).
    let got = sections(
        "EX .CSEG AT 0x00200\n .DS 2\n .DSEG DATA_AT 0xff000\n .DS 1\n .ORG 0x12\n .DB 1\n",
    );
    let names: Vec<&str> = got.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(names, ["EX_AT200", ".data_ATFF000", ".data_AT12"]);
}

#[test]
fn a_section_name_keeps_its_attribute() {
    fails(
        " .SECTION X, TEXT\n .SECTION X, DATA\n",
        "different relocation attribute",
    );
}

#[test]
fn attributes_belong_to_their_directive() {
    fails(
        " .DSEG TEXT\n",
        "not a relocation attribute `.DSEG` accepts",
    );
    fails(" .CSEG BOGUS\n", "unknown relocation attribute `BOGUS`");
    fails(" .SECTION X\n", "expected `,` and a relocation attribute");
    fails(" .SECTION X, DATA, ALIGN=4\n", "section alignment 4");
    fails(
        " .SECTION X, TEXT, ALIGN=2\n",
        "cannot be given for a code section",
    );
    fails(" .CSEG AT 0x100000\n", "out of range");
}

// ---- symbols, data and conditionals ----------------------------------------

#[test]
fn set_symbols_are_redefined_and_equ_ones_are_not() {
    assert_eq!(
        ccrl("X .SET 1\n .DB X\nX .SET X + 1\n .DB X\n").unwrap(),
        "01 02"
    );
    fails("X .SET Y\nY .SET 1\n", "must be an absolute value");
    fails("X .EQU 1\nX .EQU 2\n", "symbol `X` is already defined");
}

#[test]
fn public_extern_and_weak_set_the_binding() {
    let asm = assemble_dialect(
        "rl78",
        CcRl,
        " .PUBLIC START\n .EXTERN EXT\n .WEAK W\nSTART: BR !!EXT\nW: RET\n",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let binding = |n: &str| {
        let id = asm.symbols.lookup(asm.interner.lookup(n).unwrap()).unwrap();
        asm.symbols.get(id).binding
    };
    assert_eq!(binding("START"), rsasm::symbol::Binding::Global);
    assert_eq!(binding("W"), rsasm::symbol::Binding::Weak);
}

#[test]
fn control_instructions_include_files() {
    let dir = std::env::temp_dir().join(format!("rsasm-ccrl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inc = dir.join("inc.asm");
    let bin = dir.join("data.bin");
    std::fs::write(&inc, " .DB 0x11\n").unwrap();
    std::fs::write(&bin, [0xaa, 0xbb]).unwrap();
    // A quoted file name takes escapes, so a Windows path's backslashes are
    // doubled.
    let src = format!(
        " $INCLUDE ({})\n $BINCLUDE \"{}\"\n",
        inc.display(),
        bin.display().to_string().replace('\\', "\\\\")
    );
    let got = ccrl(&src);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(got.unwrap(), "11 aa bb");
}

#[test]
fn a_false_branch_skips_everything_but_conditionals() {
    assert_eq!(
        ccrl(
            "$IF 0\n .DB 1\n $IF 1\n .DB 2\n $ENDIF\n NOT_AN_INSTRUCTION\n$ELSE\n .DB 3\n$ENDIF\n"
        )
        .unwrap(),
        "03"
    );
}

#[test]
fn ignored_directives_change_no_byte() {
    // `d7` is `rl78-elf-as`'s `ret`.
    assert_eq!(
        ccrl(" .LINE \"a.c\", 3\n .STACK f=2\n $NOWARNING\n $WARNING\n .TYPE f, FUNCTION, 2\nf: RET\n")
            .unwrap(),
        "d7"
    );
}

#[test]
fn macro_argument_count_follows_the_manual() {
    // Extra arguments are only warned about in CC-RL (page 527), and missing
    // ones are empty.
    let src = "M .MACRO A\n .DB 9?A\n .ENDM\n M\n M 1, 2\n";
    let asm = assemble_dialect("rl78", CcRl, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert!(
        asm.diags
            .render(&asm.sm, false)
            .contains("takes 1 argument(s), but 2 were given")
    );
    assert_eq!(hex(&asm.section_bytes(SectionId(0))), "09 5b");
}

#[test]
fn a_bare_word_is_never_a_directive() {
    // `BT` is a branch in CC-RL even though `.BT` is a compiler directive,
    // and `DB` is not `.DB`. The bytes are `rl78-elf-as`'s for
    // `LAB: bt a.0, $LAB`.
    assert_eq!(ccrl("LAB: BT A.0, $LAB\n").unwrap(), "31 03 fd");
    fails(" DB 1\n", "unknown RL78 instruction `db`");
}

// ---- what rsasm refuses, and says why --------------------------------------

#[test]
fn what_elf_cannot_carry_is_refused_with_a_reason() {
    fails(" .DB4 STARTOF(.text)\n", "optimizing linker");
    fails(" .DB2 SIZEOF(.data)\n", "optimizing linker");
    fails(" MOVW AX, #MIRLW(0x1000)\n", "mirror area");
    fails("B .EQU 0xFFE20\n .DB DATAPOS(B)\n", "bit symbol");
    fails(" .BSEG\n", "bit symbols");
    fails("F .DBIT\n", "bit symbols");
    fails(" .EXTBIT F\n", "bit symbols");
    fails("_f .VECTOR 8\n", "vector table");
    fails(" .BZ !L\n", "compiler output");
    fails(" $MIRROR X\n", "mirror area");
    fails(" .EXITMA\n", "`.EXITMA` is not supported");
}

#[test]
fn separators_of_a_label_have_no_relocation_here() {
    // GNU as's `%lo16`-style relocations are not implemented for RL78.
    fails(
        " .EXTERN EXT\n MOVW AX, #LOWW(EXT)\n",
        "operand of `LOWW` must be an absolute value",
    );
}

#[test]
fn alignment_must_be_a_power_of_two_and_even() {
    fails(" .ALIGN 3\n", "even number");
    fails(" .ALIGN 6\n", "not a power of two");
}

#[test]
fn block_structure_errors_name_the_problem() {
    fails(" .LOCAL A\n", "only allowed inside a macro");
    fails(" .ENDM\n", "without a matching");
    fails(" .REPT 2\n NOP\n", "unterminated block, expected `.endm`");
    fails(" .REPT -1\n .ENDM\n", "negative");
    fails(" .MACRO A\n .ENDM\n", "needs a name");
}

#[test]
fn prefix_octal_is_the_default_notation() {
    // `08` is only a number in suffix notation, which a program has to choose
    // with `-base_number=suffix`; rsasm reads the default.
    fails(" .DB 08\n", "invalid digit `8` for base-8");
}

/// Malformed CC-RL lines must produce diagnostics, never a panic.
const BAD: &[&str] = &[
    "$",
    "$ ",
    "$IF",
    "$ELSEIFN",
    "$INCLUDE",
    "$INCLUDE (",
    "$INCLUDE ()",
    "$BINCLUDE (nosuchfile)",
    " .CSEG AT",
    " .CSEG AT X",
    " .SECTION",
    " .SECTION ,",
    " .SECTION X,",
    " .SECTION X, DATA,",
    " .SECTION X, DATA, ALIGN",
    " .SECTION X, DATA, ALIGN=",
    " .ORG",
    " .ORG -1",
    " .ALIGN",
    " .ALIGN 0",
    " .DB",
    " .DB ,",
    " .DB \"",
    " .DB2 \"AB\"",
    " .DB HIGH",
    " .DB HIGH(",
    " .DB LOWW HIGHW",
    " .DS",
    " .PUBLIC",
    " .EXTERN 1",
    " .ALIAS",
    " .ALIAS A",
    " .ALIAS A,",
    "X .EQU",
    "X .SET",
    ".MACRO",
    "M .MACRO (",
    "M .MACRO A, A\n .ENDM",
    " .IRP",
    " .IRP X\n .ENDM",
    " .REPT X\n .ENDM",
    "M .MACRO\n .LOCAL\n .ENDM\n M",
    "M .MACRO\n .LOCAL ,\nL?: .ENDM\n M\n M",
    " MOV [DE], #",
    " MOV [HL",
    " INC [",
    " BR $",
    " BR !!",
    " SET1 .3",
    "@",
    "@@:",
    "?",
    "~",
    "'\\",
    "\"\\x",
];

#[test]
fn malformed_lines_never_panic() {
    for src in BAD {
        let _ = ccrl(src);
    }
}

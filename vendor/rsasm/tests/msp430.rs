//! MSP430 and MSP430X encoding tests.
//!
//! Every expected byte string, relocation and image here was produced by
//! `msp430-elf-as` and `msp430-elf-ld` from GNU binutils 2.47, the references
//! behind `tools/xas-diff/run.sh msp430 msp430x` and `tools/flat-diff/run.sh
//! msp430 msp430x`. `CORE` and `EXTENDED` are every fifth and fourth case of
//! `tools/xas-diff/msp430.txt` and `msp430x.txt` with the reference's bytes
//! beside it, and the programs are `msp430-programs.txt` and
//! `msp430x-programs.txt`. None of the expectations came from rsasm.

#![cfg(feature = "msp430")]

mod common;
use common::*;

/// Checks a whole table, reporting every mismatch rather than the first.
#[track_caller]
fn check(arch: &str, cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (src, want) in cases {
        let got = match try_text_for(arch, &format!("\t{src}\n")) {
            Ok(b) => hex(&b),
            Err(e) => format!("error: {e}"),
        };
        if got != *want {
            failures.push(format!("{src}\n  want: {want}\n   got: {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[track_caller]
fn check_programs(arch: &str, cases: &[(&str, &str, &str)]) {
    let mut failures = Vec::new();
    for (name, src, want) in cases {
        let got = match try_text_for(arch, src) {
            Ok(b) => hex(&b),
            Err(e) => format!("error: {e}"),
        };
        if got != *want {
            failures.push(format!("[{name}]\n{src}\n  want: {want}\n   got: {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} programs differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// The relocations of an object as (offset, type, symbol, addend), with a
/// relocation against no symbol named `*ABS*`.
fn relocs(arch: &str, src: &str) -> Vec<(u64, u32, String, i64)> {
    let asm = assemble_for(arch, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm.relocs
        .iter()
        .map(|r| {
            let name = r.symbol.map_or_else(
                || "*ABS*".to_string(),
                |s| asm.interner.get(asm.symbols.get(s).name).to_string(),
            );
            (r.offset, r.kind, name, r.addend)
        })
        .collect()
}

fn rows(want: &[(u64, u32, &str, i64)]) -> Vec<(u64, u32, String, i64)> {
    want.iter()
        .map(|&(o, k, s, a)| (o, k, s.to_string(), a))
        .collect()
}

// ---- encodings ------------------------------------------------------------

#[test]
fn corpus_core_instruction_set() {
    check("msp430", CORE);
}

#[test]
fn corpus_core_instruction_set_on_a_cpuxv2() {
    // `tools/xas-diff/run.sh msp430xv2` runs the same corpus with
    // `-mcpu=430xv2`, where only `push #4` and `push #8` come out
    // differently: the original 430 may not shorten them.
    let cases: Vec<(&str, &str)> = CORE
        .iter()
        .filter(|(src, _)| !src.starts_with("push"))
        .copied()
        .collect();
    check("msp430xv2", &cases);
}

#[test]
fn corpus_msp430x_instruction_set() {
    check("msp430x", EXTENDED);
}

#[test]
fn programs_for_the_430() {
    check_programs("msp430", PROGRAMS);
}

#[test]
fn programs_for_the_430x() {
    check_programs("msp430x", PROGRAMS_X);
}

#[test]
fn push_of_four_and_eight_is_long_on_the_430_only() {
    // Silicon erratum CPU4: the original MSP430 does not decode the constant
    // generator forms of `push #4` and `push #8`, so GNU as leaves them long.
    check(
        "msp430",
        &[("push #4", "30 12 04 00"), ("push #8", "30 12 08 00")],
    );
    check("msp430x", &[("push #4", "22 12"), ("push #8", "32 12")]);
}

#[test]
fn a_register_starts_an_operand_that_only_needs_one() {
    // GNU as reads a register name with `check_reg`, which stops at the first
    // character that is not a letter or a digit, so it takes `0x5(r4)` for
    // `r5` and `r4+1` for `r4` where a register is all it looks for.
    check(
        "msp430x",
        &[
            ("cmpa r4, 0x5(r4)", "d5 04"),
            ("mov r4+1, r5", "05 44"),
            ("mov 5, r6", "06 45"),
        ],
    );
}

// ---- objects ----------------------------------------------------------------

#[test]
fn core_relocations_follow_gnu_as() {
    // `imm_op` in GNU as is set by any source operand that is not `&addr` or
    // `@rN`, and picks the unchecked relocations for the 430.
    let src = "\t.text\nstart:\tmov\text, r5\n\tmov\tr4, ext\n\tmov\t&ext, ext\n\tmov.b\text, r5\n\
               \tmov\t&ext+2, r5\n\tmov.b\t&ext, r5\n\tmov\t#ext, r5\n\tmov\text(r4), r5\n\
               \tbr\t#ext\n\tbr\text\n\tcall\t#ext\n\tjmp\tstart\n\tjz\text+4\n\
               \tmov\t#hi(ext), r5\n\t.byte\text\n\t.word\text\n\t.long\text\n";
    assert_eq!(
        hex(&text_for("msp430", src)),
        "15 40 00 00 80 44 00 00 90 42 00 00 00 00 55 40 00 00 15 42 00 00 55 42 00 00 35 40 \
         00 00 15 44 00 00 30 40 00 00 10 40 00 00 b0 12 00 00 00 3c 00 24 35 40 00 00 00 00 \
         00 00 00 00 00"
    );
    assert_eq!(
        relocs("msp430", src),
        rows(&[
            (2, 6, "ext", 0),    // R_MSP430_16_PCREL_BYTE
            (6, 6, "ext", 0),    // R_MSP430_16_PCREL_BYTE
            (10, 3, "ext", 0),   // R_MSP430_16
            (12, 4, "ext", 0),   // R_MSP430_16_PCREL
            (16, 6, "ext", 0),   // R_MSP430_16_PCREL_BYTE
            (20, 3, "ext", 2),   // R_MSP430_16
            (24, 5, "ext", 0),   // R_MSP430_16_BYTE
            (28, 5, "ext", 0),   // R_MSP430_16_BYTE
            (32, 5, "ext", 0),   // R_MSP430_16_BYTE
            (36, 3, "ext", 0),   // R_MSP430_16
            (40, 4, "ext", 0),   // R_MSP430_16_PCREL
            (44, 5, "ext", 0),   // R_MSP430_16_BYTE
            (46, 2, "start", 0), // R_MSP430_10_PCREL, even to a label here
            (48, 2, "ext", 4),   // R_MSP430_10_PCREL
            (52, 5, "ext", 0),   // R_MSP430_16_BYTE: #hi() has none on the 430
            (54, 9, "ext", 0),   // R_MSP430_8
            (55, 5, "ext", 0),   // R_MSP430_16_BYTE
            (57, 1, "ext", 0),   // R_MSP430_32
        ])
    );
}

#[test]
fn msp430x_relocations_follow_gnu_as() {
    let src = "\t.text\nstart:\tmovx.a\t#ext, r6\n\tmovx.a\text, r6\n\tmovx.a\tr5, &ext\n\
               \tmovx.a\tr5, ext\n\tmovx.a\t&ext, ext2(r5)\n\tmovx.a\text, ext2\n\
               \tmova\t#ext, r6\n\tmova\text, r6\n\tmova\text(r5), r6\n\tmova\tr5, &ext\n\
               \tcalla\text\n\tcalla\t#ext\n\tcalla\t#0x12345\n\tadda\t#ext, r6\n\
               \tmov\t#ext, r5\n\tmov\text, r5\n\tmov\t#hi(ext), r5\n\tmov\t#lo(ext), r5\n\
               \tjmp\tstart\n\tbeq\text\n";
    assert_eq!(
        hex(&text_for("msp430x", src)),
        "00 18 76 40 00 00 00 18 56 40 00 00 00 18 c2 45 00 00 00 18 c0 45 00 00 00 18 d5 42 \
         00 00 00 00 00 18 d0 40 00 00 00 00 86 00 00 00 36 00 00 00 36 05 00 00 60 05 00 00 \
         90 13 00 00 b0 13 00 00 b0 13 00 00 a6 00 00 00 35 40 00 00 15 40 00 00 35 40 00 00 \
         35 40 00 00 00 3c 02 20 30 00 00 00"
    );
    assert_eq!(
        relocs("msp430x", src),
        rows(&[
            (0, 8, "ext", 0),           // R_MSP430X_ABS20_EXT_SRC
            (6, 5, "ext", 0),           // R_MSP430X_PCR20_EXT_SRC
            (12, 9, "ext", 0),          // R_MSP430X_ABS20_EXT_DST
            (18, 6, "ext", 0),          // R_MSP430X_PCR20_EXT_DST
            (24, 8, "ext", 0),          // R_MSP430X_ABS20_EXT_SRC
            (24, 10, "ext2", 0),        // R_MSP430X_ABS20_EXT_ODST
            (32, 5, "ext", 0),          // R_MSP430X_PCR20_EXT_SRC
            (32, 7, "ext2", 0),         // R_MSP430X_PCR20_EXT_ODST
            (40, 11, "ext", 0),         // R_MSP430X_ABS20_ADR_SRC
            (46, 13, "ext", 0),         // R_MSP430X_PCR16
            (50, 15, "ext", 0),         // R_MSP430X_ABS16
            (52, 12, "ext", 0),         // R_MSP430X_ABS20_ADR_DST
            (56, 14, "ext", 0),         // R_MSP430X_PCR20_CALL
            (60, 12, "ext", 0),         // R_MSP430X_ABS20_ADR_DST
            (64, 12, "*ABS*", 0x12345), // a number, relocated all the same
            (68, 11, "ext", 0),         // R_MSP430X_ABS20_ADR_SRC
            (74, 15, "ext", 0),         // R_MSP430X_ABS16
            (78, 13, "ext", 0),         // R_MSP430X_PCR16
            (82, 16, "ext", 0),         // R_MSP430_ABS_HI16
            (86, 2, "ext", 0),          // R_MSP430_ABS16
            (88, 19, "start", 0),       // R_MSP430X_10_PCREL
            (94, 13, "ext", 0),         // R_MSP430X_PCR16, the long `beq`
        ])
    );
}

#[test]
fn differences_of_code_labels_are_left_to_the_linker() {
    // GNU as writes `R_MSP430X_SYM_DIFF` naming the subtrahend, then the
    // value's relocation, and leaves the field zero; in a data section it
    // folds a difference of data labels.
    let src = "\t.text\n\tnop\n\tnop\na:\tnop\n\tnop\n\tnop\nb:\tnop\n\t.word b - a\n\t.long b - a\n\
               \tjmp a\n\t.data\nc:\t.word 1\nd:\t.word d - c\n";
    let asm = assemble_for("msp430x", src);
    assert_eq!(
        hex(&section(&asm, ".text")),
        "03 43 03 43 03 43 03 43 03 43 03 43 00 00 00 00 00 00 00 3c"
    );
    assert_eq!(hex(&section(&asm, ".data")), "01 00 02 00");
    let got: Vec<(u64, u32, i64)> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.addend))
        .collect();
    assert_eq!(
        got,
        vec![
            (0xc, 21, 0),
            (0xc, 2, 0),
            (0xe, 21, 0),
            (0xe, 1, 0),
            (0x12, 19, 0)
        ]
    );
}

#[test]
fn objects_carry_the_attributes_gnu_as_writes() {
    // `msp430-elf-objcopy --dump-section .MSP430.attributes` of the
    // reference's objects for `-mcpu=430`, `430x` and `430xv2`, whose headers
    // say `MSP430x11` (11) and `MSP430X` (45), with an OS/ABI of 255.
    for (arch, flags, attributes) in [
        (
            "msp430",
            11,
            "41 16 00 00 00 6d 73 70 61 62 69 00 01 0b 00 00 00 04 01 06 01 08 01",
        ),
        (
            "msp430x",
            45,
            "41 16 00 00 00 6d 73 70 61 62 69 00 01 0b 00 00 00 04 02 06 01 08 01",
        ),
        (
            "msp430xv2",
            45,
            "41 16 00 00 00 6d 73 70 61 62 69 00 01 0b 00 00 00 04 02 06 01 08 01",
        ),
    ] {
        let asm = assemble_for(arch, "\tnop\n");
        assert_eq!(
            hex(&section(&asm, ".MSP430.attributes")),
            attributes,
            "{arch}"
        );
        let elf = rsasm::output::elf::build(&asm).expect("an object");
        assert_eq!(elf[7], 255, "{arch}: EI_OSABI");
        assert_eq!(
            u16::from_le_bytes([elf[18], elf[19]]),
            105,
            "{arch}: e_machine"
        );
        assert_eq!(
            u32::from_le_bytes([elf[36], elf[37], elf[38], elf[39]]),
            flags,
            "{arch}: e_flags"
        );
    }
}

#[test]
fn data_and_bss_refer_to_the_runtime_that_sets_them_up() {
    // `msp430-elf-readelf -s`: GNU as adds an undefined reference to the C
    // runtime's routine for each kind of section with contents.
    let asm = assemble_for(
        "msp430",
        "\t.text\n\tnop\n\t.data\n\t.word 1\n\t.bss\n\t.space 2\n\
         \t.section .init_array, \"aw\"\n\t.word 0\n",
    );
    let names: Vec<&str> = asm
        .symbols
        .iter()
        .filter(|(_, s)| s.used && !s.is_defined())
        .map(|(_, s)| asm.interner.get(s.name))
        .collect();
    for want in [
        "__crt0_movedata",
        "__crt0_init_bss",
        "__crt0_run_init_array",
        "__crt0_run_array",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    // An empty `.data` refers to nothing.
    let asm = assemble_for("msp430", "\t.text\n\tnop\n\t.data\n");
    assert!(
        !asm.symbols
            .iter()
            .any(|(_, s)| asm.interner.get(s.name).starts_with("__crt0"))
    );
}

// ---- flat images --------------------------------------------------------------

#[test]
fn flat_images_match_the_gnu_linker() {
    // `tools/flat-diff/run.sh msp430` with FLAT_DIFF_SHOW=1: the reference
    // object linked by `msp430-elf-ld --no-relax` at 0x1000.
    for (src, image) in [
        (
            "        .text\nstart:  nop\nback:   jz      fwd\n        jmp     back\n\
             \x20       jnz     start\n        jc      fwd\n        jnc     back\n\
             \x20       jn      fwd\n        jge     back\n        jl      fwd\nfwd:    ret\n",
            "03 43 07 24 fe 3f fc 23 04 2c fb 2b 02 30 f9 37 00 38 30 41",
        ),
        (
            "        .data\n        .text\nstart:  mov     var(r4), r5\n\
             \x20       mov     r5, var2(r6)\n        mov     var(r4), var2(r5)\n\
             \x20       inc     var(r4)\n        tst.b   var2(r7)\n        ret\n\
             \x20       .data\n        .space  0x20\nvar:    .word   1\nvar2:   .word   2\n",
            "15 44 38 10 86 45 3a 10 95 44 38 10 3a 10 94 53 38 10 c7 93 3a 10 30 41 00 00 00 00 \
             00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
             01 00 02 00",
        ),
    ] {
        let asm = assemble_flat_for("msp430", src, 0x1000);
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        let bytes = rsasm::output::raw::build(&asm).expect("an image");
        assert_eq!(hex(&bytes), image, "\n{src}");
    }
}

// ---- refusals ---------------------------------------------------------------

#[test]
fn refusals_gnu_as_shares() {
    for (arch, src) in [
        ("msp430", "mov r5, #1234"),
        ("msp430", "mov r5, @r6+"),
        ("msp430", "mov 2(r2), r5"),
        ("msp430", "mov #0x10000, r5"),
        ("msp430", "jmp $+1030"),
        ("msp430", "rrax r5"),
        ("msp430", "movx r5, r6"),
        ("msp430x", "movx #0x100000, r5"),
        ("msp430x", "mova @r2, r5"),
        ("msp430x", "popm #5, r3"),
        ("msp430x", "rrcm #5, r5"),
        ("msp430x", "rpt #17 { rrax r5"),
        ("msp430x", "rpt #4 { mov r5, r6"),
        ("msp430x", "swpbx.b r5"),
        ("msp430x", "inc.a r5"),
        ("msp430x", "beq 1234"),
        ("msp430x", ".mspabi_attribute 4, 1"),
        ("msp430", ".mspabi_attribute 4, 2"),
        ("msp430xv2", "mov @pc, r5"),
        ("msp430xv2", "rrc pc"),
        ("msp430xv2", "popm #2, r3"),
    ] {
        errors_for(arch, &format!("\t{src}\n"));
    }
}

#[test]
fn refusals_where_gnu_as_writes_something_meaningless() {
    // GNU as ignores operands an instruction does not take, makes an opcode
    // of a `pushm` count outside 1 to 16, and branches a polymorph to the
    // label while dropping what is added to it.
    for src in [
        "nop r5",
        "mov r5, r6, r7",
        "pushm #0, r5",
        "pushm #17, r5",
        "beq ext+2",
    ] {
        errors_for("msp430x", &format!("\t{src}\n"));
    }
}

// ---- tables -------------------------------------------------------------------

const CORE: &[(&str, &str)] = &[
    ("mov\tr5, r6", "06 45"),
    ("mov\tsr, r6", "06 42"),
    ("mov\t#0, r6", "06 43"),
    ("mov\t#8, r6", "36 42"),
    ("mov\t#65535, r6", "36 43"),
    ("mov\t#llo(0x12345678), r6", "36 40 78 56"),
    ("mov\t#sym, r6", "36 40 00 00"),
    ("mov\t&0, r6", "16 42 00 00"),
    ("mov\t0(r4), r6", "26 44"),
    ("mov\t@r4, r6", "26 44"),
    ("mov\t@r3+, r6", "36 43"),
    ("mov\t5, r6", "06 45"),
    ("mov.b\tsp, r6", "46 41"),
    ("mov.b\tr15, r6", "46 4f"),
    ("mov.b\t#-1, r6", "76 43"),
    ("mov.b\t#-2, r6", "76 40 fe ff"),
    ("mov.b\t#lo(0x12345), r6", "76 40 45 23"),
    ("mov.b\t#hhi(0x12345678), r6", "46 43"),
    ("mov.b\t#hi(sym), r6", "76 40 00 00"),
    ("mov.b\t&sym+4, r6", "56 42 00 00"),
    ("mov.b\tsym(r4), r6", "56 44 00 00"),
    ("mov.b\t@r15, r6", "66 4f"),
    ("mov.b\t0x200, r6", "56 40 00 02"),
    ("mov.w\t#0, r6", "06 43"),
    ("mov.w\t@r4, r6", "26 44"),
    ("add\tpc, r6", "06 50"),
    ("add\tr3, r6", "06 53"),
    ("add\t#2, r6", "26 53"),
    ("add\t#0x1234, r6", "36 50 34 12"),
    ("add\t#0x8000, r6", "36 50 00 80"),
    ("add\t#hlo(0x12345678), r6", "06 53"),
    ("add\t#lo(sym), r6", "36 50 00 00"),
    ("add\t&sym, r6", "16 52 00 00"),
    ("add\t0x7fff(r15), r6", "16 5f ff 7f"),
    ("add\t@sp+, r6", "36 51"),
    ("add\tsym+2, r6", "16 50 00 00"),
    ("add.b\tr5, r6", "46 55"),
    ("add.b\tsr, r6", "46 52"),
    ("add.b\t#0, r6", "46 53"),
    ("add.b\t#8, r6", "76 52"),
    ("add.b\t#65535, r6", "76 53"),
    ("add.b\t#llo(0x12345678), r6", "76 50 78 56"),
    ("add.b\t#sym, r6", "76 50 00 00"),
    ("add.b\t&0, r6", "56 52 00 00"),
    ("add.b\t0(r4), r6", "66 54"),
    ("add.b\t@r4, r6", "66 54"),
    ("add.b\t@r3+, r6", "76 53"),
    ("add.b\t5, r6", "46 55"),
    ("add.w\t#0x1234, r6", "36 50 34 12"),
    ("add.w\tsym, r6", "16 50 00 00"),
    ("addc\t&0x200, r6", "16 62 00 02"),
    ("addc.b\tr5, r6", "46 65"),
    ("addc.b\t2(r4), r6", "56 64 02 00"),
    ("addc.w\t#0, r6", "06 63"),
    ("addc.w\t@r4, r6", "26 64"),
    ("sub\t#3, r6", "36 80 03 00"),
    ("sub\t@r4+, r6", "36 84"),
    ("sub.b\t#0x1234, r6", "76 80 34 12"),
    ("sub.b\tsym, r6", "56 80 00 00"),
    ("sub.w\t&0x200, r6", "16 82 00 02"),
    ("subc\tr5, r6", "06 75"),
    ("subc\t2(r4), r6", "16 74 02 00"),
    ("subc.b\t#0, r6", "46 73"),
    ("subc.b\t@r4, r6", "66 74"),
    ("subc.w\t#3, r6", "36 70 03 00"),
    ("subc.w\t@r4+, r6", "36 74"),
    ("cmp\tsp, r6", "06 91"),
    ("cmp\tr15, r6", "06 9f"),
    ("cmp\t#-1, r6", "36 93"),
    ("cmp\t#-2, r6", "36 90 fe ff"),
    ("cmp\t#lo(0x12345), r6", "36 90 45 23"),
    ("cmp\t#hhi(0x12345678), r6", "06 93"),
    ("cmp\t#hi(sym), r6", "36 90 00 00"),
    ("cmp\t&sym+4, r6", "16 92 00 00"),
    ("cmp\tsym(r4), r6", "16 94 00 00"),
    ("cmp\t@r15, r6", "26 9f"),
    ("cmp\t0x200, r6", "16 90 00 02"),
    ("cmp.b\tr0, r6", "46 90"),
    ("cmp.b\tr2, r6", "46 92"),
    ("cmp.b\t#1, r6", "56 93"),
    ("cmp.b\t#3, r6", "76 90 03 00"),
    ("cmp.b\t#-32768, r6", "76 90 00 80"),
    ("cmp.b\t#lhi(0x12345678), r6", "76 90 34 12"),
    ("cmp.b\t#sym+2, r6", "76 90 00 00"),
    ("cmp.b\t&0xffff, r6", "56 92 ff ff"),
    ("cmp.b\t-2(r1), r6", "56 91 fe ff"),
    ("cmp.b\t@r4+, r6", "76 94"),
    ("cmp.b\tsym, r6", "56 90 00 00"),
    ("cmp.b\t0x5, r6", "46 95"),
    ("cmp.w\t&0x200, r6", "16 92 00 02"),
    ("dadd\tr5, r6", "06 a5"),
    ("dadd\t2(r4), r6", "16 a4 02 00"),
    ("dadd.b\t#0, r6", "46 a3"),
    ("dadd.b\t@r4, r6", "66 a4"),
    ("dadd.w\t#3, r6", "36 a0 03 00"),
    ("dadd.w\t@r4+, r6", "36 a4"),
    ("bit\t#0x1234, r6", "36 b0 34 12"),
    ("bit\tsym, r6", "16 b0 00 00"),
    ("bit.b\t&0x200, r6", "56 b2 00 02"),
    ("bit.w\tr5, r6", "06 b5"),
    ("bit.w\t2(r4), r6", "16 b4 02 00"),
    ("bic\t#0, r6", "06 c3"),
    ("bic\t@r4, r6", "26 c4"),
    ("bic.b\t#3, r6", "76 c0 03 00"),
    ("bic.b\t@r4+, r6", "76 c4"),
    ("bic.w\t#0x1234, r6", "36 c0 34 12"),
    ("bic.w\tsym, r6", "16 c0 00 00"),
    ("bis\t&0x200, r6", "16 d2 00 02"),
    ("bis.b\tr5, r6", "46 d5"),
    ("bis.b\t2(r4), r6", "56 d4 02 00"),
    ("bis.w\t#0, r6", "06 d3"),
    ("bis.w\t@r4, r6", "26 d4"),
    ("xor\t#3, r6", "36 e0 03 00"),
    ("xor\t@r4+, r6", "36 e4"),
    ("xor.b\t#0x1234, r6", "76 e0 34 12"),
    ("xor.b\tsym, r6", "56 e0 00 00"),
    ("xor.w\t&0x200, r6", "16 e2 00 02"),
    ("and\tr5, r6", "06 f5"),
    ("and\t2(r4), r6", "16 f4 02 00"),
    ("and.b\t#0, r6", "46 f3"),
    ("and.b\t@r4, r6", "66 f4"),
    ("and.w\t#3, r6", "36 f0 03 00"),
    ("and.w\t@r4+, r6", "36 f4"),
    ("mov\tsym2, r6", "16 40 00 00"),
    ("mov\tr5, r2", "02 45"),
    ("mov\t#0x1234, r3", "33 40 34 12"),
    ("mov\t&0x202, 4(r7)", "97 42 02 02 04 00"),
    ("mov\tsym2, 0(r7)", "97 40 00 00 00 00"),
    ("mov\tr5, &sym", "82 45 00 00"),
    ("mov\t#0x1234, sym", "b0 40 34 12 00 00"),
    ("mov\t&0x202, sym(r8)", "98 42 02 02 00 00"),
    ("mov\tsym2, @r8", "98 40 00 00 00 00"),
    ("mov\tr5, #0", "03 45"),
    ("mov\t#0x1234, #1", "b3 40 34 12"),
    ("mov\t&0x202, #2", "93 42 02 02 00 00"),
    ("mov.b\tsym2, r6", "56 40 00 00"),
    ("mov.b\tr5, r2", "42 45"),
    ("mov.b\t#0x1234, r3", "73 40 34 12"),
    ("mov.b\t&0x202, 4(r7)", "d7 42 02 02 04 00"),
    ("mov.b\tsym2, 0(r7)", "d7 40 00 00 00 00"),
    ("mov.b\tr5, &sym", "c2 45 00 00"),
    ("mov.b\t#0x1234, sym", "f0 40 34 12 00 00"),
    ("mov.b\t&0x202, sym(r8)", "d8 42 02 02 00 00"),
    ("mov.b\tsym2, @r8", "d8 40 00 00 00 00"),
    ("mov.b\tr5, #0", "43 45"),
    ("mov.b\t#0x1234, #1", "f3 40 34 12"),
    ("mov.b\t&0x202, #2", "d3 42 02 02 00 00"),
    ("mov.w\tsym2, r6", "16 40 00 00"),
    ("mov.w\tr5, &0x300", "82 45 00 03"),
    ("mov.w\t#0x1234, sym", "b0 40 34 12 00 00"),
    ("add\tr5, &0x300", "82 55 00 03"),
    ("add.b\tr5, sym", "c0 55 00 00"),
    ("addc\tr5, r6", "06 65"),
    ("addc.b\tr5, 4(r7)", "c7 65 04 00"),
    ("addc.w\tr5, &0x300", "82 65 00 03"),
    ("sub\tr5, sym", "80 85 00 00"),
    ("sub.w\tr5, r6", "06 85"),
    ("subc\tr5, 4(r7)", "87 75 04 00"),
    ("subc.b\tr5, &0x300", "c2 75 00 03"),
    ("subc.w\tr5, sym", "80 75 00 00"),
    ("cmp.b\tr5, r6", "46 95"),
    ("cmp.w\tr5, 4(r7)", "87 95 04 00"),
    ("dadd\tr5, &0x300", "82 a5 00 03"),
    ("dadd.b\tr5, sym", "c0 a5 00 00"),
    ("bit\tr5, r6", "06 b5"),
    ("bit.b\tr5, 4(r7)", "c7 b5 04 00"),
    ("bit.w\tr5, &0x300", "82 b5 00 03"),
    ("bic\tr5, r3", "03 c5"),
    ("bic\tr5, sym", "80 c5 00 00"),
    ("bic\tr5, #1", "83 c5"),
    ("bic.b\tr5, r3", "43 c5"),
    ("bic.b\tr5, sym", "c0 c5 00 00"),
    ("bic.b\tr5, #1", "c3 c5"),
    ("bic.w\tr5, sym", "80 c5 00 00"),
    ("bis.b\tr5, r6", "46 d5"),
    ("bis.w\tr5, 4(r7)", "87 d5 04 00"),
    ("xor\tr5, &0x300", "82 e5 00 03"),
    ("xor.b\tr5, sym", "c0 e5 00 00"),
    ("and\tr5, r6", "06 f5"),
    ("and.b\tr5, 4(r7)", "c7 f5 04 00"),
    ("and.w\tr5, &0x300", "82 f5 00 03"),
    ("rrc\t#0", "03 10"),
    ("rrc\t#-1", "33 10"),
    ("rrc\t0(r4)", "24 10"),
    ("rrc\t0x200", "10 10 00 02"),
    ("rrc.b\t#1", "53 10"),
    ("rrc.b\t#sym", "70 10 00 00"),
    ("rrc.b\tsym(r4)", "54 10 00 00"),
    ("rrc.w\tr5", "05 10"),
    ("rrc.w\t#4", "22 10"),
    ("rrc.w\t&0x200", "12 10 00 02"),
    ("rrc.w\t@r4", "24 10"),
    ("swpb\tpc", "80 10"),
    ("swpb\t#4", "a2 10"),
    ("swpb\t&0x200", "92 10 00 02"),
    ("swpb\t@r4", "a4 10"),
    ("swpb.b\tpc", "c0 10"),
    ("swpb.b\t#4", "e2 10"),
    ("swpb.b\t&0x200", "d2 10 00 02"),
    ("swpb.b\t@r4", "e4 10"),
    ("swpb.w\tpc", "80 10"),
    ("swpb.w\t#4", "a2 10"),
    ("swpb.w\t&0x200", "92 10 00 02"),
    ("swpb.w\t@r4", "a4 10"),
    ("rra\tsr", "02 11"),
    ("rra\t#8", "32 11"),
    ("rra\t&sym", "12 11 00 00"),
    ("rra\t@r4+", "34 11"),
    ("rra.b\tr3", "43 11"),
    ("rra.b\t#0x1234", "70 11 34 12"),
    ("rra.b\t2(r4)", "54 11 02 00"),
    ("rra.b\tsym", "50 11 00 00"),
    ("rra.w\t#0", "03 11"),
    ("rra.w\t#-1", "33 11"),
    ("rra.w\t0(r4)", "24 11"),
    ("rra.w\t0x200", "10 11 00 02"),
    ("sxt\t#0", "83 11"),
    ("sxt\t#-1", "b3 11"),
    ("sxt\t0(r4)", "a4 11"),
    ("sxt\t0x200", "90 11 00 02"),
    ("sxt.b\t#0", "c3 11"),
    ("sxt.b\t#-1", "f3 11"),
    ("sxt.b\t0(r4)", "e4 11"),
    ("sxt.b\t0x200", "d0 11 00 02"),
    ("sxt.w\t#0", "83 11"),
    ("sxt.w\t#-1", "b3 11"),
    ("sxt.w\t0(r4)", "a4 11"),
    ("sxt.w\t0x200", "90 11 00 02"),
    ("push\t#0", "03 12"),
    ("push\t#-1", "33 12"),
    ("push\t0(r4)", "24 12"),
    ("push\t0x200", "10 12 00 02"),
    ("push.b\t#0", "43 12"),
    ("push.b\t#-1", "73 12"),
    ("push.b\t0(r4)", "64 12"),
    ("push.b\t0x200", "50 12 00 02"),
    ("push.w\t#0", "03 12"),
    ("push.w\t#-1", "33 12"),
    ("push.w\t0(r4)", "24 12"),
    ("push.w\t0x200", "10 12 00 02"),
    ("call\t#0", "83 12"),
    ("call\t#-1", "b3 12"),
    ("call\t0(r4)", "a4 12"),
    ("call\t0x200", "90 12 00 02"),
    ("call.b\t#0", "c3 12"),
    ("call.b\t#-1", "f3 12"),
    ("call.b\t0(r4)", "e4 12"),
    ("call.b\t0x200", "d0 12 00 02"),
    ("call.w\t#0", "83 12"),
    ("call.w\t#-1", "b3 12"),
    ("call.w\t0(r4)", "a4 12"),
    ("call.w\t0x200", "90 12 00 02"),
    ("setz", "22 d3"),
    ("dint", "32 c2"),
    ("inv\tpc", "30 e3"),
    ("inv\t&sym", "b2 e3 00 00"),
    ("inv.b\tpc", "70 e3"),
    ("inv.b\t&sym", "f2 e3 00 00"),
    ("inv.w\tpc", "30 e3"),
    ("inv.w\t&sym", "b2 e3 00 00"),
    ("dadc\tpc", "00 a3"),
    ("dadc\t&sym", "82 a3 00 00"),
    ("dadc.b\tpc", "40 a3"),
    ("dadc.b\t&sym", "c2 a3 00 00"),
    ("dadc.w\tpc", "00 a3"),
    ("dadc.w\t&sym", "82 a3 00 00"),
    ("tst\tpc", "00 93"),
    ("tst\t&sym", "82 93 00 00"),
    ("tst.b\tpc", "40 93"),
    ("tst.b\t&sym", "c2 93 00 00"),
    ("tst.w\tpc", "00 93"),
    ("tst.w\t&sym", "82 93 00 00"),
    ("decd\tpc", "20 83"),
    ("decd\t&sym", "a2 83 00 00"),
    ("decd.b\tpc", "60 83"),
    ("decd.b\t&sym", "e2 83 00 00"),
    ("decd.w\tpc", "20 83"),
    ("decd.w\t&sym", "a2 83 00 00"),
    ("dec\tpc", "10 83"),
    ("dec\t&sym", "92 83 00 00"),
    ("dec.b\tpc", "50 83"),
    ("dec.b\t&sym", "d2 83 00 00"),
    ("dec.w\tpc", "10 83"),
    ("dec.w\t&sym", "92 83 00 00"),
    ("sbc\tpc", "00 73"),
    ("sbc\t&sym", "82 73 00 00"),
    ("sbc.b\tpc", "40 73"),
    ("sbc.b\t&sym", "c2 73 00 00"),
    ("sbc.w\tpc", "00 73"),
    ("sbc.w\t&sym", "82 73 00 00"),
    ("adc\tpc", "00 63"),
    ("adc\t&sym", "82 63 00 00"),
    ("adc.b\tpc", "40 63"),
    ("adc.b\t&sym", "c2 63 00 00"),
    ("adc.w\tpc", "00 63"),
    ("adc.w\t&sym", "82 63 00 00"),
    ("incd\tpc", "20 53"),
    ("incd\t&sym", "a2 53 00 00"),
    ("incd.b\tpc", "60 53"),
    ("incd.b\t&sym", "e2 53 00 00"),
    ("incd.w\tpc", "20 53"),
    ("incd.w\t&sym", "a2 53 00 00"),
    ("inc\tpc", "10 53"),
    ("inc\t&sym", "92 53 00 00"),
    ("inc.b\tpc", "50 53"),
    ("inc.b\t&sym", "d2 53 00 00"),
    ("inc.w\tpc", "10 53"),
    ("inc.w\t&sym", "92 53 00 00"),
    ("clr\tpc", "00 43"),
    ("clr\t&sym", "82 43 00 00"),
    ("clr.b\tpc", "40 43"),
    ("clr.b\t&sym", "c2 43 00 00"),
    ("clr.w\tpc", "00 43"),
    ("clr.w\t&sym", "82 43 00 00"),
    ("pop\tpc", "30 41"),
    ("pop\t&sym", "b2 41 00 00"),
    ("pop.b\tpc", "70 41"),
    ("pop.b\t&sym", "f2 41 00 00"),
    ("pop.w\tpc", "30 41"),
    ("pop.w\t&sym", "b2 41 00 00"),
    ("rla\tsr", "02 52"),
    ("rla\tsym", "90 50 00 00 00 00"),
    ("rla.b\t4(r7)", "d7 57 04 00 04 00"),
    ("rla.b\t@r8", "e8 58 00 00"),
    ("rla.w\t0(r7)", "a7 57 00 00"),
    ("rla.w\tsym(r4)", "94 54 00 00 00 00"),
    ("rlc\t&0x300", "92 62 00 03 00 03"),
    ("rlc.b\tr5", "45 65"),
    ("rlc.b\t&sym", "d2 62 00 00 00 00"),
    ("rlc.w\tsr", "02 62"),
    ("rlc.w\tsym", "90 60 00 00 00 00"),
    ("br\t#0x1234", "30 40 34 12"),
    ("br\t&sym", "10 42 00 00"),
    ("br\tsym", "10 40 00 00"),
    ("jmp\t$+4", "01 3c"),
    ("jmp\t-4", "fd 3f"),
    ("jmp\t3", "02 3c"),
    ("jl\t$-4", "fd 3b"),
    ("jl\t$+1022", "fe 39"),
    ("jl\t$+3", "01 38"),
    ("jge\t$+0", "00 34"),
    ("jge\t$-1020", "01 36"),
    ("jn\tsym", "00 30"),
    ("jn\t0", "00 30"),
    ("jn\t1024", "00 32"),
    ("jc\tsym+4", "00 2c"),
    ("jc\t4", "02 2c"),
    ("jc\t$+1026", "00 2e"),
    ("jhs\t$+4", "01 2c"),
    ("jhs\t-4", "fd 2f"),
    ("jhs\t3", "02 2c"),
    ("jnc\t$-4", "fd 2b"),
    ("jnc\t$+1022", "fe 29"),
    ("jnc\t$+3", "01 28"),
    ("jlo\t$+0", "00 28"),
    ("jlo\t$-1020", "01 2a"),
    ("jz\tsym", "00 24"),
    ("jz\t0", "00 24"),
    ("jz\t1024", "00 26"),
    ("jeq\tsym+4", "00 24"),
    ("jeq\t4", "02 24"),
    ("jeq\t$+1026", "00 26"),
    ("jnz\t$+4", "01 20"),
    ("jnz\t-4", "fd 23"),
    ("jnz\t3", "02 20"),
    ("jne\t$-4", "fd 23"),
    ("jne\t$+1022", "fe 21"),
    ("jne\t$+3", "01 20"),
    ("mov\t@R4+ , r6", "36 44"),
    ("mov #'a', r6", "36 40 61 00"),
    ("mov rsp, r6", "06 41"),
];

const EXTENDED: &[(&str, &str)] = &[
    ("movx\tr5, r6", "40 18 06 45"),
    ("movx\t#2, r6", "40 18 26 43"),
    ("movx\t#3, r6", "40 18 36 40 03 00"),
    ("movx\t#-0x80000, r6", "40 1c 36 40 00 00"),
    ("movx\t#hi(sym), r6", "40 18 36 40 00 00"),
    ("movx\t2(r4), r6", "40 18 16 44 02 00"),
    ("movx\t@r4, r6", "40 18 26 44"),
    ("movx\t-4, r6", "c0 1f 36 40 fc ff"),
    ("movx\t0x54321(r9), r6", "c0 1a 16 49 21 43"),
    ("movx\t0x54321(r9), 4(r7)", "c0 1a 97 49 21 43 04 00"),
    ("movx\t0x54321(r9), 0x12345(r7)", "c1 1a 97 49 21 43 45 23"),
    ("movx\t0x54321(r9), &0x300", "c0 1a 92 49 21 43 00 03"),
    ("movx\t0x54321(r9), &0x12345", "c1 1a 92 49 21 43 45 23"),
    ("movx\t0x54321(r9), &sym", "c0 1a 92 49 21 43 00 00"),
    ("movx\t0x54321(r9), sym", "c0 1a 90 49 21 43 00 00"),
    ("movx\t0x54321(r9), sym(r8)", "c0 1a 98 49 21 43 00 00"),
    ("movx\t0x54321(r9), @r8", "c0 1a 98 49 21 43 00 00"),
    ("movx.b\t#1, r6", "40 18 56 43"),
    ("movx.b\t#8, r6", "40 18 76 42"),
    ("movx.b\t#0xfffff, r6", "c0 1f 76 40 ff ff"),
    ("movx.b\t#lo(sym), r6", "40 18 76 40 00 00"),
    ("movx.b\t&sym, r6", "40 18 56 42 00 00"),
    ("movx.b\tsym(r4), r6", "40 18 56 44 00 00"),
    ("movx.b\t0x12345, r6", "c0 18 56 40 45 23"),
    ("movx.b\tsym2, r6", "40 18 56 40 00 00"),
    ("movx.b\tsym2, 4(r7)", "40 18 d7 40 00 00 04 00"),
    ("movx.b\tsym2, 0x12345(r7)", "41 18 d7 40 00 00 45 23"),
    ("movx.b\tsym2, &0x300", "40 18 d2 40 00 00 00 03"),
    ("movx.b\tsym2, &0x12345", "41 18 d2 40 00 00 45 23"),
    ("movx.b\tsym2, &sym", "40 18 d2 40 00 00 00 00"),
    ("movx.b\tsym2, sym", "40 18 d0 40 00 00 00 00"),
    ("movx.b\tsym2, sym(r8)", "40 18 d8 40 00 00 00 00"),
    ("movx.b\tsym2, @r8", "40 18 d8 40 00 00 00 00"),
    ("movx.w\t#0, r6", "40 18 06 43"),
    ("movx.w\t#4, r6", "40 18 26 42"),
    ("movx.w\t#0x12345, r6", "c0 18 36 40 45 23"),
    ("movx.w\t#sym, r6", "40 18 36 40 00 00"),
    ("movx.w\t&0x12345, r6", "c0 18 16 42 45 23"),
    ("movx.w\t0(r4), r6", "40 18 26 44"),
    ("movx.w\tsym, r6", "40 18 16 40 00 00"),
    ("movx.w\t#0x12345, r6", "c0 18 36 40 45 23"),
    ("movx.w\t#0x12345, 4(r7)", "c0 18 b7 40 45 23 04 00"),
    ("movx.w\t#0x12345, 0x12345(r7)", "c1 18 b7 40 45 23 45 23"),
    ("movx.w\t#0x12345, &0x300", "c0 18 b2 40 45 23 00 03"),
    ("movx.w\t#0x12345, &0x12345", "c1 18 b2 40 45 23 45 23"),
    ("movx.w\t#0x12345, &sym", "c0 18 b2 40 45 23 00 00"),
    ("movx.w\t#0x12345, sym", "c0 18 b0 40 45 23 00 00"),
    ("movx.w\t#0x12345, sym(r8)", "c0 18 b8 40 45 23 00 00"),
    ("movx.w\t#0x12345, @r8", "c0 18 b8 40 45 23 00 00"),
    ("movx.a\tpc, r6", "00 18 46 40"),
    ("movx.a\t#-1, r6", "00 18 76 43"),
    ("movx.a\t#0x1234, r6", "00 18 76 40 34 12"),
    ("movx.a\t#0xffff, r6", "00 18 76 40 ff ff"),
    ("movx.a\t&0x200, r6", "00 18 56 42 00 02"),
    ("movx.a\t0x12345(r4), r6", "80 18 56 44 45 23"),
    ("movx.a\t@r4+, r6", "00 18 76 44"),
    ("movx.a\tr5, r6", "00 18 46 45"),
    ("movx.a\tr5, 4(r7)", "00 18 c7 45 04 00"),
    ("movx.a\tr5, 0x12345(r7)", "01 18 c7 45 45 23"),
    ("movx.a\tr5, &0x300", "00 18 c2 45 00 03"),
    ("movx.a\tr5, &0x12345", "01 18 c2 45 45 23"),
    ("movx.a\tr5, &sym", "00 18 c2 45 00 00"),
    ("movx.a\tr5, sym", "00 18 c0 45 00 00"),
    ("movx.a\tr5, sym(r8)", "00 18 c8 45 00 00"),
    ("movx.a\tr5, @r8", "00 18 c8 45 00 00"),
    ("addx\tr5, r6", "40 18 06 55"),
    ("addx\t#2, r6", "40 18 26 53"),
    ("addx\t#3, r6", "40 18 36 50 03 00"),
    ("addx\t#-0x80000, r6", "40 1c 36 50 00 00"),
    ("addx\t#hi(sym), r6", "40 18 36 50 00 00"),
    ("addx\t2(r4), r6", "40 18 16 54 02 00"),
    ("addx\t@r4, r6", "40 18 26 54"),
    ("addx\t-4, r6", "c0 1f 36 50 fc ff"),
    ("addx.b\tr5, r6", "40 18 46 55"),
    ("addx.b\t#2, r6", "40 18 66 53"),
    ("addx.b\t#3, r6", "40 18 76 50 03 00"),
    ("addx.b\t#-0x80000, r6", "40 1c 76 50 00 00"),
    ("addx.b\t#hi(sym), r6", "40 18 76 50 00 00"),
    ("addx.b\t2(r4), r6", "40 18 56 54 02 00"),
    ("addx.b\t@r4, r6", "40 18 66 54"),
    ("addx.b\t-4, r6", "c0 1f 76 50 fc ff"),
    ("addx.w\tr5, r6", "40 18 06 55"),
    ("addx.w\t#2, r6", "40 18 26 53"),
    ("addx.w\t#3, r6", "40 18 36 50 03 00"),
    ("addx.w\t#-0x80000, r6", "40 1c 36 50 00 00"),
    ("addx.w\t#hi(sym), r6", "40 18 36 50 00 00"),
    ("addx.w\t2(r4), r6", "40 18 16 54 02 00"),
    ("addx.w\t@r4, r6", "40 18 26 54"),
    ("addx.w\t-4, r6", "c0 1f 36 50 fc ff"),
    ("addx.a\tr5, r6", "00 18 46 55"),
    ("addx.a\t#2, r6", "00 18 66 53"),
    ("addx.a\t#3, r6", "00 18 76 50 03 00"),
    ("addx.a\t#-0x80000, r6", "00 1c 76 50 00 00"),
    ("addx.a\t#hi(sym), r6", "00 18 76 50 00 00"),
    ("addx.a\t2(r4), r6", "00 18 56 54 02 00"),
    ("addx.a\t@r4, r6", "00 18 66 54"),
    ("addx.a\t-4, r6", "80 1f 76 50 fc ff"),
    ("addcx\tr5, r6", "40 18 06 65"),
    ("addcx\t@r4+, r6", "40 18 36 64"),
    ("addcx.b\tr5, r6", "40 18 46 65"),
    ("addcx.b\t@r4+, r6", "40 18 76 64"),
    ("addcx.w\tr5, r6", "40 18 06 65"),
    ("addcx.w\t@r4+, r6", "40 18 36 64"),
    ("addcx.a\tr5, r6", "00 18 46 65"),
    ("addcx.a\t@r4+, r6", "00 18 76 64"),
    ("subx\tr5, r6", "40 18 06 85"),
    ("subx\t@r4+, r6", "40 18 36 84"),
    ("subx.b\tr5, r6", "40 18 46 85"),
    ("subx.b\t@r4+, r6", "40 18 76 84"),
    ("subx.w\tr5, r6", "40 18 06 85"),
    ("subx.w\t@r4+, r6", "40 18 36 84"),
    ("subx.a\tr5, r6", "00 18 46 85"),
    ("subx.a\t@r4+, r6", "00 18 76 84"),
    ("subcx\tr5, r6", "40 18 06 75"),
    ("subcx\t@r4+, r6", "40 18 36 74"),
    ("subcx.b\tr5, r6", "40 18 46 75"),
    ("subcx.b\t@r4+, r6", "40 18 76 74"),
    ("subcx.w\tr5, r6", "40 18 06 75"),
    ("subcx.w\t@r4+, r6", "40 18 36 74"),
    ("subcx.a\tr5, r6", "00 18 46 75"),
    ("subcx.a\t@r4+, r6", "00 18 76 74"),
    ("cmpx\tr5, r6", "40 18 06 95"),
    ("cmpx\t#2, r6", "40 18 26 93"),
    ("cmpx\t#3, r6", "40 18 36 90 03 00"),
    ("cmpx\t#-0x80000, r6", "40 1c 36 90 00 00"),
    ("cmpx\t#hi(sym), r6", "40 18 36 90 00 00"),
    ("cmpx\t2(r4), r6", "40 18 16 94 02 00"),
    ("cmpx\t@r4, r6", "40 18 26 94"),
    ("cmpx\t-4, r6", "c0 1f 36 90 fc ff"),
    ("cmpx.b\tr5, r6", "40 18 46 95"),
    ("cmpx.b\t#2, r6", "40 18 66 93"),
    ("cmpx.b\t#3, r6", "40 18 76 90 03 00"),
    ("cmpx.b\t#-0x80000, r6", "40 1c 76 90 00 00"),
    ("cmpx.b\t#hi(sym), r6", "40 18 76 90 00 00"),
    ("cmpx.b\t2(r4), r6", "40 18 56 94 02 00"),
    ("cmpx.b\t@r4, r6", "40 18 66 94"),
    ("cmpx.b\t-4, r6", "c0 1f 76 90 fc ff"),
    ("cmpx.w\tr5, r6", "40 18 06 95"),
    ("cmpx.w\t#2, r6", "40 18 26 93"),
    ("cmpx.w\t#3, r6", "40 18 36 90 03 00"),
    ("cmpx.w\t#-0x80000, r6", "40 1c 36 90 00 00"),
    ("cmpx.w\t#hi(sym), r6", "40 18 36 90 00 00"),
    ("cmpx.w\t2(r4), r6", "40 18 16 94 02 00"),
    ("cmpx.w\t@r4, r6", "40 18 26 94"),
    ("cmpx.w\t-4, r6", "c0 1f 36 90 fc ff"),
    ("cmpx.a\tr5, r6", "00 18 46 95"),
    ("cmpx.a\t#2, r6", "00 18 66 93"),
    ("cmpx.a\t#3, r6", "00 18 76 90 03 00"),
    ("cmpx.a\t#-0x80000, r6", "00 1c 76 90 00 00"),
    ("cmpx.a\t#hi(sym), r6", "00 18 76 90 00 00"),
    ("cmpx.a\t2(r4), r6", "00 18 56 94 02 00"),
    ("cmpx.a\t@r4, r6", "00 18 66 94"),
    ("cmpx.a\t-4, r6", "80 1f 76 90 fc ff"),
    ("daddx\tr5, r6", "40 18 06 a5"),
    ("daddx\t@r4+, r6", "40 18 36 a4"),
    ("daddx.b\tr5, r6", "40 18 46 a5"),
    ("daddx.b\t@r4+, r6", "40 18 76 a4"),
    ("daddx.w\tr5, r6", "40 18 06 a5"),
    ("daddx.w\t@r4+, r6", "40 18 36 a4"),
    ("daddx.a\tr5, r6", "00 18 46 a5"),
    ("daddx.a\t@r4+, r6", "00 18 76 a4"),
    ("bitx\tr5, r6", "40 18 06 b5"),
    ("bitx\t@r4+, r6", "40 18 36 b4"),
    ("bitx.b\tr5, r6", "40 18 46 b5"),
    ("bitx.b\t@r4+, r6", "40 18 76 b4"),
    ("bitx.w\tr5, r6", "40 18 06 b5"),
    ("bitx.w\t@r4+, r6", "40 18 36 b4"),
    ("bitx.a\tr5, r6", "00 18 46 b5"),
    ("bitx.a\t@r4+, r6", "00 18 76 b4"),
    ("bicx\tr5, r6", "40 18 06 c5"),
    ("bicx\t@r4+, r6", "40 18 36 c4"),
    ("bicx\tr5, &0x300", "40 18 82 c5 00 03"),
    ("bicx\tr5, sym(r8)", "40 18 88 c5 00 00"),
    ("bicx.b\t&0x12345, r6", "c0 18 56 c2 45 23"),
    ("bicx.b\tr5, 4(r7)", "40 18 c7 c5 04 00"),
    ("bicx.b\tr5, &sym", "40 18 c2 c5 00 00"),
    ("bicx.w\tr5, r6", "40 18 06 c5"),
    ("bicx.w\t@r4+, r6", "40 18 36 c4"),
    ("bicx.w\tr5, &0x300", "40 18 82 c5 00 03"),
    ("bicx.w\tr5, sym(r8)", "40 18 88 c5 00 00"),
    ("bicx.a\t&0x12345, r6", "80 18 56 c2 45 23"),
    ("bicx.a\tr5, 4(r7)", "00 18 c7 c5 04 00"),
    ("bicx.a\tr5, &sym", "00 18 c2 c5 00 00"),
    ("bisx\tr5, r6", "40 18 06 d5"),
    ("bisx\t@r4+, r6", "40 18 36 d4"),
    ("bisx.b\tr5, r6", "40 18 46 d5"),
    ("bisx.b\t@r4+, r6", "40 18 76 d4"),
    ("bisx.w\tr5, r6", "40 18 06 d5"),
    ("bisx.w\t@r4+, r6", "40 18 36 d4"),
    ("bisx.a\tr5, r6", "00 18 46 d5"),
    ("bisx.a\t@r4+, r6", "00 18 76 d4"),
    ("xorx\tr5, r6", "40 18 06 e5"),
    ("xorx\t@r4+, r6", "40 18 36 e4"),
    ("xorx.b\tr5, r6", "40 18 46 e5"),
    ("xorx.b\t@r4+, r6", "40 18 76 e4"),
    ("xorx.w\tr5, r6", "40 18 06 e5"),
    ("xorx.w\t@r4+, r6", "40 18 36 e4"),
    ("xorx.a\tr5, r6", "00 18 46 e5"),
    ("xorx.a\t@r4+, r6", "00 18 76 e4"),
    ("andx\tr5, r6", "40 18 06 f5"),
    ("andx\t@r4+, r6", "40 18 36 f4"),
    ("andx.b\tr5, r6", "40 18 46 f5"),
    ("andx.b\t@r4+, r6", "40 18 76 f4"),
    ("andx.w\tr5, r6", "40 18 06 f5"),
    ("andx.w\t@r4+, r6", "40 18 36 f4"),
    ("andx.a\tr5, r6", "00 18 46 f5"),
    ("andx.a\t@r4+, r6", "00 18 76 f4"),
    ("pushx\tr5", "40 18 05 12"),
    ("pushx\t&sym", "40 18 12 12 00 00"),
    ("pushx\t@r4+", "40 18 34 12"),
    ("pushx.b\t#0x12345", "c0 18 70 12 45 23"),
    ("pushx.b\t2(r4)", "40 18 54 12 02 00"),
    ("pushx.b\tsym", "40 18 50 12 00 00"),
    ("pushx.w\t#4", "40 18 22 12"),
    ("pushx.w\t0x12345(r4)", "c0 18 14 12 45 23"),
    ("pushx.w\t#sym", "40 18 30 12 00 00"),
    ("pushx.a\t&0x12345", "80 18 52 12 45 23"),
    ("pushx.a\t@r4", "00 18 64 12"),
    ("rrax\tr5", "40 18 05 11"),
    ("rrax\t0x12345(r4)", "c0 18 14 11 45 23"),
    ("rrax.b\tr5", "40 18 45 11"),
    ("rrax.b\t0x12345(r4)", "c0 18 54 11 45 23"),
    ("rrax.w\tr5", "40 18 05 11"),
    ("rrax.w\t0x12345(r4)", "c0 18 14 11 45 23"),
    ("rrax.a\tr5", "00 18 45 11"),
    ("rrax.a\t0x12345(r4)", "80 18 54 11 45 23"),
    ("rrcx\tr5", "40 18 05 10"),
    ("rrcx\t0x12345(r4)", "c0 18 14 10 45 23"),
    ("rrcx.b\tr5", "40 18 45 10"),
    ("rrcx.b\t0x12345(r4)", "c0 18 54 10 45 23"),
    ("rrcx.w\tr5", "40 18 05 10"),
    ("rrcx.w\t0x12345(r4)", "c0 18 14 10 45 23"),
    ("rrcx.a\tr5", "00 18 45 10"),
    ("rrcx.a\t0x12345(r4)", "80 18 54 10 45 23"),
    ("rrux\tr5", "40 19 05 10"),
    ("rrux\t0x12345(r4)", "c0 19 14 10 45 23"),
    ("rrux.b\tr5", "40 19 45 10"),
    ("rrux.b\t0x12345(r4)", "c0 19 54 10 45 23"),
    ("rrux.w\tr5", "40 19 05 10"),
    ("rrux.w\t0x12345(r4)", "c0 19 14 10 45 23"),
    ("rrux.a\tr5", "00 19 45 10"),
    ("rrux.a\t0x12345(r4)", "80 19 54 10 45 23"),
    ("swpbx\tr5", "40 18 85 10"),
    ("swpbx\t0x12345(r4)", "c0 18 94 10 45 23"),
    ("swpbx.w\tr5", "40 18 85 10"),
    ("swpbx.w\t0x12345(r4)", "c0 18 94 10 45 23"),
    ("swpbx.a\tr5", "00 18 85 10"),
    ("swpbx.a\t0x12345(r4)", "80 18 94 10 45 23"),
    ("sxtx\tr5", "40 18 85 11"),
    ("sxtx\t0x12345(r4)", "c0 18 94 11 45 23"),
    ("sxtx.w\tr5", "40 18 85 11"),
    ("sxtx.w\t0x12345(r4)", "c0 18 94 11 45 23"),
    ("sxtx.a\tr5", "00 18 85 11"),
    ("sxtx.a\t0x12345(r4)", "80 18 94 11 45 23"),
    ("adcx\tr5", "40 18 05 63"),
    ("adcx\t&sym", "40 18 82 63 00 00"),
    ("adcx.b\t0x12345(r7)", "c0 18 c7 63 45 23"),
    ("adcx.w\tr5", "40 18 05 63"),
    ("adcx.w\t&sym", "40 18 82 63 00 00"),
    ("adcx.a\t0x12345(r7)", "80 18 c7 63 45 23"),
    ("clra\tr5", "40 18 05 43"),
    ("clra\t&sym", "40 18 82 43 00 00"),
    ("clra.b\t0x12345(r7)", "c0 18 c7 43 45 23"),
    ("clra.w\tr5", "40 18 05 43"),
    ("clra.w\t&sym", "40 18 82 43 00 00"),
    ("clra.a\t0x12345(r7)", "80 18 c7 43 45 23"),
    ("clrx\tr5", "40 18 05 43"),
    ("clrx\t&sym", "40 18 82 43 00 00"),
    ("clrx.b\t0x12345(r7)", "c0 18 c7 43 45 23"),
    ("clrx.w\tr5", "40 18 05 43"),
    ("clrx.w\t&sym", "40 18 82 43 00 00"),
    ("clrx.a\t0x12345(r7)", "80 18 c7 43 45 23"),
    ("dadcx\tr5", "40 18 05 a3"),
    ("dadcx\t&sym", "40 18 82 a3 00 00"),
    ("dadcx.b\t0x12345(r7)", "c0 18 c7 a3 45 23"),
    ("dadcx.w\tr5", "40 18 05 a3"),
    ("dadcx.w\t&sym", "40 18 82 a3 00 00"),
    ("dadcx.a\t0x12345(r7)", "80 18 c7 a3 45 23"),
    ("decx\tr5", "40 18 15 83"),
    ("decx\t&sym", "40 18 92 83 00 00"),
    ("decx.b\t0x12345(r7)", "c0 18 d7 83 45 23"),
    ("decx.w\tr5", "40 18 15 83"),
    ("decx.w\t&sym", "40 18 92 83 00 00"),
    ("decx.a\t0x12345(r7)", "80 18 d7 83 45 23"),
    ("decda\tr5", "40 18 25 83"),
    ("decda\t&sym", "40 18 a2 83 00 00"),
    ("decda.b\t0x12345(r7)", "c0 18 e7 83 45 23"),
    ("decda.w\tr5", "40 18 25 83"),
    ("decda.w\t&sym", "40 18 a2 83 00 00"),
    ("decda.a\t0x12345(r7)", "80 18 e7 83 45 23"),
    ("decdx\tr5", "40 18 25 83"),
    ("decdx\t&sym", "40 18 a2 83 00 00"),
    ("decdx.b\t0x12345(r7)", "c0 18 e7 83 45 23"),
    ("decdx.w\tr5", "40 18 25 83"),
    ("decdx.w\t&sym", "40 18 a2 83 00 00"),
    ("decdx.a\t0x12345(r7)", "80 18 e7 83 45 23"),
    ("incx\tr5", "40 18 15 53"),
    ("incx\t&sym", "40 18 92 53 00 00"),
    ("incx.b\t0x12345(r7)", "c0 18 d7 53 45 23"),
    ("incx.w\tr5", "40 18 15 53"),
    ("incx.w\t&sym", "40 18 92 53 00 00"),
    ("incx.a\t0x12345(r7)", "80 18 d7 53 45 23"),
    ("incda\tr5", "40 18 25 53"),
    ("incda\t&sym", "40 18 a2 53 00 00"),
    ("incda.b\t0x12345(r7)", "c0 18 e7 53 45 23"),
    ("incda.w\tr5", "40 18 25 53"),
    ("incda.w\t&sym", "40 18 a2 53 00 00"),
    ("incda.a\t0x12345(r7)", "80 18 e7 53 45 23"),
    ("incdx\tr5", "40 18 25 53"),
    ("incdx\t&sym", "40 18 a2 53 00 00"),
    ("incdx.b\t0x12345(r7)", "c0 18 e7 53 45 23"),
    ("incdx.w\tr5", "40 18 25 53"),
    ("incdx.w\t&sym", "40 18 a2 53 00 00"),
    ("incdx.a\t0x12345(r7)", "80 18 e7 53 45 23"),
    ("invx\tr5", "40 18 35 e3"),
    ("invx\t&sym", "40 18 b2 e3 00 00"),
    ("invx.b\t0x12345(r7)", "c0 18 f7 e3 45 23"),
    ("invx.w\tr5", "40 18 35 e3"),
    ("invx.w\t&sym", "40 18 b2 e3 00 00"),
    ("invx.a\t0x12345(r7)", "80 18 f7 e3 45 23"),
    ("popx\tr5", "40 18 35 41"),
    ("popx\t&sym", "40 18 b2 41 00 00"),
    ("popx.b\t0x12345(r7)", "c0 18 f7 41 45 23"),
    ("popx.w\tr5", "40 18 35 41"),
    ("popx.w\t&sym", "40 18 b2 41 00 00"),
    ("popx.a\t0x12345(r7)", "80 18 f7 41 45 23"),
    ("rlax\tr5", "40 18 05 55"),
    ("rlax\t&sym", "40 18 92 52 00 00 00 00"),
    ("rlax.b\t0x12345(r7)", "c1 18 d7 57 45 23 45 23"),
    ("rlax.w\tr5", "40 18 05 55"),
    ("rlax.w\t&sym", "40 18 92 52 00 00 00 00"),
    ("rlax.a\t0x12345(r7)", "81 18 d7 57 45 23 45 23"),
    ("rlcx\tr5", "40 18 05 65"),
    ("rlcx\t&sym", "40 18 92 62 00 00 00 00"),
    ("rlcx.b\t0x12345(r7)", "c1 18 d7 67 45 23 45 23"),
    ("rlcx.w\tr5", "40 18 05 65"),
    ("rlcx.w\t&sym", "40 18 92 62 00 00 00 00"),
    ("rlcx.a\t0x12345(r7)", "81 18 d7 67 45 23 45 23"),
    ("sbcx\tr5", "40 18 05 73"),
    ("sbcx\t&sym", "40 18 82 73 00 00"),
    ("sbcx.b\t0x12345(r7)", "c0 18 c7 73 45 23"),
    ("sbcx.w\tr5", "40 18 05 73"),
    ("sbcx.w\t&sym", "40 18 82 73 00 00"),
    ("sbcx.a\t0x12345(r7)", "80 18 c7 73 45 23"),
    ("tsta\tr5", "40 18 05 93"),
    ("tsta\t&sym", "40 18 82 93 00 00"),
    ("tsta.b\t0x12345(r7)", "c0 18 c7 93 45 23"),
    ("tsta.w\tr5", "40 18 05 93"),
    ("tsta.w\t&sym", "40 18 82 93 00 00"),
    ("tsta.a\t0x12345(r7)", "80 18 c7 93 45 23"),
    ("tstx\tr5", "40 18 05 93"),
    ("tstx\t&sym", "40 18 82 93 00 00"),
    ("tstx.b\t0x12345(r7)", "c0 18 c7 93 45 23"),
    ("tstx.w\tr5", "40 18 05 93"),
    ("tstx.w\t&sym", "40 18 82 93 00 00"),
    ("tstx.a\t0x12345(r7)", "80 18 c7 93 45 23"),
    ("calla\tr6", "46 13"),
    ("calla\tsym", "90 13 00 00"),
    ("calla\t#0x12345", "b0 13 00 00"),
    ("mova &0x12345, r6", "26 01 45 23"),
    ("mova -2(r5), r6", "36 05 fe ff"),
    ("mova #1, r6", "86 00 01 00"),
    ("mova r5, 0x1234(r6)", "76 05 34 12"),
    ("mova pc, r5", "c5 00"),
    ("bra @r5", "00 05"),
    ("bra &0x12345", "20 01 45 23"),
    ("br.a r5", "c0 05"),
    ("adda #sym, r6", "a6 00 00 00"),
    ("cmpa #0x12345, r6", "96 01 45 23"),
    ("suba r5, r6", "f6 05"),
    ("incd.a r6", "40 18 26 53"),
    ("pushm\t#1, r10", "0a 15"),
    ("pushm\t#2, r4", "14 15"),
    ("pushm.a\t#8, r15", "7f 14"),
    ("pushm.w\t#1, r10", "0a 15"),
    ("pushm.w\t#2, r4", "14 15"),
    ("popm\t#8, r15", "78 17"),
    ("popm.a\t#1, r10", "0a 16"),
    ("popm.a\t#2, r4", "13 16"),
    ("popm.w\t#8, r15", "78 17"),
    ("rrcm\t#1, r5", "55 00"),
    ("rrcm.a\t#1, r5", "45 00"),
    ("rrcm.w\t#1, r5", "55 00"),
    ("rram\t#1, r5", "55 01"),
    ("rram.a\t#1, r5", "45 01"),
    ("rram.w\t#1, r5", "55 01"),
    ("rlam\t#1, r5", "55 02"),
    ("rlam.a\t#1, r5", "45 02"),
    ("rlam.w\t#1, r5", "55 02"),
    ("rrum\t#1, r5", "55 03"),
    ("rrum.a\t#1, r5", "45 03"),
    ("rrum.w\t#1, r5", "55 03"),
    ("rpt #4 { rrax.a r6", "03 18 46 11"),
    ("rpt #2 { movx r5, r6", "41 18 06 45"),
    ("push #4", "22 12"),
    ("mov @pc+, r5", "35 40"),
    ("mov #lo(sym), r5", "35 40 00 00"),
];

const PROGRAMS: &[(&str, &str, &str)] = &[
    (
        "blink an LED, with the watchdog held",
        "# Register addresses from the MSP430G2 family.\n        .equ    WDTCTL, 0x0120\n        .equ    WDTPW, 0x5a00\n        .equ    WDTHOLD, 0x0080\n        .equ    P1DIR, 0x0022\n        .equ    P1OUT, 0x0021\n        .equ    BIT0, 1\n\n        .text\n        .globl  main\nmain:   mov.w   #WDTPW|WDTHOLD, &WDTCTL   ; stop the watchdog\n        mov     #0x0280, sp\n        bis.b   #BIT0, &P1DIR\nloop:   xor.b   #BIT0, &P1OUT\n        mov     #50000, r15\n1:      dec     r15\n        jnz     1b\n        jmp     loop\n\n        .section __reset_vector, \"ax\", @progbits\n        .word   main\n\n",
        "b2 40 80 5a 20 01 31 40 80 02 d2 d3 22 00 d2 e3 21 00 3f 40 50 c3 1f 83 00 20 00 3c",
    ),
    (
        "a delay routine with a register frame",
        "        .text\ndelay:  push    r10\n        push    r11\n        mov     r15, r10\nouter:  mov     #1000, r11\ninner:  dec     r11\n        jnz     inner\n        dec     r10\n        jnz     outer\n        pop     r11\n        pop     r10\n        ret\n\n",
        "0a 12 0b 12 0a 4f 3b 40 e8 03 1b 83 00 20 1a 83 00 20 3b 41 3a 41 30 41",
    ),
    (
        "string output through a pointer",
        "        .equ    UCA0TXBUF, 0x0067\n        .equ    IFG2, 0x0003\n        .equ    UCA0TXIFG, 2\n        .text\nputs:   mov.b   @r15+, r14\n        tst.b   r14\n        jz      2f\n1:      bit.b   #UCA0TXIFG, &IFG2\n        jz      1b\n        mov.b   r14, &UCA0TXBUF\n        jmp     puts\n2:      ret\n        .section .rodata\nmsg:    .asciz  \"Hello, MSP430\\r\\n\"\n\n",
        "7e 4f 4e 93 00 24 e2 b3 03 00 00 24 c2 4e 67 00 00 3c 30 41",
    ),
    (
        "arithmetic on 32-bit values held in pairs of registers",
        "        .text\nadd32:  add     r12, r14\n        adc     r15\n        add     r13, r15\n        ret\nsub32:  sub     r12, r14\n        sbc     r15\n        sub     r13, r15\n        ret\nneg32:  inv     r14\n        inv     r15\n        inc     r14\n        adc     r15\n        ret\nshl32:  rla     r14\n        rlc     r15\n        ret\nshr32:  clrc\n        rrc     r15\n        rrc     r14\n        ret\nbcd:    clrc\n        dadd    r14, r15\n        dadc    r15\n        ret\n\n",
        "0e 5c 0f 63 0f 5d 30 41 0e 8c 0f 73 0f 8d 30 41 3e e3 3f e3 1e 53 0f 63 30 41 0e 5e 0f 6f 30 41 12 c3 0f 10 0e 10 30 41 12 c3 0f ae 0f a3 30 41",
    ),
    (
        "a jump table through local label differences",
        "        .text\ndispatch:\n        cmp     #3, r15\n        jhs     out\n        rla     r15\n        add     r15, pc\n        jmp     case0\n        jmp     case1\n        jmp     case2\ncase0:  mov     #10, r15\n        ret\ncase1:  mov     #20, r15\n        ret\ncase2:  mov     #30, r15\nout:    ret\n        .word   2f - 1f\n1:      nop\n        nop\n2:      nop\n\n",
        "3f 90 03 00 00 2c 0f 5f 00 5f 00 3c 00 3c 00 3c 3f 40 0a 00 30 41 3f 40 14 00 30 41 3f 40 1e 00 30 41 00 00 03 43 03 43 03 43",
    ),
    (
        "macros and repetition",
        "        .macro  save reg\n        push    \\reg\n        .endm\n        .macro  restore reg\n        pop     \\reg\n        .endm\n        .text\nisr:    save    r15\n        save    r14\n        .rept   3\n        rra     r14\n        .endr\n        .irp    r, r12, r13\n        clr     \\r\n        .endr\n        restore r14\n        restore r15\n        reti\n\n",
        "0f 12 0e 12 0e 11 0e 11 0e 11 0c 43 0d 43 3e 41 3f 41 00 13",
    ),
    (
        "conditional assembly",
        "        .equ    DEBUG, 1\n        .text\n        .if     DEBUG\n        mov     #0xdead, r15\n        .else\n        mov     #0, r15\n        .endif\n        .ifdef  UNDEFINED\n        nop\n        .endif\n        ret\n\n",
        "3f 40 ad de 30 41",
    ),
    (
        "separators, comments and number spellings",
        "        .text\n        mov #0FFh, r15 { mov #10h, r14 ; two statements\n# a line comment\n        mov     #0x10, r13      ; a comment\n        mov     #010, r12       ; ten, not eight\n        mov     #0b1010, r11\n        mov     #'A', r10\n        mov     #(3 << 4) | 5, r9\n        mov     #-1 & 0xff, r8\n        ret\n\n",
        "3f 40 ff 00 3e 40 10 00 3d 40 10 00 3c 40 0a 00 3b 40 0a 00 3a 40 41 00 39 40 35 00 38 40 ff 00 30 41",
    ),
    (
        "every addressing mode of mov, with numbers",
        "        .text\n        mov     r4, r5\n        mov     4(r4), r5\n        mov     &0x0200, r5\n        mov     @r4, r5\n        mov     @r4+, r5\n        mov     #0x1234, r5\n        mov     r4, 4(r5)\n        mov     r4, &0x0202\n        mov     4(r4), 6(r5)\n        mov     &0x0200, &0x0202\n        mov     @r4+, 2(r5)\n        mov     #0x5678, &0x0204\n        mov.b   @r4, 0(r5)\n        ret\n\n",
        "05 44 15 44 04 00 15 42 00 02 25 44 35 44 35 40 34 12 85 44 04 00 82 44 02 02 95 44 04 00 06 00 92 42 00 02 02 02 b5 44 02 00 b2 40 78 56 04 02 e5 44 00 00 30 41",
    ),
    (
        "the constant generators in every position",
        "        .text\n        mov     #0, r5\n        mov     #1, r5\n        mov     #2, r5\n        mov     #4, r5\n        mov     #8, r5\n        mov     #-1, r5\n        mov     #0xffff, r5\n        add     #2, 4(r5)\n        cmp.b   #8, &0x0200\n        bic     #4, sr\n        bis     #8, sr\n        push    #4\n        push    #8\n        push    #2\n        ret\n\n",
        "05 43 15 43 25 43 25 42 35 42 35 43 35 43 a5 53 04 00 f2 92 00 02 22 c2 32 d2 30 12 04 00 30 12 08 00 23 12 30 41",
    ),
    (
        "status bits",
        "        .text\n        dint\n        nop\n        setc\n        setz\n        setn\n        clrc\n        clrz\n        clrn\n        eint\n        nop\n        ret\n\n",
        "32 c2 03 43 12 d3 22 d3 22 d2 12 c3 22 c3 22 c2 32 d2 03 43 30 41",
    ),
    (
        "alignment within code",
        "        .text\n        nop\n        .balign 4\n        mov     #1, r5\n        .p2align 3\n        mov     #2, r5\n        .align  2\n        ret\n        .byte   1\n        .balign 2\n        ret\n\n",
        "03 43 00 00 15 43 00 00 25 43 00 00 30 41 01 00 30 41 00 00 00 00 00 00",
    ),
    (
        "data in a code section",
        "        .text\n        jmp     over\n        .byte   1, 2, 3\n        .balign 2\n        .word   0x1234, -1\n        .long   0x12345678\n        .ascii  \"ab\"\n        .space  4, 0xff\nover:   ret\n\n",
        "00 3c 01 02 03 00 34 12 ff ff 78 56 34 12 61 62 ff ff ff ff 30 41",
    ),
    (
        "jumps to numbers",
        "        .text\n        jmp     $+4\n        nop\n        jz      $-2\n        jnz     2\n        jc      -2\n        ret\n",
        "01 3c 03 43 fe 27 01 20 fe 2f 30 41",
    ),
];

const PROGRAMS_X: &[(&str, &str, &str)] = &[
    (
        "a large-model function with a pushm frame",
        "        .text\n        .globl  func\nfunc:   pushm.a #4, r10\n        mova    r12, r10\n        movx.w  &0x10000, r11\n        addx.a  r11, r10\n        calla   #helper\n        popm.a  #4, r10\n        reta\nhelper: reta\n\n",
        "3a 14 ca 0c c0 18 1b 42 00 00 00 18 4a 5b b0 13 00 00 37 16 10 01 10 01",
    ),
    (
        "20-bit arithmetic",
        "        .text\n        mova    #0x12345, r12\n        adda    #0x10000, r12\n        suba    r13, r12\n        cmpa    #0xfffff, r12\n        jz      1f\n        incdx.a r12\n        decx.a  r13\n1:      reta\n\n",
        "8c 01 45 23 ac 01 00 00 fc 0d 9c 0f ff ff 00 24 00 18 6c 53 00 18 5d 83 10 01",
    ),
    (
        "repeated shifts",
        "        .text\n        mov     #4, r14\n        rpt     r14\n        rrax.w  r12\n        rpt     #16\n        rlax.a  r13\n        rrcm.a  #4, r12\n        rram    #2, r13\n        rlam.a  #3, r14\n        rrum    #1, r15\n        rpt     #3 { rrux r12\n        reta\n\n",
        "2e 42 ce 18 0c 11 0f 18 4d 5d 4c 0c 5d 05 4e 0a 5f 03 42 19 0c 10 10 01",
    ),
    (
        "far memory through the extension words",
        "        .text\n        movx.a  &0x12345, r5\n        movx.a  r5, &0x54321\n        movx    0x12345(r4), 0x2(r6)\n        movx.b  #0xff, &0x20000\n        bicx.a  #0xf0000, r7\n        bisx.a  #0x10000, 0x10000(r8)\n        pushx.a &0x12344\n        popx.a  r9\n        tstx.a  0x10002(r10)\n        swpbx   r11\n        sxtx.a  r12\n        reta\n\n",
        "80 18 55 42 45 23 05 18 c2 45 21 43 c0 18 96 44 45 23 02 00 42 18 f2 40 ff 00 00 00 80 1f 77 c0 00 00 81 18 f8 d0 00 00 00 00 80 18 52 12 44 23 00 18 79 41 80 18 ca 93 02 00 40 18 8b 10 00 18 8c 11 10 01",
    ),
    (
        "address instructions in every form",
        "        .text\n        mova    r4, r5\n        mova    @r4, r5\n        mova    @r4+, r5\n        mova    &0x12345, r5\n        mova    0x1234(r4), r5\n        mova    #0x54321, r5\n        mova    r4, &0x12346\n        mova    r4, 0x1234(r5)\n        bra     r4\n        bra     @r4\n        bra     @r4+\n        bra     #0x12344\n        calla   r4\n        calla   @r4\n        calla   @r4+\n        calla   0x10(r4)\n        reta\n\n",
        "c5 04 05 04 15 04 25 01 45 23 35 04 34 12 85 05 21 43 61 04 46 23 75 04 34 12 c0 04 00 04 10 04 80 01 44 23 44 13 64 13 74 13 54 13 00 00 10 01",
    ),
    (
        "the core instruction set on the MSP430X",
        "        .text\n        mov     #0x1234, r5\n        push    #4\n        push    #8\n        call    #0x8000\n        br      #0x8000\n        mov.b   @r4+, 0(r5)\n        jmp     $+4\n        ret\n",
        "35 40 34 12 22 12 32 12 b0 12 00 80 30 40 00 80 f5 44 00 00 01 3c 30 41",
    ),
];

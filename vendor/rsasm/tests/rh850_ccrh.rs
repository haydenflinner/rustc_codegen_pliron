//! RH850 source in the syntax of Renesas CC-RH.
//!
//! No Renesas assembler is available to compare against, so the syntax and
//! the instruction expansions follow the *CC-RH Compiler User's Manual*,
//! R20UT3516EJ0113, and every encoding comes from GNU as: each program in
//! `PAIRS` is a pair in `tools/xas-diff/rh850-ccrh-pairs.txt`, where it sits
//! beside the GNU-syntax program the manual says it expands to, and the
//! expected bytes are what `v850-elf-as -mv850e3v5` (GNU binutils 2.47) made
//! of that GNU half. `tools/xas-diff/run.sh rh850-ccrh` checks the pairing
//! again, including the two pairs too long to keep here.

#![cfg(feature = "v850")]

mod common;
use common::*;
use rsasm::lexer::Dialect::{CcRh, Gas};
use rsasm::section::SectionId;

/// The first section's bytes of a relocatable assembly, or the diagnostics.
fn ccrh(src: &str) -> Result<String, String> {
    let asm = assemble_dialect("rh850", CcRh, src);
    if asm.diags.has_errors() {
        return Err(asm.diags.render(&asm.sm, false));
    }
    Ok(hex(&asm.section_bytes(SectionId(0))))
}

#[track_caller]
fn fails(src: &str, needle: &str) {
    match ccrh(src) {
        Ok(bytes) => panic!("expected an error mentioning `{needle}`, got {bytes}\nsource:\n{src}"),
        Err(e) => assert!(
            e.contains(needle),
            "`{needle}` not in:\n{e}\nsource:\n{src}"
        ),
    }
}

/// (what the pair shows, CC-RH source, the GNU half's bytes from GNU as)
const PAIRS: &[(&str, &str, &str)] = &[
    (
        "comments, numbers and character constants",
        "# a comment line\n\t.cseg\ttext\n\tmov\t10, r10\t\t; decimal\n\tmov\t0xA, r11\n\tmov\t012, r12\t; a leading zero is octal\n\tmov\t0b1010, r13\n\t.db\t'A', '\\n', \"a\\tb\"\n\t.align\t2\n",
        "0a 52 0a 5a 0a 62 0a 6a 41 0a 61 09 62 00",
    ),
    (
        "operator precedence: `&` binds tighter than `+`, and `!` is NOT",
        "\t.db\t2 & 1 + 1, 1 + 2 << 1, !0x3, 6 | 1 ^ 3\n\t.dw\t0x800001AF >> 5, 0x3BF >> 2 << 2\n",
        "01 05 fc 04 0d 00 00 fc bc 03 00 00",
    ),
    (
        "separators, the manual's examples",
        "\tmovea\tHIGHW(0x12345678), r0, r10\n\tmovea\tLOWW(0x12345678), r0, r10\n\tmovhi\tHIGHW1(0x12348765), r0, r10\n\tmov\tHIGH(0xC08), r10\n\tmov\tLOW(0xC08), r10\n",
        "20 56 34 12 20 56 78 56 40 56 35 12 0c 52 08 52",
    ),
    (
        "data directives, and CC-RH keeping the low bytes",
        "\t.db\t0xA, 0xB, 0x1FF\n\t.db2\t0x1234\n\t.dhw\t0x5678\n\t.dshw\t0x100\n\t.db4\t0x12345678\n\t.dw\t0x9ABCDEF0\n\t.db8\t0x1122334455667788\n\t.ddw\t1\n\t.ds\t3\n",
        "0a 0b ff 34 12 78 56 80 00 78 56 34 12 f0 de bc 9a 88 77 66 55 44 33 22 11 01 00 00 00 00 00 00 00 00 00 00",
    ),
    (
        ".align takes a fill byte",
        "\t.db\t1\n\t.align\t4, 0xff\n\t.db\t2\n",
        "01 ff ff ff 02 00 00 00",
    ),
    (
        ".set can be redefined, and each use sees its value then",
        "TEN\t.set\t0x10\n\tmov\tTEN - 0x05, r10\nTEN\t.set\tTEN + 1\n\tmov\tTEN, r10\n",
        "0b 52 20 56 11 00",
    ),
    (
        "mov chooses its form from the value",
        "\tmov\t15, r10\n\tmov\t-16, r10\n\tmov\t16, r10\n\tmov\t-32768, r10\n\tmov\t0x10000, r10\n\tmov\t0x12345, r10\n\tmov\t-1, r10\n\tmov\t0xFFFFFFFF, r10\n\tmov\t5, r0\n\tmov32\t0x10000, r10\n\tmov\tr11, r10\n",
        "0f 52 10 52 20 56 10 00 20 56 00 80 40 56 01 00 2a 06 45 23 01 00 1f 52 1f 52 20 06 05 00 00 00 2a 06 00 00 01 00 0b 50",
    ),
    (
        "movea widens to movhi and a movea through r1",
        "\tmovea\t0x7FFF, r11, r10\n\tmovea\t0x20000, r11, r10\n\tmovea\t0x12348765, r11, r10\n\tmovea\t0x12340001, r11, r10\n",
        "2b 56 ff 7f 4b 56 02 00 4b 0e 35 12 21 56 65 87 4b 0e 34 12 21 56 01 00",
    ),
    (
        "add and mulh",
        "\tadd\t15, r10\n\tadd\t100, r10\n\tadd\t-32768, r10\n\tadd\t0x30000, r10\n\tadd\t0x12345, r10\n\tadd\tr11, r10\n\tmulh\t7, r10\n\tmulh\t1000, r10\n\tmulh\t0x40000, r10\n\tmulh\t0x40001, r10\n",
        "4f 52 0a 56 64 00 0a 56 00 80 40 0e 03 00 c1 51 21 06 45 23 01 00 c1 51 cb 51 e7 52 ea 56 e8 03 40 0e 04 00 e1 50 21 06 01 00 04 00 e1 50",
    ),
    (
        "addi and mulhi, into r0, the source, or another register",
        "\taddi\t100, r11, r10\n\taddi\t0x50000, r11, r0\n\taddi\t0x50000, r11, r11\n\taddi\t0x50000, r11, r10\n\taddi\t0x50001, r11, r0\n\taddi\t0x50001, r11, r11\n\taddi\t0x50001, r11, r10\n\tmulhi\t0x60000, r11, r11\n\tmulhi\t0x60001, r11, r10\n",
        "0b 56 64 00 40 0e 05 00 cb 09 40 0e 05 00 c1 59 40 56 05 00 cb 51 21 06 01 00 05 00 cb 09 21 06 01 00 05 00 c1 59 2a 06 01 00 05 00 cb 51 40 0e 06 00 e1 58 2a 06 01 00 06 00 eb 50",
    ),
    (
        "cmp and satadd go through r1",
        "\tcmp\t-16, r10\n\tcmp\t100, r10\n\tcmp\t0x70000, r10\n\tcmp\t0x70001, r10\n\tsatadd\t3, r10\n\tsatadd\t-300, r10\n\tsatadd\t0x7FFFFFFF, r10\n",
        "70 52 20 0e 64 00 e1 51 40 0e 07 00 e1 51 21 06 01 00 07 00 e1 51 23 52 20 0e d4 fe c1 50 21 06 ff ff ff 7f c1 50",
    ),
    (
        "mul and mulu",
        "\tmul\t255, r10, r11\n\tmul\t-256, r10, r11\n\tmul\t1000, r10, r11\n\tmul\t0x80000, r10, r11\n\tmulu\t511, r10, r11\n\tmulu\t-1, r10, r11\n\tmulu\t600, r10, r11\n\tmulu\t0x80001, r10, r11\n",
        "ff 57 5c 5a e0 57 60 5a 20 0e e8 03 e1 57 20 5a 40 0e 08 00 e1 57 20 5a ff 57 7e 5a 1f 0a e1 57 22 5a 20 0e 58 02 e1 57 22 5a 21 06 01 00 08 00 e1 57 22 5a",
    ),
    (
        "divh with two operands, and the three-operand divisions",
        "\tdivh\t5, r10\n\tdivh\t500, r10\n\tdivh\t0x90000, r10\n\tdivh\t0, r10, r11\n\tdivh\t-3, r10, r11\n\tdiv\t1000, r10, r11\n\tdivhu\t0x90001, r10, r11\n\tdivu\t7, r10, r11\n\tdiv\tr12, r10, r11\n",
        "05 0a 41 50 20 0e f4 01 41 50 40 0e 09 00 41 50 e0 57 80 5a 1d 0a e1 57 80 5a 20 0e e8 03 e1 57 c0 5a 21 06 01 00 09 00 e1 57 82 5a 07 0a e1 57 c2 5a ec 57 c0 5a",
    ),
    (
        "satsub and satsubi",
        "\tsatsub\t0, r10\n\tsatsub\t100, r10\n\tsatsub\t0xA0000, r10\n\tsatsub\t0xA0001, r10\n\tsatsubi\t100, r11, r10\n\tsatsubi\t0xB0000, r11, r11\n\tsatsubi\t0xB0000, r11, r10\n\tsatsubi\t0xB0001, r11, r11\n\tsatsubi\t0xB0001, r11, r10\n",
        "a0 50 6a 56 64 00 40 0e 0a 00 a1 50 21 06 01 00 0a 00 a1 50 6b 56 64 00 40 0e 0b 00 a1 58 40 56 0b 00 8b 50 21 06 01 00 0b 00 a1 58 2a 06 01 00 0b 00 8b 50",
    ),
    (
        "and, or and xor with an immediate",
        "\tand\t0, r10\n\tand\t0xFFFF, r10\n\tand\t-1, r10\n\tand\t-100, r10\n\tand\t0xC0000, r10\n\tor\t0x10001, r10\n\txor\t0x8000, r10\n",
        "40 51 ca 56 ff ff 1f 0a 41 51 20 0e 9c ff 41 51 40 0e 0c 00 41 51 21 06 01 00 01 00 01 51 aa 56 00 80",
    ),
    (
        "andi, ori and xori with values they cannot hold",
        "\tandi\t0x1234, r11, r10\n\tandi\t-1, r11, r0\n\tandi\t-1, r11, r11\n\tandi\t-1, r11, r10\n\tori\t-100, r11, r10\n\txori\t0xD0000, r11, r11\n\tori\t0xD0001, r11, r10\n",
        "cb 56 34 12 1f 0a 4b 09 1f 0a 41 59 1f 52 4b 51 20 56 9c ff 0b 51 40 0e 0d 00 21 59 2a 06 01 00 0d 00 0b 51",
    ),
    (
        "not, satsubr, sub, subr and tst with an immediate",
        "\tnot\t0, r10\n\tnot\t5, r10\n\tsub\t100, r10\n\tsubr\t0xE0000, r10\n\ttst\t0xE0001, r10\n\tsatsubr\t-7, r10\n",
        "20 50 05 0a 21 50 20 0e 64 00 a1 51 40 0e 0e 00 81 51 21 06 01 00 0e 00 61 51 19 0a 81 50",
    ),
    (
        "condition-suffixed setf, sasf, adf, sbf and cmov",
        "\tsetfgt\tr10\n\tsetfnz\tr10\n\tsetfsa\tr10\n\tsasfz\tr10\n\tadfc\tr10, r11, r12\n\tsbfnv\tr10, r11, r12\n\tcmovlt\tr10, r11, r12\n\tcmovz\t5, r11, r12\n\tcmovz\t100, r11, r12\n\tcmov\t0x2, 0xF0000, r11, r12\n",
        "ef 57 00 00 ea 57 00 00 ed 57 00 00 e2 57 00 02 ea 5f a2 63 ea 5f 90 63 ea 5f 2c 63 e5 5f 04 63 20 0e 64 00 e1 5f 24 63 40 0e 0f 00 e1 5f 24 63",
    ),
    (
        "condition-suffixed cmpf",
        "\tcmpfeq.s\tr10, r11, 0\n\tcmpfngt.d\tr10, r12, 3\n",
        "eb 57 20 14 ec 57 36 7c",
    ),
    (
        "loads and stores choose 16 or 23 bits, then go through r1",
        "\tld.w\t4[sp], r10\n\tld.w\t[r11], r10\n\tld.b\t0x18000[r11], r12\n\tld.w\t0x18000[r11], r12\n\tld.hu\t-0x400000[r11], r12\n\tld.w\t0x12348000[r11], r12\n\tst.b\tr12, 0x7FFF[r11]\n\tst.w\tr12, 0x200000[r11]\n\tst.h\tr12, 0x1000000[r11]\n\tld23.w\t0x10000[r11], r12\n\tst23.b\tr12, -0x9000[r11]\n\tld.bu\t100, r12\n",
        "23 57 05 00 2b 57 01 00 8b 07 05 60 00 03 8b 07 09 60 00 03 ab 07 07 60 00 80 4b 0e 35 12 21 67 01 80 4b 67 ff 7f 8b 07 0f 60 00 40 4b 0e 00 01 61 67 00 00 8b 07 09 60 00 02 8b 07 0d 60 e0 fe 80 67 65 00",
    ),
    (
        "bit instructions, with the displacement and register left out",
        "\tset1\t3, 4[r10]\n\tclr1\t0, [r10]\n\tnot1\t7, 0x100\n\ttst1\t5, 0x9000[r10]\n\tset1\tr11, [r10]\n",
        "ca 1f 04 00 ca 87 00 00 c0 7f 00 01 4a 0e 01 00 c1 ef 00 90 ea 5f e0 00",
    ),
    (
        "sld and sst without [ep]",
        "\tsld.b\t4, r10\n\tsst.w\tr10, 8\n\tsld.h\t6[ep], r10\n",
        "04 53 05 55 03 54",
    ),
    (
        "push, pushm, pop and popm",
        "\tpush\tr10\n\tpop\tr10\n\tpushm\tr10, r11, r12\n\tpopm\tr10, r11, r12\n",
        "5c 1a 63 57 01 00 23 57 01 00 44 1a 03 1e f4 ff 63 67 09 00 63 5f 05 00 63 57 01 00 23 57 01 00 23 5f 05 00 23 67 09 00 03 1e 0c 00",
    ),
    (
        "prepare and dispose with a plain register list, sizes in bytes",
        "\tprepare\tr26, r29, r31, 0x10\n\tprepare\t0x103, 0x10\n\tprepare\tr20, r21, 4, sp\n\tprepare\tr20, 0, 0x1234\n\tdispose\t0x10, r26, r29, r31\n\tdispose\t8, r20, r21, [lp]\n",
        "88 07 61 20 88 07 61 20 82 07 03 0c 80 07 0b 08 34 12 48 06 60 20 44 06 1f 0c",
    ),
    (
        "prepare and dispose with frames too big for the instruction",
        "\tprepare\tr20, 0x200\n\tprepare\tr20, 0x10000\n\tdispose\t0x200, r20\n\tdispose\t0x10000, r20, [lp]\n",
        "80 07 01 08 23 1e 00 fe 80 07 01 08 21 06 00 00 01 00 a1 19 23 1e 00 02 40 06 00 08 21 06 00 00 01 00 c1 19 40 06 1f 08",
    ),
    (
        "pushsp and popsp take two operands",
        "\tpushsp\tr20, r25\n\tpopsp\tr20, r25\n",
        "f4 47 60 c9 f4 67 60 c9",
    ),
    (
        "branches to labels relax as in GNU as, and jcond is bcond",
        "\t.cseg\ttext\nstart:\tbz\tstart\n\tjnz\tstart\n\tjbr\tstart\n\tbsa\tstart\n\tbr\tstart\n\tjr\tstart\n\tjarl\tstart, lp\n\tjmp\t[r10]\n",
        "82 05 fa fd e5 fd dd fd c5 fd bf 07 f6 ff bf ff f2 ff 6a 00",
    ),
    (
        "a numeric branch displacement that fits 9 bits",
        "\tbz\t0x10\n\tbnz9\t-8\n\tjr\t0x100\n\tjr32\t0x400000\n\tjmp\t0x12345678\n",
        "82 0d ca fd 80 07 00 01 e0 02 00 00 40 00 e0 06 78 56 34 12",
    ),
    (
        "symbols may contain @ and $",
        "a@b\t.set\t3\nc$d\t.set\t4\n\tmov\ta@b, r10\n\tmov\tc$d, r11\n",
        "03 52 04 5a",
    ),
    (
        "conditional assembly",
        "SW\t.set\t0\n$IFDEF SW\n$IFN SW\n\tmov\t1, r10\n$ELSE\n\tmov\t2, r10\n$ENDIF\n$ENDIF\n$IFNDEF NOPE\n\tmov\t3, r10\n$ENDIF\n",
        "01 52 03 52",
    ),
    (
        "macros: named parameters, `~` concatenation, .local",
        "PUSHMAC\t.macro\tREG\n\tadd\t-4, sp\n\tst.w\tREG, 0x0[sp]\n\t.endm\nabc\t.macro\tx\nabc~x:\tmov\tr10, r20\n\t.endm\nm1\t.macro\tx\n\t.local\ta\na:\t.dw\ta - a + x\n\t.endm\n\tPUSHMAC\tr19\n\tabc\tSTU\n\tm1\t10\n\tm1\t20\n",
        "5c 1a 63 9f 01 00 0a a0 0a 00 00 00 14 00 00 00",
    ),
    (
        ".rept and .irp end with .endm",
        "\t.rept\t3\n\tnop\n\t.endm\n\t.irp\tPARA 1, 2, 3\n\tadd\tPARA, r10\n\t.endm\n",
        "00 00 00 00 00 00 41 52 42 52 43 52",
    ),
    (
        "the manual's own expression examples",
        "FIVE\t.set\t+5\nNO\t.set\t-1\n\tmov\t256 / 50, r10\n\tmov\t256 % 50, r10\n\tmov\t!0x3, r10\n\tmov\t0x6FA & 0xF, r10\n\tmov\t0xA | 0b1101, r10\n\tmov\t0x9A ^ 0x9D, r12\n\tmov\t0x21 << 2, r20\n\tmov\t(4 + 3) * 2, r10\n\tmov\tFIVE, r10\n\tmov\tNO, r10\n",
        "05 52 06 52 1c 52 0a 52 0f 52 07 62 20 a6 84 00 0e 52 05 52 1f 52",
    ),
    (
        "label references: `#label`, `!label` and the separators of a label",
        "\t.extern\text\n\tmov\t#ext, r10\n\tmovea\t#ext, r11, r10\n\tmovea\t!ext, r0, r10\n\tmovhi\tHIGHW1(#ext), r0, r10\n\tmovea\tLOWW(#ext), r10, r10\n\tmovhi\tHIGHW(ext), r0, r10\n\tadd\t#ext, r10\n\taddi\t!ext, r10, r11\n\tcmp\t#ext, r10\n\tld.w\t#ext[r11], r12\n\tst.w\tr12, LOWW(#ext)[r1]\n\tset1\t3, #ext[r10]\n\t.dw\t#ext, !ext\n",
        "2a 06 00 00 00 00 4b 0e 00 00 21 56 00 00 20 56 00 00 40 56 00 00 2a 56 00 00 40 56 00 00 21 06 00 00 00 00 c1 51 0a 5e 00 00 21 06 00 00 00 00 e1 51 4b 0e 00 00 21 67 01 00 61 67 01 00 4a 0e 00 00 c1 1f 00 00 00 00 00 00 00 00 00 00",
    ),
];

#[test]
fn programs_match_their_gnu_equivalents() {
    let mut failures = Vec::new();
    for (name, src, want) in PAIRS {
        let got = ccrh(src).unwrap_or_else(|e| e);
        if got != *want {
            failures.push(format!("{name}\n  want: {want}\n   got: {got}"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn numeric_branch_displacements_widen() {
    // The two long pairs of the corpus: a displacement past 9 bits takes the
    // 17-bit form, and one past 17 bits a `jr` behind the inverted
    // condition (page 534). The GNU halves branch to labels that far away;
    // GNU as's bytes for them are 4096 and 65554 long, and start as below.
    let got = ccrh(" bz 0x1000\n .ds 0x1000 - 4\n").unwrap();
    assert_eq!(got.len(), 4096 * 3 - 1);
    assert!(got.starts_with("e2 07 01 10 00"), "{}", &got[..40]);
    let got = ccrh(" bz 0x10004\n br 0x10004\n bsa 0x10008\n .ds 0x10000\n").unwrap();
    assert_eq!(got.len(), 65554 * 3 - 1);
    assert!(
        got.starts_with("ba 05 81 07 02 00 81 07 04 00 ad 05 b5 05 81 07 04 00 00"),
        "{}",
        &got[..60]
    );
}

/// Relocations of a relocatable assembly, as (offset, type, addend).
fn relocs(dialect: rsasm::lexer::Dialect, src: &str) -> Vec<(u64, u32, i64)> {
    let asm = assemble_dialect("rh850", dialect, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm.relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.addend))
        .collect()
}

#[test]
fn label_references_carry_the_gnu_relocations() {
    // The byte-level pairing cannot see relocations, so the CC-RH spellings
    // are compared with rsasm's own GNU-syntax output for the same program,
    // whose relocations `tests/v850.rs` checks against GNU as.
    let vendor = " .extern ext\n mov #ext, r10\n movea #ext, r11, r10\n movea !ext, r0, r10\n \
                  movhi HIGHW1(#ext), r0, r10\n movea LOWW(#ext), r10, r10\n movhi HIGHW(ext), r0, r10\n \
                  ld.w #ext[r11], r12\n set1 3, #ext[r10]\n .dw #ext\n";
    let gnu = " mov hilo(ext), r10\n movhi hi(ext), r11, r1\n movea lo(ext), r1, r10\n \
               movea zdaoff(ext), r0, r10\n movhi hi(ext), r0, r10\n movea lo(ext), r10, r10\n \
               movhi hi0(ext), r0, r10\n movhi hi(ext), r11, r1\n ld.w lo(ext)[r1], r12\n \
               movhi hi(ext), r10, r1\n set1 3, lo(ext)[r1]\n .long ext\n";
    let got = relocs(CcRh, vendor);
    assert_eq!(got.len(), 12);
    assert_eq!(got, relocs(Gas, gnu));
}

// ---- directives (R20UT3516EJ0113 §5.2, pages 424-467) -----------------------

#[test]
fn sections_take_the_cc_rh_attributes_and_alignments() {
    let asm = assemble_dialect(
        "rh850",
        CcRh,
        " .cseg zconst\n .db 1\n .dseg sbss\n .ds 4\nd .dseg tdata\n .db 1\n .section \"x\", edata23, align=2\n .db 1\n",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let find = |n: &str| {
        asm.sections
            .iter()
            .find(|s| asm.interner.get(s.name) == n)
            .unwrap_or_else(|| panic!("no section `{n}`"))
    };
    assert_eq!(find(".zconst").align, 4);
    assert!(!find(".zconst").flags.write);
    assert_eq!(find(".sbss").kind, rsasm::section::SectionKind::Nobits);
    assert!(find("d").flags.write);
    assert_eq!(find("x").align, 2);
}

#[test]
fn org_starts_a_section_named_with_dot_at() {
    // "section" + ".AT" + the address (page 432).
    let asm = assemble_dialect(
        "rh850",
        CcRh,
        " .section \"My_text\", text\n nop\n .org 0x50\n nop\n",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert!(
        asm.sections
            .iter()
            .any(|s| asm.interner.get(s.name) == "My_text.AT50")
    );
}

#[test]
fn data_labels_need_an_address_sigil() {
    // A plain label in data is its offset in its section (Table 5.25, page
    // 497), which ELF has no relocation for; `#label` is its address.
    fails(" .dw lab\nlab: .dw 0\n", "offset within its section");
    fails(" .extern e\n .db4 e\n", "offset within its section");
    assert!(ccrh(" .dw #lab, lab2 - lab\nlab: .dw 0\nlab2:\n").is_ok());
    // A constant defined later is fine without one.
    assert_eq!(ccrh(" .db2 K\nK .set 7\n").unwrap(), "07 00");
}

#[test]
fn nomacro_turns_the_expansions_off() {
    // `$NOMACRO` (page 471): the operands that need an expansion are refused.
    fails("$NOMACRO\n mov 0x10, r10\n", "`$NOMACRO` is in effect");
    fails(
        "$NOMACRO\n ld.w 0x18000[r11], r12\n",
        "`$NOMACRO` is in effect",
    );
    assert!(ccrh("$NOMACRO\n mov 5, r10\n$MACRO\n mov 0x10, r10\n").is_ok());
}

#[test]
fn macro_arguments_must_match_the_parameters() {
    // CC-RH wants as many arguments as parameters (page 460).
    fails(
        "m .macro a, b\n.endm\n m 1\n",
        "takes 2 argument(s), but 1 were given",
    );
    assert_eq!(
        ccrh("m .macro (a, b)\n .db a, b\n .endm\n m 1, 2\n").unwrap(),
        "01 02"
    );
}

// ---- what rsasm refuses, and says why --------------------------------------

#[test]
fn gp_and_ep_relative_references_are_refused() {
    fails(" ld.w $lab[gp], r10\n", "offset from `gp`");
    fails(" sld.b %lab, r10\n", "offset from `ep`");
    fails(" .dw $lab\n", "gp-relative label reference cannot be data");
    fails("$DATA x\n", "32-bit gp-relative");
}

#[test]
fn a_bare_label_is_refused_where_cc_rh_expands() {
    fails(
        " mov lab, r10\nlab:\n",
        "must be a constant defined before this line",
    );
    fails(
        " add later, r10\nlater .set 1\n",
        "must be a constant defined before this line",
    );
    fails(" ld.w !lab[r11], r10\n", "cannot be a displacement");
}

#[test]
fn expansion_errors_name_the_problem() {
    fails(
        " mulhi 0x50000, r11, r0\n",
        "`mulhi` cannot store its result in r0",
    );
    fails(" divh 0, r10\n", "divides by zero");
    fails(" bt lab\nlab:\n", "cannot be used in CC-RH");
    fails(" br17 lab\nlab:\n", "there is no `br17`");
    fails(" bz9 0x200\n", "out of range");
    fails(" prepare r20, 0x200, sp\n", "0 to 127 bytes");
    fails(" ld23.w 0x400000[r11], r12\n", "does not fit 23 bits");
    fails(" setfq r10\n", "unknown instruction `setfq`");
    fails(" .float 1\n", "floating-point constants are not supported");
}

#[test]
fn prepare_lists_ignore_what_cannot_be_saved() {
    // r10 cannot be in the list: warned about and left out (page 541).
    let asm = assemble_dialect("rh850", CcRh, " prepare r10, r20, 4\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert!(
        asm.diags
            .render(&asm.sm, false)
            .contains("`r10` cannot be in a register list")
    );
    let gnu = assemble_dialect("rh850", Gas, " prepare {r20}, 1\n");
    assert_eq!(
        asm.section_bytes(SectionId(0)),
        gnu.section_bytes(SectionId(0))
    );
}

/// Malformed CC-RH lines must produce diagnostics, never a panic.
const BAD: &[&str] = &[
    " mov",
    " mov ,",
    " mov #",
    " mov !",
    " mov $",
    " mov %",
    " mov 1, 2",
    " mov r1",
    " movea 1",
    " movea #, r0, r1",
    " add 0x12345",
    " addi 1, 2, 3",
    " and 1, r1, r2, r3",
    " ld.w",
    " ld.w [",
    " ld.w 4[",
    " ld.w 4[r1",
    " ld.w r1, r2",
    " st.w 4[r1], r2",
    " set1 3",
    " set1 3, r1, r2",
    " sld.b r1, r2",
    " push",
    " pushm",
    " popm 1",
    " prepare",
    " prepare r20",
    " prepare 0x1000, 4",
    " prepare r20, -1",
    " dispose",
    " dispose 4",
    " dispose r20, 4",
    " jmp",
    " jr",
    " jr32 lab",
    " jarl22 lab",
    " bz",
    " bz 1, 2",
    " bz9",
    " jnz17",
    " bsa 3",
    " setfgt",
    " cmovz",
    " cmovz 1, 2, 3",
    " cmpfeq.s",
    " cmov 1, 2",
    " mov HIGHW1(#)",
    " mov LOWW(",
    " movhi HIGHW1(#x",
    " .dshw",
    " .dshw x",
    " .align 4,",
    " .public",
    " .public x,",
    " .extern",
    "$NOMACRO x",
    "$REG_MODE",
    "a .macro\n .local\n.endm\n a",
    "a .macro x\n x~\n.endm\n a 1",
    "~",
    "!",
];

#[test]
fn malformed_lines_never_panic() {
    for src in BAD {
        let _ = ccrh(src);
    }
}

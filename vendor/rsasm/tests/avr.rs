//! AVR encoding and object tests.
//!
//! Every expected byte, relocation and flag here was produced by GNU binutils
//! 2.47's `avr-elf-as` (read back with `avr-elf-readelf` and `avr-elf-objdump`),
//! the reference behind `tools/xas-diff/run.sh avr avr51 avrxmega avrtiny`. The
//! `corpus_*` tables are an even spread of those corpora with the reference's
//! bytes beside each line. None of the expectations came from rsasm; where
//! rsasm refuses what the reference accepts with a warning, the test says so.

#![cfg(feature = "avr")]

mod common;
use common::*;
use rsasm::arch;

/// Checks a whole table for `arch`, reporting every mismatch rather than the
/// first.
#[track_caller]
fn check(arch: &str, cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (src, want) in cases {
        let got = match try_text_for(arch, src) {
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

/// The object `arch` makes of `src`, checked to have assembled.
#[track_caller]
fn object(arch: &str, src: &str) -> rsasm::assembler::Assembler {
    let asm = assemble_for(arch, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm
}

/// `e_flags` of the ELF object `arch` makes of `src`.
#[track_caller]
fn e_flags(arch: &str, src: &str) -> u32 {
    let elf = rsasm::output::elf::build(&object(arch, src)).expect("ELF output");
    u32::from_le_bytes(elf[0x24..0x28].try_into().unwrap())
}

/// The relocations of `src`, as (section, offset, type, symbol, addend).
#[track_caller]
fn relocs(arch: &str, src: &str) -> Vec<(String, u64, u32, String, i64)> {
    let asm = object(arch, src);
    asm.relocs
        .iter()
        .map(|r| {
            let section = asm.interner.get(asm.section(r.section).name).to_string();
            let symbol = r
                .symbol
                .map(|s| asm.interner.get(asm.symbols.get(s).name).to_string())
                .unwrap_or_default();
            (section, r.offset, r.kind, symbol, r.addend)
        })
        .collect()
}

#[track_caller]
fn refused(arch: &str, src: &str, why: &str) {
    let e = errors_for(arch, src);
    assert!(e.contains(why), "`{src}`: expected `{why}` in:\n{e}");
}

// ---- the target -----------------------------------------------------------

#[test]
fn target_properties_match_the_reference_objects() {
    let a = arch::lookup("avr").expect("avr backend");
    // `avr-elf-readelf -h`: "Class: ELF32", "Machine: Atmel AVR 8-bit
    // microcontroller".
    assert_eq!(a.elf_machine(), 83);
    assert_eq!(a.pointer_bytes(&a.initial_state()), 4);
    // `nop` is `0000`, and so is the padding of `.balign` in `.text`.
    assert_eq!(text_for("avr", "nop"), vec![0, 0]);
    // The section's end is padded to its alignment too.
    assert_eq!(
        hex(&text_for("avr", "nop\n.balign 4\nret")),
        "00 00 00 00 08 95 00 00"
    );
    // `.align` counts powers of two.
    assert_eq!(
        hex(&text_for("avr", "ret\n.align 2\nret")),
        "08 95 00 00 08 95 00 00"
    );
    // `.byte`, `.word` and `.long` of an undefined symbol, and `.long sym - .`.
    assert_eq!(a.data_reloc(1, false), Some(26)); // R_AVR_8
    assert_eq!(a.data_reloc(2, false), Some(4)); // R_AVR_16
    assert_eq!(a.data_reloc(4, false), Some(1)); // R_AVR_32
    assert_eq!(a.data_reloc(4, true), Some(36)); // R_AVR_32_PCREL
    assert_eq!(a.data_reloc(2, true), None);
    for name in [
        "AVR",
        "avr5",
        "avrxmega7",
        "avrtiny",
        "atmega328p",
        "ATtiny85",
        "at90s1200",
    ] {
        assert!(arch::lookup(name).is_some(), "{name}");
    }
    assert!(arch::lookup("avr7").is_none());
}

#[test]
fn e_flags_carry_the_core_and_the_relaxation_flag() {
    // `avr-elf-readelf -h` on `nop` assembled with each `-mmcu`.
    for (arch, want) in [
        ("avr", 0x82),
        ("avr1", 0x81),
        ("avr2", 0x82),
        ("avr25", 0x99),
        ("avr3", 0x83),
        ("avr31", 0x9f),
        ("avr35", 0xa3),
        ("avr4", 0x84),
        ("avr5", 0x85),
        ("avr51", 0xb3),
        ("avr6", 0x86),
        ("avrxmega2", 0xe6),
        ("avrxmega3", 0xe7),
        ("avrxmega4", 0xe8),
        ("avrxmega5", 0xe9),
        ("avrxmega6", 0xea),
        ("avrxmega7", 0xeb),
        ("avrtiny", 0xe4),
        ("atmega328p", 0x85),
        ("attiny85", 0x99),
        ("atxmega128a1u", 0xeb),
        ("at90s1200", 0x81),
    ] {
        assert_eq!(e_flags(arch, "nop"), want, "{arch}");
    }
    // And with `.arch`, which `avr-elf-as` takes from the default core.
    assert_eq!(e_flags("avr", ".arch avr5\nnop"), 0x85);
}

#[test]
fn each_core_has_the_instructions_of_its_set() {
    // What `avr-elf-as -mmcu=<core>` refuses ("illegal opcode `jmp' for mcu
    // avr2", "addressing mode not supported", ...).
    refused("avr", "jmp 0", "not available on avr");
    refused("avr", "movw r0, r2", "not available on avr");
    refused("avr", "mul r0, r1", "not available on avr");
    refused("avr", "break", "not available on avr");
    refused("avr", "lpm r0, z+", "no postincrementing");
    refused("avr1", "push r0", "not available on avr1");
    refused("avr1", "ld r0, X", "not available on avr1");
    refused("avr6", "des 1", "not available on avr6");
    refused("avrxmega7", "xch z, r0", "not available on avrxmega7");
    refused("avrtiny", "adiw r24, 1", "not available on avrtiny");
    refused("avrtiny", "mov r15, r16", "r16 to r31");
    // The row after a `?` form is taken without checking it again, so these
    // assemble where the plain `lpm` and `spm` exist.
    assert_eq!(hex(&text_for("avr", "lpm r0, z")), "04 90");
    assert_eq!(hex(&text_for("avr51", "spm z+")), "f8 95");
}

// ---- operands ---------------------------------------------------------------

#[test]
fn registers_are_names_numbers_or_constants() {
    check(
        "avr5",
        &[
            ("mov r0, r31", "0f 2e"),
            ("mov R16, R17", "01 2f"),
            ("mov 16, 17", "01 2f"),
            (".set n, 17\nmov n, 3", "13 2d"),
            ("mov xl, zh", "af 2f"),
            ("movw x, y", "de 01"),
            ("adiw z, 3", "33 96"),
            ("ld r0, x+", "0d 90"),
            ("st -Y, r1", "1a 92"),
            ("ldd r2, z+1", "21 80"),
        ],
    );
    refused("avr5", "ldi r15, 1", "r16 to r31");
    refused("avr5", "movw r1, r2", "odd");
    refused("avr5", "adiw r25, 1", "r24, r26, r28 or r30");
    refused("avr5", "fmul r24, r16", "r16 to r23");
    refused("avr5", "mov 32, r0", "out of range (0 to 31)");
    refused("avr5", "mov r0, sym", "must be a constant");
    refused("avr5", "ldd r0, X+1", "`y` or `z`");
    refused("avr5", "ld r0, -X+", "both predecrement and postincrement");
    refused("avr5", "lpm r0, -Z", "predecrement");
}

#[test]
fn ldi_modifiers_pick_the_byte_and_the_relocation() {
    // `avr-elf-as -mmcu=avr51`, then `avr-elf-readelf -rW`.
    let src = "\t.text\nstart:\n\
               \tldi r16, lo8(ext)\n\
               \tldi r17, hi8(ext+2)\n\
               \tldi r18, hh8(ext)\n\
               \tldi r19, pm_lo8(ext)\n\
               \tldi r20, lo8(gs(ext))\n\
               \tldi r21, lo8(-(ext))\n\
               \tldi r22, ext\n\
               \tin r0, ext\n\
               \tcbi ext, 1\n\
               \tadiw r24, ext\n\
               \tldd r0, Y+ext\n\
               \tlds r0, ext\n\
               \trjmp start\n\
               \tbreq start\n\
               \trcall ext\n\
               \t.data\n\
               \t.byte ext, lo8(ext), hi8(ext), hlo8(ext)\n\
               \t.word ext, pm(ext), gs(ext+2)\n\
               \t.long ext, ext - .\n";
    let t = |o: u64, k: u32, s: &str, a: i64| (".text".to_string(), o, k, s.to_string(), a);
    let d = |o: u64, k: u32, a: i64| (".data".to_string(), o, k, "ext".to_string(), a);
    assert_eq!(
        relocs("avr51", src),
        vec![
            t(0x00, 6, "ext", 0),   // R_AVR_LO8_LDI
            t(0x02, 7, "ext", 2),   // R_AVR_HI8_LDI
            t(0x04, 8, "ext", 0),   // R_AVR_HH8_LDI
            t(0x06, 12, "ext", 0),  // R_AVR_LO8_LDI_PM
            t(0x08, 24, "ext", 0),  // R_AVR_LO8_LDI_GS
            t(0x0a, 9, "ext", 0),   // R_AVR_LO8_LDI_NEG
            t(0x0c, 19, "ext", 0),  // R_AVR_LDI
            t(0x0e, 34, "ext", 0),  // R_AVR_PORT6
            t(0x10, 35, "ext", 0),  // R_AVR_PORT5
            t(0x12, 21, "ext", 0),  // R_AVR_6_ADIW
            t(0x14, 20, "ext", 0),  // R_AVR_6
            t(0x18, 4, "ext", 0),   // R_AVR_16, the second word of `lds`
            t(0x1a, 3, "start", 0), // R_AVR_13_PCREL, against the local label
            t(0x1c, 2, "start", 0), // R_AVR_7_PCREL
            t(0x1e, 3, "ext", 0),   // R_AVR_13_PCREL
            d(0x00, 26, 0),         // R_AVR_8
            d(0x01, 27, 0),         // R_AVR_8_LO8
            d(0x02, 28, 0),         // R_AVR_8_HI8
            d(0x03, 29, 0),         // R_AVR_8_HLO8
            d(0x04, 4, 0),          // R_AVR_16
            d(0x06, 5, 0),          // R_AVR_16_PM
            d(0x08, 5, 2),          // R_AVR_16_PM
            d(0x0a, 1, 0),          // R_AVR_32
            d(0x0e, 36, 0),         // R_AVR_32_PCREL
        ]
    );
    // `avr-elf-objdump -s`: the fields under every relocation are zero, and
    // the rest is the opcodes.
    let asm = object("avr51", src);
    assert_eq!(
        hex(&section(&asm, ".text")),
        "00 e0 10 e0 20 e0 30 e0 40 e0 50 e0 60 e0 00 b0 01 98 00 96 08 80 00 90 00 00 00 c0 \
         01 f0 00 d0"
    );
}

#[test]
fn call_and_the_avr_tiny_load_and_store_have_their_own_relocations() {
    assert_eq!(
        relocs("avr5", "call ext\njmp ext+4"),
        vec![
            (".text".to_string(), 0, 18, "ext".to_string(), 0), // R_AVR_CALL
            (".text".to_string(), 4, 18, "ext".to_string(), 4),
        ]
    );
    assert_eq!(
        relocs("avrtiny", "lds r16, ext\nsts ext, r17"),
        vec![
            (".text".to_string(), 0, 33, "ext".to_string(), 0), // R_AVR_LDS_STS_16
            (".text".to_string(), 2, 33, "ext".to_string(), 0),
        ]
    );
    assert_eq!(
        hex(&text_for("avrtiny", "lds r16, ext\nsts ext, r17")),
        "00 a0 10 a8"
    );
}

#[test]
fn ldi_modifiers_on_numbers_are_worked_out() {
    // `avr-elf-as` resolves every one of these without a relocation.
    check(
        "avr",
        &[
            ("ldi r20, lo8(0x1234)", "44 e3"),
            ("ldi r20, hi8(0x1234)", "42 e1"),
            ("ldi r20, hh8(0x123456)", "42 e1"),
            ("ldi r20, hhi8(0x12345678)", "42 e1"),
            ("ldi r20, pm_lo8(0x1234)", "4a e1"),
            ("ldi r20, lo8(-(0x1234))", "4c ec"),
            ("ldi r20, lo8(-(pm(0x1234)))", "46 ee"),
            ("ldi r20, -1", "4f ef"),
            ("ldi r20, 'A'", "41 e4"),
        ],
    );
    // Case matters to `avr_ldi_expression`, and a modifier is the whole
    // operand.
    refused("avr", "ldi r16, LO8(0x10)", "unexpected tokens");
    refused("avr", "ldi r16, lo8(1) + 1", "not closed");
    refused("avr", "ldi r16, pm_lo8(pm(1))", "cannot go inside it");
    refused("avr", "ldi r16, 256", "out of range (-255 to 255)");
}

#[test]
fn data_modifiers_are_calls_around_the_whole_value() {
    // `avr-elf-objdump -s -j .data`: a modifier's name is read in any case,
    // and only where a `(` follows it.
    let asm = object(
        "avr",
        "\t.data\n\
         \t.byte lo8(0x123456), hi8(0x123456), hlo8(0x123456), hh8(0x123456)\n\
         \t.word pm(0x1234), gs(0x2000)\n\
         \t.byte LO8(0x1234), Hi8(0x1234)\n\
         \t.word PM(0x10)\n",
    );
    assert_eq!(
        hex(&section(&asm, ".data")),
        "56 34 12 12 1a 09 00 10 34 12 08 00"
    );
    let r = relocs("avr", ".globl pm\n.word pm\npm: nop");
    assert_eq!(r[0].2, 4, "a symbol named `pm` is still a symbol");
    refused("avr", ".byte pm(ext)", "in a 1-byte data field");
    refused("avr", ".word lo8(ext)", "in a 2-byte data field");
}

// ---- linker relaxation --------------------------------------------------------

#[test]
fn branches_to_a_label_are_left_to_the_linker() {
    // `avr-elf-readelf -rW`: every branch is relocated, the field zero, even
    // to the next instruction and in a section that is not code.
    let r = relocs(
        "avr",
        "\t.text\n1: rjmp 1b\nbreq 2f\n2: nop\n\t.section .rodata\nrcall 3f\n3: nop",
    );
    let kinds: Vec<(String, u64, u32, i64)> =
        r.iter().map(|x| (x.0.clone(), x.1, x.2, x.4)).collect();
    assert_eq!(
        kinds,
        vec![
            (".text".to_string(), 0, 3, 0),
            (".text".to_string(), 2, 2, 0),
            (".rodata".to_string(), 0, 3, 0),
        ]
    );
    assert_eq!(
        hex(&text_for("avr", "1: rjmp 1b\nbreq 2f\n2: nop")),
        "00 c0 01 f0 00 00"
    );
    // A number is resolved, from the section's start.
    assert_eq!(hex(&text_for("avr", "nop\nrjmp 0")), "00 00 fe cf");
}

#[test]
fn rjmp_and_rcall_wrap_around_on_a_small_device() {
    // `avr-elf-as` keeps the low 12 bits of a displacement to a number on a
    // device with no more than 8K, and refuses it on a larger one ("operand
    // out of range: 4096").
    check(
        "avr",
        &[("rjmp 0xe042", "20 c0"), ("rcall 0x865a", "2c d3")],
    );
    refused("avr51", "rjmp 0x2002", "out of range (-4096 to 4095)");
    // To a label, in a flat image, GNU ld wraps for the avr2, avr25 and avr4
    // machines only.
    let src = "\trcall far\n\t.space 0x1000\nfar: ret\n";
    assert!(!assemble_flat_for("avr", src, 0).diags.has_errors());
    assert!(assemble_flat_for("avrtiny", src, 0).diags.has_errors());
}

#[test]
fn the_location_counter_in_an_operand_is_past_the_instruction() {
    // GNU as reserves an instruction's bytes before it reads its operands, so
    // `rjmp .` jumps to the next instruction: `avr-elf-readelf -rW` names
    // `.text+2` for the first of these, `.text+6` for the second.
    let r = relocs("avr", "rjmp .\nrjmp . + 2");
    assert_eq!(r[0].1, 0);
    assert_eq!(r[1].1, 2);
    let asm = object("avr", "rjmp .\nrjmp . + 2");
    let value = |i: usize| {
        let sym = asm.symbols.get(asm.relocs[i].symbol.expect("a symbol"));
        let rsasm::symbol::SymbolValue::Label { section, frag } = sym.value else {
            panic!("not a label");
        };
        let s = asm.section(section);
        let at = s.frags.get(frag as usize).map_or(s.size, |f| f.offset);
        at as i64 + asm.relocs[i].addend
    };
    assert_eq!((value(0), value(1)), (2, 6));
}

#[test]
fn alignment_and_org_in_code_are_recorded_for_the_linker() {
    // `avr-elf-objdump -s -j .avr.prop` and `avr-elf-readelf -rW`.
    let asm = object(
        "avr",
        "\t.text\n\tnop\n\t.balign 4\n\tnop\n\t.balign 4, 0x55\n\t.org 0x10\n\tnop\n\t.org 0x18, 0xaa\n\tret\n",
    );
    assert_eq!(
        hex(&section(&asm, ".avr.prop")),
        "01 00 05 00 00 00 00 00 02 02 00 00 00 00 00 00 00 03 02 00 00 00 55 00 00 00 \
         00 00 00 00 00 00 00 00 00 01 aa ff ff ff 00 00 00 00 02 02 00 00 00"
    );
    let addends: Vec<(u64, i64)> = asm
        .relocs
        .iter()
        .filter(|r| asm.interner.get(asm.section(r.section).name) == ".avr.prop")
        .map(|r| (r.offset, r.addend))
        .collect();
    assert_eq!(
        addends,
        vec![(4, 4), (0xd, 8), (0x1a, 0x10), (0x1f, 0x18), (0x28, 0x1c)]
    );
    // Code with no alignment has no such section.
    let plain = object("avr", "nop");
    assert!(
        plain
            .sections
            .iter()
            .all(|s| plain.interner.get(s.name) != ".avr.prop")
    );
}

// ---- lexing -------------------------------------------------------------------

#[test]
fn dollar_separates_statements_and_semicolon_comments() {
    assert_eq!(
        hex(&text_for(
            "avr",
            "ldi r16, 1 $ ldi r17, 2 ; comment\n# line comment\nret$nop"
        )),
        "01 e0 12 e0 08 95 00 00"
    );
}

// ---- where rsasm differs on purpose ---------------------------------------------

#[test]
fn values_the_reference_truncates_with_a_warning_are_refused() {
    // `avr-elf-as` assembles each with "Warning: constant out of 8-bit range"
    // or "operand out of range" and keeps the low bits.
    refused("avr", "ldi r16, -256", "out of range (-255 to 255)");
    refused("avrtiny", "lds r16, 0x20", "out of range (64 to 191)");
    refused("avrtiny", "sts 0xc0, r16", "out of range (64 to 191)");
    // `avr-elf-as` writes this one without a word, keeping 22 bits.
    refused("avr51", "call 0x800000", "out of range");
    // A displacement is not optional. GNU as looks one character past the
    // base register for its `+`, so it refuses `ldd r0, Y` with "garbage at
    // end of line" — except on the last line of a file, where the character
    // it reads is past the end of the source.
    refused("avr5", "ldd r0, Y\nnop", "displacement");
    // GNU as does not check a number counted in words; GNU ld refuses an odd
    // label for these, and rsasm refuses both.
    refused("avr", "ldi r16, pm_lo8(3)", "not a multiple of 2");
    refused("avr", ".word pm(3)", "not a multiple of 2");
}
#[test]
fn corpus_avr_no_operands() {
    check(
        "avr",
        &[
            ("clc", "88 94"),
            ("clh", "d8 94"),
            ("cli", "f8 94"),
            ("cln", "a8 94"),
            ("cls", "c8 94"),
            ("clt", "e8 94"),
            ("clv", "b8 94"),
            ("clz", "98 94"),
            ("sec", "08 94"),
            ("seh", "58 94"),
            ("sei", "78 94"),
            ("sen", "28 94"),
            ("ses", "48 94"),
            ("set", "68 94"),
            ("sev", "38 94"),
            ("sez", "18 94"),
            ("icall", "09 95"),
            ("ijmp", "09 94"),
            ("nop", "00 00"),
            ("ret", "08 95"),
            ("reti", "18 95"),
            ("sleep", "88 95"),
            ("wdr", "a8 95"),
        ],
    );
}

#[test]
fn corpus_avr_sreg_bit_set_and_clear() {
    check(
        "avr",
        &[
            ("bclr 0", "88 94"),
            ("bclr 1", "98 94"),
            ("bclr 5", "d8 94"),
            ("bclr 7", "f8 94"),
            ("bset 0", "08 94"),
            ("bset 1", "18 94"),
            ("bset 5", "58 94"),
            ("bset 7", "78 94"),
        ],
    );
}

#[test]
fn corpus_avr_program_memory_load() {
    check(
        "avr",
        &[
            ("lpm", "c8 95"),
            ("lpm r0, Z", "04 90"),
            ("lpm r0, z", "04 90"),
            ("lpm r5, Z", "54 90"),
            ("lpm r5, z", "54 90"),
            ("lpm r16, Z", "04 91"),
            ("lpm r16, z", "04 91"),
            ("lpm r31, Z", "f4 91"),
            ("lpm r31, z", "f4 91"),
        ],
    );
}

#[test]
fn corpus_avr_two_registers() {
    check(
        "avr",
        &[
            ("adc r0, r5", "05 1c"),
            ("adc r23, r5", "75 1d"),
            ("adc r5, r0", "50 1c"),
            ("adc r5, r24", "58 1e"),
            ("adc R16, R17", "01 1f"),
            ("add r16, r5", "05 0d"),
            ("add r30, r5", "e5 0d"),
            ("add r5, r17", "51 0e"),
            ("add r5, r31", "5f 0e"),
            ("and r1, r5", "15 20"),
            ("and r25, r5", "95 21"),
            ("and r5, r7", "57 20"),
            ("and r5, r26", "5a 22"),
            ("and xl, zh", "af 23"),
            ("cp r23, r5", "75 15"),
            ("cp r5, r0", "50 14"),
            ("cp r5, r24", "58 16"),
            ("cp r31,r31", "ff 17"),
            ("cpc r15, r5", "f5 04"),
            ("cpc r29, r5", "d5 05"),
            ("cpc r5, r16", "50 06"),
            ("cpc r5, r30", "5e 06"),
            ("cpse r1, r5", "15 10"),
            ("cpse r25, r5", "95 11"),
            ("cpse r5, r7", "57 10"),
            ("cpse r5, r25", "59 12"),
            ("cpse 3, 4", "34 10"),
            ("eor r17, r5", "15 25"),
            ("eor r31, r5", "f5 25"),
            ("eor r5, r23", "57 26"),
            ("eor r31,r31", "ff 27"),
            ("mov r15, r5", "f5 2c"),
            ("mov r29, r5", "d5 2d"),
            ("mov r5, r15", "5f 2c"),
            ("mov r5, r29", "5d 2e"),
            ("or r0, r5", "05 28"),
            ("or r24, r5", "85 29"),
            ("or r5, r1", "51 28"),
            ("or r5, r25", "59 2a"),
            ("or 3, 4", "34 28"),
            ("sbc r17, r5", "15 09"),
            ("sbc r30, r5", "e5 09"),
            ("sbc r5, r17", "51 0a"),
            ("sbc r5, r31", "5f 0a"),
            ("sub r7, r5", "75 18"),
            ("sub r26, r5", "a5 19"),
            ("sub r5, r15", "5f 18"),
            ("sub r5, r29", "5d 1a"),
        ],
    );
}

#[test]
fn corpus_avr_one_register_standing_for_two() {
    check(
        "avr",
        &[
            ("clr r0", "00 24"),
            ("clr r1", "11 24"),
            ("clr r7", "77 24"),
            ("clr r15", "ff 24"),
            ("clr r17", "11 27"),
            ("clr r23", "77 27"),
            ("clr r24", "88 27"),
            ("clr r25", "99 27"),
            ("clr r29", "dd 27"),
            ("clr r30", "ee 27"),
            ("clr r31", "ff 27"),
            ("clr yl", "cc 27"),
            ("lsl r0", "00 0c"),
            ("lsl r1", "11 0c"),
            ("lsl r7", "77 0c"),
            ("lsl r15", "ff 0c"),
            ("lsl r17", "11 0f"),
            ("lsl r23", "77 0f"),
            ("lsl r24", "88 0f"),
            ("lsl r25", "99 0f"),
            ("lsl r29", "dd 0f"),
            ("lsl r30", "ee 0f"),
            ("lsl r31", "ff 0f"),
            ("lsl yl", "cc 0f"),
            ("rol r0", "00 1c"),
            ("rol r1", "11 1c"),
            ("rol r7", "77 1c"),
            ("rol r15", "ff 1c"),
            ("rol r17", "11 1f"),
            ("rol r23", "77 1f"),
            ("rol r24", "88 1f"),
            ("rol r25", "99 1f"),
            ("rol r29", "dd 1f"),
            ("rol r30", "ee 1f"),
            ("rol r31", "ff 1f"),
            ("rol yl", "cc 1f"),
            ("tst r0", "00 20"),
            ("tst r1", "11 20"),
            ("tst r7", "77 20"),
            ("tst r15", "ff 20"),
            ("tst r17", "11 23"),
            ("tst r23", "77 23"),
            ("tst r24", "88 23"),
            ("tst r25", "99 23"),
            ("tst r29", "dd 23"),
            ("tst r30", "ee 23"),
            ("tst r31", "ff 23"),
            ("tst yl", "cc 23"),
        ],
    );
}

#[test]
fn corpus_avr_8_bit_immediates_with_and_without_modifiers() {
    check(
        "avr",
        &[
            ("andi r16, 0x55", "05 75"),
            ("andi r20, 1", "41 70"),
            ("andi r20, 0xff", "4f 7f"),
            ("andi r20, lo8(0x1234)", "44 73"),
            ("andi r20, pm_hi8(0x1234)", "49 70"),
            ("andi r20, lo8(pm(0x1234))", "4a 71"),
            ("andi r20, lo8(-(pm(0x1234)))", "46 7e"),
            ("ldi r16, 0x55", "05 e5"),
            ("ldi r20, 0x0f", "4f e0"),
            ("ldi r20, 255", "4f ef"),
            ("ldi r20, hi8(0x1234)", "42 e1"),
            ("ldi r20, pm_hh8(0x123456)", "49 e0"),
            ("ldi r20, hi8(pm(0x1234))", "49 e0"),
            ("ldi r20, hi8(-(pm(0x1234)))", "46 ef"),
            ("ori r17, 0x55", "15 65"),
            ("ori r20, 0x10", "40 61"),
            ("ori r20, -1", "4f 6f"),
            ("ori r20, hh8(0x123456)", "42 61"),
            ("ori r20, lo8(-(0x1234))", "4c 6c"),
            ("ori r20, hh8(pm(0x123456))", "49 60"),
            ("ori r20, hh8(-(pm(0x123456)))", "46 6f"),
            ("sbr r24, 0x55", "85 65"),
            ("sbr r20, 0x7f", "4f 67"),
            ("sbr r20, -128", "40 68"),
            ("sbr r20, hlo8(0x123456)", "42 61"),
            ("sbr r20, hi8(-(0x1234))", "4d 6e"),
            ("sbr r20, pm_lo8(-(0x1234))", "46 6e"),
            ("sbr r20, lo8(0x12 + 3)", "45 61"),
            ("cpi r31, 0x55", "f5 35"),
            ("cpi r20, 0x80", "40 38"),
            ("cpi r20, -255", "41 30"),
            ("cpi r20, hhi8(0x12345678)", "42 31"),
            ("cpi r20, hh8(-(0x123456))", "4d 3e"),
            ("cpi r20, pm_hi8(-(0x1234))", "46 3f"),
            ("cpi r20, (1 << 4) | 3", "43 31"),
            ("sbci r20, 0", "40 40"),
            ("sbci r20, 0xf0", "40 4f"),
            ("sbci r20, 'A'", "41 44"),
            ("sbci r20, pm_lo8(0x1234)", "4a 41"),
            ("sbci r20, hhi8(-(0x12345678))", "4d 4e"),
            ("sbci r20, pm_hh8(-(0x123456))", "46 4f"),
            ("sbci r20, ~0 & 0xff", "4f 4f"),
            ("subi r20, 1", "41 50"),
            ("subi r20, 0xff", "4f 5f"),
            ("subi r20, lo8(0x1234)", "44 53"),
            ("subi r20, pm_hi8(0x1234)", "49 50"),
            ("subi r20, lo8(pm(0x1234))", "4a 51"),
            ("subi r20, lo8(-(pm(0x1234)))", "46 5e"),
        ],
    );
}

#[test]
fn corpus_avr_cbr_a_complemented_andi() {
    check(
        "avr",
        &[
            ("cbr r16, 0x55", "0a 7a"),
            ("cbr r17, 0x55", "1a 7a"),
            ("cbr r24, 0x55", "8a 7a"),
            ("cbr r31, 0x55", "fa 7a"),
            ("cbr r20, 0", "4f 7f"),
            ("cbr r20, 1", "4e 7f"),
            ("cbr r20, 0xf0", "4f 70"),
            ("cbr r20, 0xff", "40 70"),
            ("cbr r20, 7", "48 7f"),
        ],
    );
}

#[test]
fn corpus_avr_ser() {
    check(
        "avr",
        &[
            ("ser r16", "0f ef"),
            ("ser r17", "1f ef"),
            ("ser r24", "8f ef"),
            ("ser r31", "ff ef"),
        ],
    );
}

#[test]
fn corpus_avr_register_bits() {
    check(
        "avr",
        &[
            ("sbrc r0, 0", "00 fc"),
            ("sbrc r0, 1", "01 fc"),
            ("sbrc r0, 5", "05 fc"),
            ("sbrc r0, 7", "07 fc"),
            ("sbrc r16, 0", "00 fd"),
            ("sbrc r16, 1", "01 fd"),
            ("sbrc r16, 5", "05 fd"),
            ("sbrc r16, 7", "07 fd"),
            ("sbrc r31, 0", "f0 fd"),
            ("sbrc r31, 1", "f1 fd"),
            ("sbrc r31, 5", "f5 fd"),
            ("sbrc r31, 7", "f7 fd"),
            ("sbrs r0, 0", "00 fe"),
            ("sbrs r0, 1", "01 fe"),
            ("sbrs r0, 5", "05 fe"),
            ("sbrs r0, 7", "07 fe"),
            ("sbrs r16, 0", "00 ff"),
            ("sbrs r16, 1", "01 ff"),
            ("sbrs r16, 5", "05 ff"),
            ("sbrs r16, 7", "07 ff"),
            ("sbrs r31, 0", "f0 ff"),
            ("sbrs r31, 1", "f1 ff"),
            ("sbrs r31, 5", "f5 ff"),
            ("sbrs r31, 7", "f7 ff"),
            ("bld r0, 0", "00 f8"),
            ("bld r0, 1", "01 f8"),
            ("bld r0, 5", "05 f8"),
            ("bld r0, 7", "07 f8"),
            ("bld r16, 0", "00 f9"),
            ("bld r16, 1", "01 f9"),
            ("bld r16, 5", "05 f9"),
            ("bld r16, 7", "07 f9"),
            ("bld r31, 0", "f0 f9"),
            ("bld r31, 1", "f1 f9"),
            ("bld r31, 5", "f5 f9"),
            ("bld r31, 7", "f7 f9"),
            ("bst r0, 0", "00 fa"),
            ("bst r0, 1", "01 fa"),
            ("bst r0, 5", "05 fa"),
            ("bst r0, 7", "07 fa"),
            ("bst r16, 0", "00 fb"),
            ("bst r16, 1", "01 fb"),
            ("bst r16, 5", "05 fb"),
            ("bst r16, 7", "07 fb"),
            ("bst r31, 0", "f0 fb"),
            ("bst r31, 1", "f1 fb"),
            ("bst r31, 5", "f5 fb"),
            ("bst r31, 7", "f7 fb"),
        ],
    );
}

#[test]
fn corpus_avr_i_o_ports() {
    check(
        "avr",
        &[
            ("in r0, 0", "00 b0"),
            ("in r0, 1", "01 b0"),
            ("in r0, 0x1f", "0f b2"),
            ("in r0, 0x20", "00 b4"),
            ("in r0, 0x3f", "0f b6"),
            ("in r16, 0", "00 b1"),
            ("in r16, 1", "01 b1"),
            ("in r16, 0x1f", "0f b3"),
            ("in r16, 0x20", "00 b5"),
            ("in r16, 0x3f", "0f b7"),
            ("in r31, 0", "f0 b1"),
            ("in r31, 1", "f1 b1"),
            ("in r31, 0x1f", "ff b3"),
            ("in r31, 0x20", "f0 b5"),
            ("in r31, 0x3f", "ff b7"),
            ("out 0, r0", "00 b8"),
            ("out 1, r0", "01 b8"),
            ("out 0x1f, r0", "0f ba"),
            ("out 0x20, r0", "00 bc"),
            ("out 0x3f, r0", "0f be"),
            ("out 0, r16", "00 b9"),
            ("out 1, r16", "01 b9"),
            ("out 0x1f, r16", "0f bb"),
            ("out 0x20, r16", "00 bd"),
            ("out 0x3f, r16", "0f bf"),
            ("out 0, r31", "f0 b9"),
            ("out 1, r31", "f1 b9"),
            ("out 0x1f, r31", "ff bb"),
            ("out 0x20, r31", "f0 bd"),
            ("out 0x3f, r31", "ff bf"),
        ],
    );
}

#[test]
fn corpus_avr_16_bit_add_and_subtract_on_a_register_pair() {
    check(
        "avr",
        &[
            ("adiw r24, 0", "00 96"),
            ("adiw r24, 1", "01 96"),
            ("adiw r24, 0x20", "80 96"),
            ("adiw r24, 63", "cf 96"),
            ("adiw r26, 0", "10 96"),
            ("adiw r26, 1", "11 96"),
            ("adiw r26, 63", "df 96"),
            ("adiw r28, 0", "20 96"),
            ("adiw r28, 1", "21 96"),
            ("adiw r28, 0x20", "a0 96"),
            ("adiw r28, 63", "ef 96"),
            ("adiw r30, 0", "30 96"),
            ("adiw r30, 0x20", "b0 96"),
            ("adiw r30, 63", "ff 96"),
            ("adiw x, 0", "10 96"),
            ("adiw x, 1", "11 96"),
            ("adiw x, 0x20", "90 96"),
            ("adiw x, 63", "df 96"),
            ("adiw y, 1", "21 96"),
            ("adiw y, 0x20", "a0 96"),
            ("adiw y, 63", "ef 96"),
            ("adiw z, 0", "30 96"),
            ("adiw z, 1", "31 96"),
            ("adiw z, 0x20", "b0 96"),
            ("sbiw r24, 0", "00 97"),
            ("sbiw r24, 1", "01 97"),
            ("sbiw r24, 0x20", "80 97"),
            ("sbiw r24, 63", "cf 97"),
            ("sbiw r26, 0", "10 97"),
            ("sbiw r26, 1", "11 97"),
            ("sbiw r26, 63", "df 97"),
            ("sbiw r28, 0", "20 97"),
            ("sbiw r28, 1", "21 97"),
            ("sbiw r28, 0x20", "a0 97"),
            ("sbiw r28, 63", "ef 97"),
            ("sbiw r30, 0", "30 97"),
            ("sbiw r30, 0x20", "b0 97"),
            ("sbiw r30, 63", "ff 97"),
            ("sbiw x, 0", "10 97"),
            ("sbiw x, 1", "11 97"),
            ("sbiw x, 0x20", "90 97"),
            ("sbiw x, 63", "df 97"),
            ("sbiw y, 1", "21 97"),
            ("sbiw y, 0x20", "a0 97"),
            ("sbiw y, 63", "ef 97"),
            ("sbiw z, 0", "30 97"),
            ("sbiw z, 1", "31 97"),
            ("sbiw z, 0x20", "b0 97"),
        ],
    );
}

#[test]
fn corpus_avr_i_o_bits() {
    check(
        "avr",
        &[
            ("cbi 0, 0", "00 98"),
            ("cbi 0, 1", "01 98"),
            ("cbi 0, 5", "05 98"),
            ("cbi 1, 0", "08 98"),
            ("cbi 1, 1", "09 98"),
            ("cbi 1, 5", "0d 98"),
            ("cbi 0x10, 0", "80 98"),
            ("cbi 0x10, 1", "81 98"),
            ("cbi 0x10, 5", "85 98"),
            ("cbi 0x1f, 0", "f8 98"),
            ("cbi 0x1f, 1", "f9 98"),
            ("cbi 0x1f, 5", "fd 98"),
            ("sbi 0, 0", "00 9a"),
            ("sbi 0, 1", "01 9a"),
            ("sbi 0, 5", "05 9a"),
            ("sbi 1, 0", "08 9a"),
            ("sbi 1, 1", "09 9a"),
            ("sbi 1, 5", "0d 9a"),
            ("sbi 0x10, 0", "80 9a"),
            ("sbi 0x10, 1", "81 9a"),
            ("sbi 0x10, 5", "85 9a"),
            ("sbi 0x1f, 0", "f8 9a"),
            ("sbi 0x1f, 1", "f9 9a"),
            ("sbi 0x1f, 5", "fd 9a"),
            ("sbic 0, 0", "00 99"),
            ("sbic 0, 1", "01 99"),
            ("sbic 0, 5", "05 99"),
            ("sbic 1, 0", "08 99"),
            ("sbic 1, 1", "09 99"),
            ("sbic 1, 5", "0d 99"),
            ("sbic 0x10, 0", "80 99"),
            ("sbic 0x10, 1", "81 99"),
            ("sbic 0x10, 5", "85 99"),
            ("sbic 0x1f, 0", "f8 99"),
            ("sbic 0x1f, 1", "f9 99"),
            ("sbic 0x1f, 5", "fd 99"),
            ("sbis 0, 0", "00 9b"),
            ("sbis 0, 1", "01 9b"),
            ("sbis 0, 5", "05 9b"),
            ("sbis 1, 0", "08 9b"),
            ("sbis 1, 1", "09 9b"),
            ("sbis 1, 5", "0d 9b"),
            ("sbis 0x10, 0", "80 9b"),
            ("sbis 0x10, 1", "81 9b"),
            ("sbis 0x10, 5", "85 9b"),
            ("sbis 0x1f, 0", "f8 9b"),
            ("sbis 0x1f, 1", "f9 9b"),
            ("sbis 0x1f, 5", "fd 9b"),
        ],
    );
}

#[test]
fn corpus_avr_conditional_branches() {
    check(
        "avr",
        &[
            ("brcc 0", "f8 f7"),
            ("brcc -2", "f0 f7"),
            ("brcc 0x40", "f8 f4"),
            ("brcs 4", "08 f0"),
            ("brcs -126", "00 f2"),
            ("breq 2", "01 f0"),
            ("breq 128", "f9 f1"),
            ("brge 0", "fc f7"),
            ("brge 128", "fc f5"),
            ("brhc 0", "fd f7"),
            ("brhc -2", "f5 f7"),
            ("brhc 0x40", "fd f4"),
            ("brhs 4", "0d f0"),
            ("brhs -126", "05 f2"),
            ("brid 2", "07 f4"),
            ("brid 128", "ff f5"),
            ("brie 2", "07 f0"),
            ("brie 128", "ff f1"),
            ("brlo 0", "f8 f3"),
            ("brlo -2", "f0 f3"),
            ("brlo 0x40", "f8 f0"),
            ("brlt 4", "0c f0"),
            ("brlt -126", "04 f2"),
            ("brmi 2", "02 f0"),
            ("brmi -126", "02 f2"),
            ("brne 2", "01 f4"),
            ("brne 128", "f9 f5"),
            ("brpl 0", "fa f7"),
            ("brpl -2", "f2 f7"),
            ("brpl 0x40", "fa f4"),
            ("brsh 4", "08 f4"),
            ("brsh -126", "00 f6"),
            ("brtc 4", "0e f4"),
            ("brtc -126", "06 f6"),
            ("brts 2", "06 f0"),
            ("brts 128", "fe f1"),
            ("brvc 0", "fb f7"),
            ("brvc -2", "f3 f7"),
            ("brvc 0x40", "fb f4"),
            ("brvs 4", "0b f0"),
            ("brvs 0x40", "fb f0"),
            ("brbc 0, -2", "f0 f7"),
            ("brbc 1, -2", "f1 f7"),
            ("brbc 5, -2", "f5 f7"),
            ("brbc 7, -2", "f7 f7"),
            ("brbs 0, -2", "f0 f3"),
            ("brbs 1, -2", "f1 f3"),
            ("brbs 5, -2", "f5 f3"),
        ],
    );
}

#[test]
fn corpus_avr_relative_jump_and_call() {
    check(
        "avr",
        &[
            ("rcall 0", "ff df"),
            ("rcall 2", "00 d0"),
            ("rcall -2", "fe df"),
            ("rcall 4096", "ff d7"),
            ("rcall -4094", "00 d8"),
            ("rcall 0x100", "7f d0"),
            ("rjmp 0", "ff cf"),
            ("rjmp 2", "00 c0"),
            ("rjmp -2", "fe cf"),
            ("rjmp 4096", "ff c7"),
            ("rjmp -4094", "00 c8"),
            ("rjmp 0x100", "7f c0"),
        ],
    );
}

#[test]
fn corpus_avr_one_register() {
    check(
        "avr",
        &[
            ("asr r0", "05 94"),
            ("asr r7", "75 94"),
            ("asr r17", "15 95"),
            ("asr r25", "95 95"),
            ("asr r29", "d5 95"),
            ("com r0", "00 94"),
            ("com r15", "f0 94"),
            ("com r17", "10 95"),
            ("com r25", "90 95"),
            ("com r30", "e0 95"),
            ("dec r1", "1a 94"),
            ("dec r15", "fa 94"),
            ("dec r23", "7a 95"),
            ("dec r26", "aa 95"),
            ("dec r30", "ea 95"),
            ("inc r1", "13 94"),
            ("inc r16", "03 95"),
            ("inc r24", "83 95"),
            ("inc r26", "a3 95"),
            ("inc r31", "f3 95"),
            ("lsr r7", "76 94"),
            ("lsr r16", "06 95"),
            ("lsr r24", "86 95"),
            ("lsr r29", "d6 95"),
            ("neg r0", "01 94"),
            ("neg r7", "71 94"),
            ("neg r17", "11 95"),
            ("neg r25", "91 95"),
            ("neg r29", "d1 95"),
            ("pop r0", "0f 90"),
            ("pop r15", "ff 90"),
            ("pop r17", "1f 91"),
            ("pop r25", "9f 91"),
            ("pop r30", "ef 91"),
            ("push r1", "1f 92"),
            ("push r15", "ff 92"),
            ("push r23", "7f 93"),
            ("push r26", "af 93"),
            ("push r30", "ef 93"),
            ("ror r1", "17 94"),
            ("ror r16", "07 95"),
            ("ror r24", "87 95"),
            ("ror r26", "a7 95"),
            ("ror r31", "f7 95"),
            ("swap r7", "72 94"),
            ("swap r16", "02 95"),
            ("swap r24", "82 95"),
            ("swap r29", "d2 95"),
        ],
    );
}

#[test]
fn corpus_avr_direct_load_and_store() {
    check(
        "avr",
        &[
            ("sts 0x40, r20", "40 93 40 00"),
            ("sts 0x41, r20", "40 93 41 00"),
            ("sts 0x7f, r20", "40 93 7f 00"),
            ("sts 0x80, r20", "40 93 80 00"),
            ("sts 0xbf, r20", "40 93 bf 00"),
            ("sts 0, r0", "00 92 00 00"),
            ("sts 0, r16", "00 93 00 00"),
            ("sts 0, r31", "f0 93 00 00"),
            ("sts 0x60, r0", "00 92 60 00"),
            ("sts 0x60, r16", "00 93 60 00"),
            ("sts 0x60, r31", "f0 93 60 00"),
            ("sts 0x1234, r0", "00 92 34 12"),
            ("sts 0x1234, r16", "00 93 34 12"),
            ("sts 0x1234, r31", "f0 93 34 12"),
            ("sts 0xffff, r0", "00 92 ff ff"),
            ("sts 0xffff, r16", "00 93 ff ff"),
            ("sts 0xffff, r31", "f0 93 ff ff"),
            ("lds r20, 0x40", "40 91 40 00"),
            ("lds r20, 0x41", "40 91 41 00"),
            ("lds r20, 0x7f", "40 91 7f 00"),
            ("lds r20, 0x80", "40 91 80 00"),
            ("lds r20, 0xbf", "40 91 bf 00"),
            ("lds r0, 0", "00 90 00 00"),
            ("lds r16, 0", "00 91 00 00"),
            ("lds r31, 0", "f0 91 00 00"),
            ("lds r0, 0x60", "00 90 60 00"),
            ("lds r16, 0x60", "00 91 60 00"),
            ("lds r31, 0x60", "f0 91 60 00"),
            ("lds r0, 0x1234", "00 90 34 12"),
            ("lds r16, 0x1234", "00 91 34 12"),
            ("lds r31, 0x1234", "f0 91 34 12"),
            ("lds r0, 0xffff", "00 90 ff ff"),
            ("lds r16, 0xffff", "00 91 ff ff"),
            ("lds r31, 0xffff", "f0 91 ff ff"),
        ],
    );
}

#[test]
fn corpus_avr_load_and_store_with_displacement() {
    check(
        "avr",
        &[
            ("ldd r0, Y+0", "08 80"),
            ("ldd r0, Y+1", "09 80"),
            ("ldd r0, Y+63", "0f ac"),
            ("ldd r0, Z+0", "00 80"),
            ("ldd r0, Z+7", "07 80"),
            ("ldd r0, Z+0x28", "00 a4"),
            ("ldd r0, y+32", "08 a0"),
            ("ldd r0, z+(2*3)", "06 80"),
            ("ldd r16, Y+0", "08 81"),
            ("ldd r16, Y+1", "09 81"),
            ("ldd r16, Y+63", "0f ad"),
            ("ldd r16, Z+0", "00 81"),
            ("ldd r16, Z+7", "07 81"),
            ("ldd r16, Z+0x28", "00 a5"),
            ("ldd r16, y+32", "08 a1"),
            ("ldd r16, z+(2*3)", "06 81"),
            ("ldd r31, Y+0", "f8 81"),
            ("ldd r31, Y+1", "f9 81"),
            ("ldd r31, Y+63", "ff ad"),
            ("ldd r31, Z+0", "f0 81"),
            ("ldd r31, Z+7", "f7 81"),
            ("ldd r31, Z+0x28", "f0 a5"),
            ("ldd r31, y+32", "f8 a1"),
            ("ldd r31, z+(2*3)", "f6 81"),
            ("std Y+0, r0", "08 82"),
            ("std Y+1, r0", "09 82"),
            ("std Y+63, r0", "0f ae"),
            ("std Z+0, r0", "00 82"),
            ("std Z+7, r0", "07 82"),
            ("std Z+0x28, r0", "00 a6"),
            ("std y+32, r0", "08 a2"),
            ("std z+(2*3), r0", "06 82"),
            ("std Y+0, r16", "08 83"),
            ("std Y+1, r16", "09 83"),
            ("std Y+63, r16", "0f af"),
            ("std Z+0, r16", "00 83"),
            ("std Z+7, r16", "07 83"),
            ("std Z+0x28, r16", "00 a7"),
            ("std y+32, r16", "08 a3"),
            ("std z+(2*3), r16", "06 83"),
            ("std Y+0, r31", "f8 83"),
            ("std Y+1, r31", "f9 83"),
            ("std Y+63, r31", "ff af"),
            ("std Z+0, r31", "f0 83"),
            ("std Z+7, r31", "f7 83"),
            ("std Z+0x28, r31", "f0 a7"),
            ("std y+32, r31", "f8 a3"),
            ("std z+(2*3), r31", "f6 83"),
        ],
    );
}

#[test]
fn corpus_avr_load_and_store_through_a_pointer() {
    check(
        "avr",
        &[
            ("ld r0, X", "0c 90"),
            ("ld r0, -X", "0e 90"),
            ("ld r0, Y+", "09 90"),
            ("ld r0, Z", "00 80"),
            ("ld r0, -Z", "02 90"),
            ("ld r0, y+", "09 90"),
            ("ld r5, X", "5c 90"),
            ("ld r5, -X", "5e 90"),
            ("ld r5, Y+", "59 90"),
            ("ld r5, Z", "50 80"),
            ("ld r5, -Z", "52 90"),
            ("ld r5, y+", "59 90"),
            ("ld r16, X", "0c 91"),
            ("ld r16, -X", "0e 91"),
            ("ld r16, Y+", "09 91"),
            ("ld r16, Z", "00 81"),
            ("ld r16, -Z", "02 91"),
            ("ld r16, y+", "09 91"),
            ("ld r31, X", "fc 91"),
            ("ld r31, -X", "fe 91"),
            ("ld r31, Y+", "f9 91"),
            ("ld r31, Z", "f0 81"),
            ("ld r31, -Z", "f2 91"),
            ("ld r31, y+", "f9 91"),
            ("st X, r0", "0c 92"),
            ("st -X, r0", "0e 92"),
            ("st Y+, r0", "09 92"),
            ("st Z, r0", "00 82"),
            ("st -Z, r0", "02 92"),
            ("st y+, r0", "09 92"),
            ("st X, r5", "5c 92"),
            ("st -X, r5", "5e 92"),
            ("st Y+, r5", "59 92"),
            ("st Z, r5", "50 82"),
            ("st -Z, r5", "52 92"),
            ("st y+, r5", "59 92"),
            ("st X, r16", "0c 93"),
            ("st -X, r16", "0e 93"),
            ("st Y+, r16", "09 93"),
            ("st Z, r16", "00 83"),
            ("st -Z, r16", "02 93"),
            ("st y+, r16", "09 93"),
            ("st X, r31", "fc 93"),
            ("st -X, r31", "fe 93"),
            ("st Y+, r31", "f9 93"),
            ("st Z, r31", "f0 83"),
            ("st -Z, r31", "f2 93"),
            ("st y+, r31", "f9 93"),
        ],
    );
}

#[test]
fn corpus_avr51_program_memory_load() {
    check(
        "avr51",
        &[
            ("lpm r0, Z+", "05 90"),
            ("lpm r0, z+", "05 90"),
            ("lpm r5, Z+", "55 90"),
            ("lpm r5, z+", "55 90"),
            ("lpm r16, Z+", "05 91"),
            ("lpm r16, z+", "05 91"),
            ("lpm r31, Z+", "f5 91"),
            ("lpm r31, z+", "f5 91"),
            ("elpm", "d8 95"),
            ("elpm r0, Z", "06 90"),
            ("elpm r0, Z+", "07 90"),
            ("elpm r0, z", "06 90"),
            ("elpm r0, z+", "07 90"),
            ("elpm r5, Z", "56 90"),
            ("elpm r5, Z+", "57 90"),
            ("elpm r5, z", "56 90"),
            ("elpm r5, z+", "57 90"),
            ("elpm r16, Z", "06 91"),
            ("elpm r16, Z+", "07 91"),
            ("elpm r16, z", "06 91"),
            ("elpm r16, z+", "07 91"),
            ("elpm r31, Z", "f6 91"),
            ("elpm r31, Z+", "f7 91"),
            ("elpm r31, z", "f6 91"),
            ("elpm r31, z+", "f7 91"),
        ],
    );
}

#[test]
fn corpus_avr51_no_operands() {
    check("avr51", &[("break", "98 95")]);
}

#[test]
fn corpus_avr51_self_programming() {
    check(
        "avr51",
        &[
            ("spm", "e8 95"),
            ("spm Z", "e8 95"),
            ("spm Z+", "f8 95"),
            ("spm z", "e8 95"),
            ("spm z+", "f8 95"),
        ],
    );
}

#[test]
fn corpus_avr51_two_registers() {
    check(
        "avr51",
        &[
            ("mul r0, r5", "05 9c"),
            ("mul r1, r5", "15 9c"),
            ("mul r7, r5", "75 9c"),
            ("mul r15, r5", "f5 9c"),
            ("mul r16, r5", "05 9d"),
            ("mul r17, r5", "15 9d"),
            ("mul r23, r5", "75 9d"),
            ("mul r24, r5", "85 9d"),
            ("mul r25, r5", "95 9d"),
            ("mul r26, r5", "a5 9d"),
            ("mul r29, r5", "d5 9d"),
            ("mul r30, r5", "e5 9d"),
            ("mul r31, r5", "f5 9d"),
            ("mul r5, r0", "50 9c"),
            ("mul r5, r1", "51 9c"),
            ("mul r5, r7", "57 9c"),
            ("mul r5, r15", "5f 9c"),
            ("mul r5, r16", "50 9e"),
            ("mul r5, r17", "51 9e"),
            ("mul r5, r23", "57 9e"),
            ("mul r5, r24", "58 9e"),
            ("mul r5, r25", "59 9e"),
            ("mul r5, r26", "5a 9e"),
            ("mul r5, r29", "5d 9e"),
            ("mul r5, r30", "5e 9e"),
            ("mul r5, r31", "5f 9e"),
            ("mul r31,r31", "ff 9f"),
            ("mul R16, R17", "01 9f"),
            ("mul 3, 4", "34 9c"),
            ("mul xl, zh", "af 9f"),
        ],
    );
}

#[test]
fn corpus_avr51_absolute_jump_and_call() {
    check(
        "avr51",
        &[
            ("call 0", "0e 94 00 00"),
            ("call 2", "0e 94 01 00"),
            ("call 0x1234", "0e 94 1a 09"),
            ("call 0x10000", "0e 94 00 80"),
            ("call 0x3ffffe", "ff 94 ff ff"),
            ("call 0x123456", "4f 94 2b 1a"),
            ("jmp 0", "0c 94 00 00"),
            ("jmp 2", "0c 94 01 00"),
            ("jmp 0x1234", "0c 94 1a 09"),
            ("jmp 0x10000", "0c 94 00 80"),
            ("jmp 0x3ffffe", "fd 94 ff ff"),
            ("jmp 0x123456", "4d 94 2b 1a"),
        ],
    );
}

#[test]
fn corpus_avr51_movw() {
    check(
        "avr51",
        &[
            ("movw r0, r0", "00 01"),
            ("movw r0, r30", "0f 01"),
            ("movw r0, x", "0d 01"),
            ("movw r2, r0", "10 01"),
            ("movw r2, r30", "1f 01"),
            ("movw r2, x", "1d 01"),
            ("movw r14, r0", "70 01"),
            ("movw r14, r30", "7f 01"),
            ("movw r14, x", "7d 01"),
            ("movw r16, r0", "80 01"),
            ("movw r16, r30", "8f 01"),
            ("movw r16, x", "8d 01"),
            ("movw r24, r0", "c0 01"),
            ("movw r24, r30", "cf 01"),
            ("movw r24, x", "cd 01"),
            ("movw r30, r0", "f0 01"),
            ("movw r30, r30", "ff 01"),
            ("movw r30, x", "fd 01"),
            ("movw x, r0", "d0 01"),
            ("movw x, r30", "df 01"),
            ("movw x, x", "dd 01"),
            ("movw y, r0", "e0 01"),
            ("movw y, r30", "ef 01"),
            ("movw y, x", "ed 01"),
            ("movw z, r0", "f0 01"),
            ("movw z, r30", "ff 01"),
            ("movw z, x", "fd 01"),
        ],
    );
}

#[test]
fn corpus_avr51_multiplies() {
    check(
        "avr51",
        &[
            ("muls r16, r16", "00 02"),
            ("muls r16, r31", "0f 02"),
            ("muls r17, r16", "10 02"),
            ("muls r17, r31", "1f 02"),
            ("muls r24, r16", "80 02"),
            ("muls r24, r31", "8f 02"),
            ("muls r31, r16", "f0 02"),
            ("muls r31, r31", "ff 02"),
            ("mulsu r16, r16", "00 03"),
            ("mulsu r16, r19", "03 03"),
            ("mulsu r16, r23", "07 03"),
            ("mulsu r19, r16", "30 03"),
            ("mulsu r19, r19", "33 03"),
            ("mulsu r19, r23", "37 03"),
            ("mulsu r23, r16", "70 03"),
            ("mulsu r23, r19", "73 03"),
            ("mulsu r23, r23", "77 03"),
            ("fmul r16, r16", "08 03"),
            ("fmul r16, r19", "0b 03"),
            ("fmul r16, r23", "0f 03"),
            ("fmul r19, r16", "38 03"),
            ("fmul r19, r19", "3b 03"),
            ("fmul r19, r23", "3f 03"),
            ("fmul r23, r16", "78 03"),
            ("fmul r23, r19", "7b 03"),
            ("fmul r23, r23", "7f 03"),
            ("fmuls r16, r16", "80 03"),
            ("fmuls r16, r19", "83 03"),
            ("fmuls r16, r23", "87 03"),
            ("fmuls r19, r16", "b0 03"),
            ("fmuls r19, r19", "b3 03"),
            ("fmuls r19, r23", "b7 03"),
            ("fmuls r23, r16", "f0 03"),
            ("fmuls r23, r19", "f3 03"),
            ("fmuls r23, r23", "f7 03"),
            ("fmulsu r16, r16", "88 03"),
            ("fmulsu r16, r19", "8b 03"),
            ("fmulsu r16, r23", "8f 03"),
            ("fmulsu r19, r16", "b8 03"),
            ("fmulsu r19, r19", "bb 03"),
            ("fmulsu r19, r23", "bf 03"),
            ("fmulsu r23, r16", "f8 03"),
            ("fmulsu r23, r19", "fb 03"),
            ("fmulsu r23, r23", "ff 03"),
        ],
    );
}

#[test]
fn corpus_avrxmega_xmega_read_modify_write() {
    check(
        "atxmega128a1u",
        &[
            ("xch Z, r0", "04 92"),
            ("xch Z, r16", "04 93"),
            ("xch Z, r31", "f4 93"),
            ("las Z, r0", "05 92"),
            ("las Z, r16", "05 93"),
            ("las Z, r31", "f5 93"),
            ("lac Z, r0", "06 92"),
            ("lac Z, r16", "06 93"),
            ("lac Z, r31", "f6 93"),
            ("lat Z, r0", "07 92"),
            ("lat Z, r16", "07 93"),
            ("lat Z, r31", "f7 93"),
        ],
    );
}

#[test]
fn corpus_avrxmega_no_operands() {
    check("atxmega128a1u", &[("eicall", "19 95"), ("eijmp", "19 94")]);
}

#[test]
fn corpus_avrxmega_des() {
    check(
        "atxmega128a1u",
        &[("des 0", "0b 94"), ("des 1", "1b 94"), ("des 15", "fb 94")],
    );
}

#[test]
fn corpus_avrtiny_no_operands() {
    check(
        "avrtiny",
        &[
            ("clc", "88 94"),
            ("clh", "d8 94"),
            ("cli", "f8 94"),
            ("cln", "a8 94"),
            ("cls", "c8 94"),
            ("clt", "e8 94"),
            ("clv", "b8 94"),
            ("clz", "98 94"),
            ("sec", "08 94"),
            ("seh", "58 94"),
            ("sei", "78 94"),
            ("sen", "28 94"),
            ("ses", "48 94"),
            ("set", "68 94"),
            ("sev", "38 94"),
            ("sez", "18 94"),
            ("icall", "09 95"),
            ("ijmp", "09 94"),
            ("nop", "00 00"),
            ("ret", "08 95"),
            ("reti", "18 95"),
            ("sleep", "88 95"),
            ("break", "98 95"),
            ("wdr", "a8 95"),
        ],
    );
}

#[test]
fn corpus_avrtiny_sreg_bit_set_and_clear() {
    check(
        "avrtiny",
        &[
            ("bclr 0", "88 94"),
            ("bclr 1", "98 94"),
            ("bclr 5", "d8 94"),
            ("bset 0", "08 94"),
            ("bset 1", "18 94"),
            ("bset 5", "58 94"),
        ],
    );
}

#[test]
fn corpus_avrtiny_two_registers() {
    check(
        "avrtiny",
        &[
            ("adc r31,r31", "ff 1f"),
            ("adc R16, R17", "01 1f"),
            ("adc xl, zh", "af 1f"),
            ("add r31,r31", "ff 0f"),
            ("add R16, R17", "01 0f"),
            ("add xl, zh", "af 0f"),
            ("and r31,r31", "ff 23"),
            ("and R16, R17", "01 23"),
            ("and xl, zh", "af 23"),
            ("cp r31,r31", "ff 17"),
            ("cp R16, R17", "01 17"),
            ("cp xl, zh", "af 17"),
            ("cpc r31,r31", "ff 07"),
            ("cpc R16, R17", "01 07"),
            ("cpc xl, zh", "af 07"),
            ("cpse r31,r31", "ff 13"),
            ("cpse R16, R17", "01 13"),
            ("cpse xl, zh", "af 13"),
            ("eor r31,r31", "ff 27"),
            ("eor R16, R17", "01 27"),
            ("eor xl, zh", "af 27"),
            ("mov r31,r31", "ff 2f"),
            ("mov R16, R17", "01 2f"),
            ("mov xl, zh", "af 2f"),
            ("or r31,r31", "ff 2b"),
            ("or R16, R17", "01 2b"),
            ("or xl, zh", "af 2b"),
            ("sbc r31,r31", "ff 0b"),
            ("sbc R16, R17", "01 0b"),
            ("sbc xl, zh", "af 0b"),
            ("sub r31,r31", "ff 1b"),
            ("sub R16, R17", "01 1b"),
            ("sub xl, zh", "af 1b"),
        ],
    );
}

#[test]
fn corpus_avrtiny_one_register_standing_for_two() {
    check(
        "avrtiny",
        &[
            ("clr r16", "00 27"),
            ("clr r17", "11 27"),
            ("clr r23", "77 27"),
            ("lsl r16", "00 0f"),
            ("lsl r17", "11 0f"),
            ("lsl r23", "77 0f"),
            ("rol r16", "00 1f"),
            ("rol r17", "11 1f"),
            ("rol r23", "77 1f"),
            ("tst r16", "00 23"),
            ("tst r17", "11 23"),
            ("tst r23", "77 23"),
        ],
    );
}

#[test]
fn corpus_avrtiny_8_bit_immediates_with_and_without_modifiers() {
    check(
        "avrtiny",
        &[
            ("andi r16, 0x55", "05 75"),
            ("andi r17, 0x55", "15 75"),
            ("andi r24, 0x55", "85 75"),
            ("ldi r16, 0x55", "05 e5"),
            ("ldi r17, 0x55", "15 e5"),
            ("ldi r24, 0x55", "85 e5"),
            ("ori r16, 0x55", "05 65"),
            ("ori r17, 0x55", "15 65"),
            ("ori r24, 0x55", "85 65"),
            ("sbr r16, 0x55", "05 65"),
            ("sbr r17, 0x55", "15 65"),
            ("sbr r24, 0x55", "85 65"),
            ("cpi r16, 0x55", "05 35"),
            ("cpi r17, 0x55", "15 35"),
            ("cpi r24, 0x55", "85 35"),
            ("sbci r16, 0x55", "05 45"),
            ("sbci r17, 0x55", "15 45"),
            ("sbci r24, 0x55", "85 45"),
            ("subi r16, 0x55", "05 55"),
            ("subi r17, 0x55", "15 55"),
            ("subi r24, 0x55", "85 55"),
        ],
    );
}

#[test]
fn corpus_avrtiny_cbr_a_complemented_andi() {
    check(
        "avrtiny",
        &[
            ("cbr r16, 0x55", "0a 7a"),
            ("cbr r17, 0x55", "1a 7a"),
            ("cbr r24, 0x55", "8a 7a"),
        ],
    );
}

#[test]
fn corpus_avrtiny_ser() {
    check(
        "avrtiny",
        &[
            ("ser r16", "0f ef"),
            ("ser r17", "1f ef"),
            ("ser r24", "8f ef"),
        ],
    );
}

#[test]
fn corpus_avrtiny_register_bits() {
    check(
        "avrtiny",
        &[
            ("sbrc r16, 0", "00 fd"),
            ("sbrc r16, 1", "01 fd"),
            ("sbrc r16, 5", "05 fd"),
            ("sbrs r16, 0", "00 ff"),
            ("sbrs r16, 1", "01 ff"),
            ("sbrs r16, 5", "05 ff"),
            ("bld r16, 0", "00 f9"),
            ("bld r16, 1", "01 f9"),
            ("bld r16, 5", "05 f9"),
            ("bst r16, 0", "00 fb"),
            ("bst r16, 1", "01 fb"),
            ("bst r16, 5", "05 fb"),
        ],
    );
}

#[test]
fn corpus_avrtiny_i_o_ports() {
    check(
        "avrtiny",
        &[
            ("in r16, 0", "00 b1"),
            ("in r16, 1", "01 b1"),
            ("in r16, 0x1f", "0f b3"),
            ("out 0, r16", "00 b9"),
            ("out 1, r16", "01 b9"),
            ("out 0x1f, r16", "0f bb"),
        ],
    );
}

#[test]
fn corpus_avrtiny_i_o_bits() {
    check(
        "avrtiny",
        &[
            ("cbi 0, 0", "00 98"),
            ("cbi 0, 1", "01 98"),
            ("cbi 0, 5", "05 98"),
            ("sbi 0, 0", "00 9a"),
            ("sbi 0, 1", "01 9a"),
            ("sbi 0, 5", "05 9a"),
            ("sbic 0, 0", "00 99"),
            ("sbic 0, 1", "01 99"),
            ("sbic 0, 5", "05 99"),
            ("sbis 0, 0", "00 9b"),
            ("sbis 0, 1", "01 9b"),
            ("sbis 0, 5", "05 9b"),
        ],
    );
}

#[test]
fn corpus_avrtiny_conditional_branches() {
    check(
        "avrtiny",
        &[
            ("brcc 0", "f8 f7"),
            ("brcc 2", "00 f4"),
            ("brcc 4", "08 f4"),
            ("brcs 0", "f8 f3"),
            ("brcs 4", "08 f0"),
            ("breq 0", "f9 f3"),
            ("breq 2", "01 f0"),
            ("breq 4", "09 f0"),
            ("brge 2", "04 f4"),
            ("brge 4", "0c f4"),
            ("brhc 0", "fd f7"),
            ("brhc 2", "05 f4"),
            ("brhs 0", "fd f3"),
            ("brhs 2", "05 f0"),
            ("brhs 4", "0d f0"),
            ("brid 0", "ff f7"),
            ("brid 4", "0f f4"),
            ("brie 0", "ff f3"),
            ("brie 2", "07 f0"),
            ("brie 4", "0f f0"),
            ("brlo 2", "00 f0"),
            ("brlo 4", "08 f0"),
            ("brlt 0", "fc f3"),
            ("brlt 2", "04 f0"),
            ("brmi 0", "fa f3"),
            ("brmi 2", "02 f0"),
            ("brmi 4", "0a f0"),
            ("brne 0", "f9 f7"),
            ("brne 4", "09 f4"),
            ("brpl 0", "fa f7"),
            ("brpl 2", "02 f4"),
            ("brpl 4", "0a f4"),
            ("brsh 2", "00 f4"),
            ("brsh 4", "08 f4"),
            ("brtc 0", "fe f7"),
            ("brtc 2", "06 f4"),
            ("brts 0", "fe f3"),
            ("brts 2", "06 f0"),
            ("brts 4", "0e f0"),
            ("brvc 0", "fb f7"),
            ("brvc 4", "0b f4"),
            ("brvs 0", "fb f3"),
            ("brvs 2", "03 f0"),
            ("brvs 4", "0b f0"),
            ("brbc 0, 2", "00 f4"),
            ("brbc 0, -2", "f0 f7"),
            ("brbs 0, 0", "f8 f3"),
            ("brbs 0, 2", "00 f0"),
        ],
    );
}

#[test]
fn corpus_avrtiny_relative_jump_and_call() {
    check(
        "avrtiny",
        &[
            ("rcall 0", "ff df"),
            ("rcall 2", "00 d0"),
            ("rcall -2", "fe df"),
            ("rjmp 0", "ff cf"),
            ("rjmp 2", "00 c0"),
            ("rjmp -2", "fe cf"),
        ],
    );
}

#[test]
fn corpus_avrtiny_one_register() {
    check(
        "avrtiny",
        &[
            ("asr r16", "05 95"),
            ("asr r17", "15 95"),
            ("asr r23", "75 95"),
            ("com r16", "00 95"),
            ("com r17", "10 95"),
            ("com r23", "70 95"),
            ("dec r16", "0a 95"),
            ("dec r17", "1a 95"),
            ("dec r23", "7a 95"),
            ("inc r16", "03 95"),
            ("inc r17", "13 95"),
            ("inc r23", "73 95"),
            ("lsr r16", "06 95"),
            ("lsr r17", "16 95"),
            ("lsr r23", "76 95"),
            ("neg r16", "01 95"),
            ("neg r17", "11 95"),
            ("neg r23", "71 95"),
            ("pop r16", "0f 91"),
            ("pop r17", "1f 91"),
            ("pop r23", "7f 91"),
            ("push r16", "0f 93"),
            ("push r17", "1f 93"),
            ("push r23", "7f 93"),
            ("ror r16", "07 95"),
            ("ror r17", "17 95"),
            ("ror r23", "77 95"),
            ("swap r16", "02 95"),
            ("swap r17", "12 95"),
            ("swap r23", "72 95"),
        ],
    );
}

#[test]
fn corpus_avrtiny_direct_load_and_store() {
    check(
        "avrtiny",
        &[
            ("sts 0x40, r20", "40 a9"),
            ("sts 0x41, r20", "41 a9"),
            ("sts 0x7f, r20", "4f af"),
            ("sts 0x80, r20", "40 a8"),
            ("sts 0xbf, r20", "4f ae"),
            ("sts 0x60, r16", "00 ad"),
            ("sts 0x60, r31", "f0 ad"),
            ("lds r20, 0x40", "40 a1"),
            ("lds r20, 0x41", "41 a1"),
            ("lds r20, 0x7f", "4f a7"),
            ("lds r20, 0x80", "40 a0"),
            ("lds r20, 0xbf", "4f a6"),
            ("lds r16, 0x60", "00 a5"),
            ("lds r31, 0x60", "f0 a5"),
        ],
    );
}

#[test]
fn corpus_avrtiny_load_and_store_through_a_pointer() {
    check(
        "avrtiny",
        &[
            ("ld r16, X", "0c 91"),
            ("ld r16, X+", "0d 91"),
            ("ld r16, -X", "0e 91"),
            ("st X, r16", "0c 93"),
            ("st X+, r16", "0d 93"),
            ("st -X, r16", "0e 93"),
        ],
    );
}

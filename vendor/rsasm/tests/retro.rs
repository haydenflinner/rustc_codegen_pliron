//! Encoding tests for the 8-bit backends: MOS 6502, Zilog Z80 and Intel 8080.
//!
//! There is no llvm-mc or GNU as oracle for any of these three, so the
//! expected bytes here come from two places, and the tests are shaped to make
//! a transcription mistake loud:
//!
//! 1. **Independent transcription.** [`OFFICIAL_6502`] below is the whole
//!    151-opcode official 6502 set, written out flat, in opcode order, from
//!    the published MOS opcode matrix. The backend builds its table the other
//!    way round — structurally, from the `aaabbbcc` decomposition — so the two
//!    only agree if both are right.
//! 2. **Whole-opcode-space walks.** The 6502 test asserts the map has exactly
//!    151 entries and no duplicates; the Z80 tests walk the main page and the
//!    entire `CB` page and assert that every byte value is produced exactly
//!    once; the 8080 test asserts the set of reachable opcodes is exactly the
//!    244 the part defines, leaving the twelve documented holes empty.
//!
//! During development the tables were also diffed against the independent Z80
//! and 6502 *decoders* in the `rsemu` emulator: all 151 official 6502 opcodes
//! agreed on mnemonic and addressing mode, and 690 documented Z80 encodings
//! round-tripped disassemble-then-reassemble byte for byte. That harness lives
//! outside this repository (it needs `rsemu`), which is why the checks kept
//! here are self-contained.

#![cfg(feature = "retro")]

mod common;

use common::*;
use rsasm::arch;
use rsasm::assembler::{Assembler, Options};
use rsasm::lexer::Dialect;
use rsasm::section::SectionId;
use std::collections::BTreeMap;

#[track_caller]
fn enc(arch: &str, src: &str, want: &str) {
    let got = hex(&text_for(arch, src));
    assert_eq!(
        got, want,
        "\n  {arch}: {src}\n  want: {want}\n   got: {got}"
    );
}

#[track_caller]
fn enc6502(src: &str, want: &str) {
    enc("6502", src, want);
}

#[track_caller]
fn encz80(src: &str, want: &str) {
    enc("z80", src, want);
}

#[track_caller]
fn enc8080(src: &str, want: &str) {
    enc("i8080", src, want);
}

// ===========================================================================
// 6502
// ===========================================================================

/// Every official 6502 opcode, in opcode order, with a source line that must
/// produce it.
///
/// Transcribed from the MOS opcode matrix by hand, deliberately *not* from the
/// structural table in `src/arch/retro/mos6502.rs`. The operands are always
/// the same three shapes so the expected length is derivable: `$0x11` is an
/// immediate, `0x11` a zero-page address, `0x2211` an absolute one.
const OFFICIAL_6502: &[(u8, &str)] = &[
    (0x00, "brk"),
    (0x01, "ora (0x11,x)"),
    (0x05, "ora 0x11"),
    (0x06, "asl 0x11"),
    (0x08, "php"),
    (0x09, "ora $0x11"),
    (0x0a, "asl a"),
    (0x0d, "ora 0x2211"),
    (0x0e, "asl 0x2211"),
    (0x10, "bpl .+2"),
    (0x11, "ora (0x11),y"),
    (0x15, "ora 0x11,x"),
    (0x16, "asl 0x11,x"),
    (0x18, "clc"),
    (0x19, "ora 0x2211,y"),
    (0x1d, "ora 0x2211,x"),
    (0x1e, "asl 0x2211,x"),
    (0x20, "jsr 0x2211"),
    (0x21, "and (0x11,x)"),
    (0x24, "bit 0x11"),
    (0x25, "and 0x11"),
    (0x26, "rol 0x11"),
    (0x28, "plp"),
    (0x29, "and $0x11"),
    (0x2a, "rol a"),
    (0x2c, "bit 0x2211"),
    (0x2d, "and 0x2211"),
    (0x2e, "rol 0x2211"),
    (0x30, "bmi .+2"),
    (0x31, "and (0x11),y"),
    (0x35, "and 0x11,x"),
    (0x36, "rol 0x11,x"),
    (0x38, "sec"),
    (0x39, "and 0x2211,y"),
    (0x3d, "and 0x2211,x"),
    (0x3e, "rol 0x2211,x"),
    (0x40, "rti"),
    (0x41, "eor (0x11,x)"),
    (0x45, "eor 0x11"),
    (0x46, "lsr 0x11"),
    (0x48, "pha"),
    (0x49, "eor $0x11"),
    (0x4a, "lsr a"),
    (0x4c, "jmp 0x2211"),
    (0x4d, "eor 0x2211"),
    (0x4e, "lsr 0x2211"),
    (0x50, "bvc .+2"),
    (0x51, "eor (0x11),y"),
    (0x55, "eor 0x11,x"),
    (0x56, "lsr 0x11,x"),
    (0x58, "cli"),
    (0x59, "eor 0x2211,y"),
    (0x5d, "eor 0x2211,x"),
    (0x5e, "lsr 0x2211,x"),
    (0x60, "rts"),
    (0x61, "adc (0x11,x)"),
    (0x65, "adc 0x11"),
    (0x66, "ror 0x11"),
    (0x68, "pla"),
    (0x69, "adc $0x11"),
    (0x6a, "ror a"),
    (0x6c, "jmp (0x2211)"),
    (0x6d, "adc 0x2211"),
    (0x6e, "ror 0x2211"),
    (0x70, "bvs .+2"),
    (0x71, "adc (0x11),y"),
    (0x75, "adc 0x11,x"),
    (0x76, "ror 0x11,x"),
    (0x78, "sei"),
    (0x79, "adc 0x2211,y"),
    (0x7d, "adc 0x2211,x"),
    (0x7e, "ror 0x2211,x"),
    (0x81, "sta (0x11,x)"),
    (0x84, "sty 0x11"),
    (0x85, "sta 0x11"),
    (0x86, "stx 0x11"),
    (0x88, "dey"),
    (0x8a, "txa"),
    (0x8c, "sty 0x2211"),
    (0x8d, "sta 0x2211"),
    (0x8e, "stx 0x2211"),
    (0x90, "bcc .+2"),
    (0x91, "sta (0x11),y"),
    (0x94, "sty 0x11,x"),
    (0x95, "sta 0x11,x"),
    (0x96, "stx 0x11,y"),
    (0x98, "tya"),
    (0x99, "sta 0x2211,y"),
    (0x9a, "txs"),
    (0x9d, "sta 0x2211,x"),
    (0xa0, "ldy $0x11"),
    (0xa1, "lda (0x11,x)"),
    (0xa2, "ldx $0x11"),
    (0xa4, "ldy 0x11"),
    (0xa5, "lda 0x11"),
    (0xa6, "ldx 0x11"),
    (0xa8, "tay"),
    (0xa9, "lda $0x11"),
    (0xaa, "tax"),
    (0xac, "ldy 0x2211"),
    (0xad, "lda 0x2211"),
    (0xae, "ldx 0x2211"),
    (0xb0, "bcs .+2"),
    (0xb1, "lda (0x11),y"),
    (0xb4, "ldy 0x11,x"),
    (0xb5, "lda 0x11,x"),
    (0xb6, "ldx 0x11,y"),
    (0xb8, "clv"),
    (0xb9, "lda 0x2211,y"),
    (0xba, "tsx"),
    (0xbc, "ldy 0x2211,x"),
    (0xbd, "lda 0x2211,x"),
    (0xbe, "ldx 0x2211,y"),
    (0xc0, "cpy $0x11"),
    (0xc1, "cmp (0x11,x)"),
    (0xc4, "cpy 0x11"),
    (0xc5, "cmp 0x11"),
    (0xc6, "dec 0x11"),
    (0xc8, "iny"),
    (0xc9, "cmp $0x11"),
    (0xca, "dex"),
    (0xcc, "cpy 0x2211"),
    (0xcd, "cmp 0x2211"),
    (0xce, "dec 0x2211"),
    (0xd0, "bne .+2"),
    (0xd1, "cmp (0x11),y"),
    (0xd5, "cmp 0x11,x"),
    (0xd6, "dec 0x11,x"),
    (0xd8, "cld"),
    (0xd9, "cmp 0x2211,y"),
    (0xdd, "cmp 0x2211,x"),
    (0xde, "dec 0x2211,x"),
    (0xe0, "cpx $0x11"),
    (0xe1, "sbc (0x11,x)"),
    (0xe4, "cpx 0x11"),
    (0xe5, "sbc 0x11"),
    (0xe6, "inc 0x11"),
    (0xe8, "inx"),
    (0xe9, "sbc $0x11"),
    (0xea, "nop"),
    (0xec, "cpx 0x2211"),
    (0xed, "sbc 0x2211"),
    (0xee, "inc 0x2211"),
    (0xf0, "beq .+2"),
    (0xf1, "sbc (0x11),y"),
    (0xf5, "sbc 0x11,x"),
    (0xf6, "inc 0x11,x"),
    (0xf8, "sed"),
    (0xf9, "sbc 0x2211,y"),
    (0xfd, "sbc 0x2211,x"),
    (0xfe, "inc 0x2211,x"),
];

/// The bytes each row of [`OFFICIAL_6502`] must produce, derived from the
/// shape of its operand rather than written out a second time.
fn expected_6502(opcode: u8, src: &str) -> Vec<u8> {
    if src.contains("0x2211") {
        vec![opcode, 0x11, 0x22]
    } else if src.contains(".+2") {
        // A branch to the instruction after this one: displacement zero.
        vec![opcode, 0x00]
    } else if src.contains("0x11") {
        vec![opcode, 0x11]
    } else {
        vec![opcode]
    }
}

#[test]
fn every_official_6502_opcode_assembles_to_its_documented_byte() {
    for (opcode, src) in OFFICIAL_6502 {
        let want = expected_6502(*opcode, src);
        let got = text_for("6502", src);
        assert_eq!(
            hex(&got),
            hex(&want),
            "`{src}` should encode as {}",
            hex(&want)
        );
    }
}

#[test]
fn the_official_6502_set_is_exactly_151_opcodes() {
    let mut seen: BTreeMap<u8, &str> = BTreeMap::new();
    for (opcode, src) in OFFICIAL_6502 {
        if let Some(prev) = seen.insert(*opcode, src) {
            panic!("opcode {opcode:02x} is claimed twice: `{prev}` and `{src}`");
        }
    }
    assert_eq!(
        seen.len(),
        151,
        "the NMOS 6502 has 151 official opcodes, this table has {}",
        seen.len()
    );
    // 56 official instructions, each named by the first word of its row.
    let mnemonics: std::collections::BTreeSet<&str> =
        OFFICIAL_6502.iter().map(|(_, s)| word(s)).collect();
    assert_eq!(mnemonics.len(), 56, "the 6502 has 56 official instructions");
}

/// The backend must not answer to anything outside those 151 opcodes: every
/// undocumented mnemonic is rejected, not silently encoded.
#[test]
fn undocumented_6502_mnemonics_are_rejected() {
    for m in [
        "slo", "rla", "sre", "rra", "sax", "lax", "dcp", "isc", "anc", "jam",
    ] {
        let err = errors_for("6502", &format!("{m} 0x11"));
        assert!(
            err.contains("unknown 6502 instruction"),
            "`{m}` should not be recognised, got: {err}"
        );
    }
}

#[test]
fn addressing_modes_place_their_operand_bytes() {
    // Zero page and absolute differ in length as well as opcode, and the
    // absolute address is little-endian.
    enc6502("lda 0x12", "a5 12");
    enc6502("lda 0x1234", "ad 34 12");
    enc6502("lda 0x12,x", "b5 12");
    enc6502("lda 0x1234,x", "bd 34 12");
    enc6502("lda 0x1234,y", "b9 34 12");
    enc6502("ldx 0x12,y", "b6 12");
    enc6502("lda (0x12,x)", "a1 12");
    enc6502("lda (0x12),y", "b1 12");
    enc6502("jmp (0x1234)", "6c 34 12");
    enc6502("lda $0x12", "a9 12");
    // A shift with no operand at all means the accumulator, as most 6502
    // sources spell it.
    enc6502("asl", "0a");
    enc6502("asl a", "0a");
    enc6502("rol", "2a");
}

#[test]
fn the_immediate_marker_is_accepted_in_both_spellings() {
    // `$` in the GAS dialect, where `#` would start a comment...
    enc6502("lda $0x12", "a9 12");
    // ...and `#`, the traditional 6502 spelling, in the NASM dialect where
    // `#` is an ordinary token.
    assert_eq!(hex(&nasm_6502("lda #0x12")), "a9 12");
    assert_eq!(hex(&nasm_6502("cpx #0xff")), "e0 ff");
}

#[test]
fn parentheses_around_an_expression_are_not_an_indirect_operand() {
    // `lda` has no indirect mode, so the parentheses can only be grouping.
    enc6502("lda (0x10+2)", "a5 12");
    // `jmp` does have one, so the same shape means indirection there.
    enc6502("jmp (0x10+2)", "6c 12 00");
}

#[test]
fn zero_page_is_chosen_for_values_known_when_encoding() {
    // A constant below 256 takes the short form, above it the long one.
    enc6502("lda 0x00ff", "a5 ff");
    enc6502("lda 0x0100", "ad 00 01");
    enc6502("lda 0xff,x", "b5 ff");
    enc6502("lda 0x100,x", "bd 00 01");
    // So does a symbol defined before it is used, which is how zero-page
    // variables are normally declared.
    enc6502("ptr = 0x20\nlda ptr", "a5 20");
    enc6502("ptr = 0x20\nsta ptr+1,x", "95 21");

    // A symbol that is not known yet takes the absolute form even when it
    // turns out to be small; see `direct` in the backend for why relaxation
    // cannot safely make this choice.
    let bytes = flat("6502", "lda later\nlater = 0x20\n");
    assert_eq!(hex(&bytes), "ad 20 00");
    let bytes = flat("6502", "lda zp\n.org 0x40\nzp: .byte 0\n");
    assert_eq!(hex(&bytes[..3]), "ad 40 00");

    // A zero-page-only form with a symbolic operand is still accepted, and
    // checked against the final address.
    let bytes = flat("6502", "lda (vec),y\nvec = 0x80\n");
    assert_eq!(hex(&bytes), "b1 80");
    let err = assemble_flat_for("6502", "lda (vec),y\nvec = 0x1234\n", 0);
    assert!(err.diags.has_errors(), "0x1234 is not a zero-page pointer");
}

#[test]
fn a_rom_based_away_from_zero_resolves_its_labels() {
    // The regression this backend's zero-page policy exists for: with a base
    // address, labels that are small *offsets* are large *addresses*.
    let src = "\
reset:  ldx $0
loop:   lda msg,x
        beq done
        jsr putc
        inx
        bne loop
done:   jmp done
putc:   sta 0xd012
        rts
msg:    .ascii \"HI\"
        .byte 0
";
    let asm = assemble_flat_for("6502", src, 0x8000);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(
        hex(&asm.section_bytes(SectionId(0))),
        "a2 00 bd 14 80 f0 06 20 10 80 e8 d0 f5 4c 0d 80 8d 12 d0 60 48 49 00"
    );
}

#[test]
fn a_zero_page_only_form_diagnoses_an_address_it_cannot_reach() {
    // `stx zp,y` exists; `stx abs,y` does not.
    let err = errors_for("6502", "stx 0x1234,y");
    assert!(err.contains("zero page"), "got: {err}");
    assert!(
        err.contains("stx"),
        "the message should name the mnemonic: {err}"
    );
}

#[test]
fn branch_displacements_are_measured_from_the_next_instruction() {
    // Backwards to itself: -2.
    enc6502("here: bne here", "d0 fe");
    // Forwards over one 2-byte instruction.
    enc6502("beq skip\nnop\nnop\nskip:", "f0 02 ea ea");
    // The extremes of the range, +127 and -128 from the following byte.
    let far = text_for("6502", "bne 1f\n.space 127, 0xea\n1:\n");
    assert_eq!(hex(&far[..2]), "d0 7f");
    let back = text_for("6502", "1:\n.space 126, 0xea\nbne 1b\n");
    assert_eq!(hex(&back[126..]), "d0 80");
}

#[test]
fn a_branch_out_of_range_is_diagnosed() {
    let err = errors_for("6502", "bne 1f\n.space 128, 0xea\n1:\n");
    assert!(err.contains("out of range"), "got: {err}");
    // The message names the real limit rather than the field's byte width.
    assert!(err.contains("(-128 to 127)"), "got: {err}");
}

#[test]
fn operands_that_no_form_accepts_are_diagnosed() {
    assert!(errors_for("6502", "sta $0x12").contains("no immediate form"));
    assert!(errors_for("6502", "inx 0x12").contains("takes no operand"));
    assert!(errors_for("6502", "tax a").contains("takes no operand"));
    assert!(errors_for("6502", "bne").contains("needs an operand"));
    assert!(errors_for("6502", "lda 0x12,z").contains("expected `x` or `y`"));
}

// ===========================================================================
// Z80
// ===========================================================================

/// Every main-page opcode: one source line per byte value, in opcode order.
/// The four prefix bytes (`CB`, `DD`, `ED`, `FD`) are not instructions, so 252
/// rows is the whole page.
fn main_page_corpus() -> Vec<(u8, String)> {
    let r = ["b", "c", "d", "e", "h", "l", "(hl)", "a"];
    let rp = ["bc", "de", "hl", "sp"];
    let rp2 = ["bc", "de", "hl", "af"];
    let cc = ["nz", "z", "nc", "c", "po", "pe", "p", "m"];
    let alu = [
        "add a,", "adc a,", "sub ", "sbc a,", "and ", "xor ", "or ", "cp ",
    ];
    // x = 0
    let mut out: Vec<(u8, String)> = vec![
        (0x00, "nop".into()),
        (0x08, "ex af,af".into()),
        (0x10, "djnz .+2".into()),
        (0x18, "jr .+2".into()),
    ];
    for (i, c) in cc[..4].iter().enumerate() {
        out.push((0x20 | (i as u8) << 3, format!("jr {c},.+2")));
    }
    for (p, name) in rp.iter().enumerate() {
        let p = p as u8;
        out.push((0x01 | p << 4, format!("ld {name},0x2211")));
        out.push((0x09 | p << 4, format!("add hl,{name}")));
        out.push((0x03 | p << 4, format!("inc {name}")));
        out.push((0x0b | p << 4, format!("dec {name}")));
    }
    out.push((0x02, "ld (bc),a".into()));
    out.push((0x0a, "ld a,(bc)".into()));
    out.push((0x12, "ld (de),a".into()));
    out.push((0x1a, "ld a,(de)".into()));
    out.push((0x22, "ld (0x2211),hl".into()));
    out.push((0x2a, "ld hl,(0x2211)".into()));
    out.push((0x32, "ld (0x2211),a".into()));
    out.push((0x3a, "ld a,(0x2211)".into()));
    for (y, name) in r.iter().enumerate() {
        let y = y as u8;
        out.push((0x04 | y << 3, format!("inc {name}")));
        out.push((0x05 | y << 3, format!("dec {name}")));
        out.push((0x06 | y << 3, format!("ld {name},0x11")));
    }
    for (y, name) in ["rlca", "rrca", "rla", "rra", "daa", "cpl", "scf", "ccf"]
        .iter()
        .enumerate()
    {
        out.push((0x07 | (y as u8) << 3, (*name).into()));
    }

    // x = 1: the 8-bit load matrix, with HALT in the hole.
    for (d, dn) in r.iter().enumerate() {
        for (s, sn) in r.iter().enumerate() {
            if d == 6 && s == 6 {
                continue;
            }
            out.push((0x40 | (d as u8) << 3 | s as u8, format!("ld {dn},{sn}")));
        }
    }
    out.push((0x76, "halt".into()));

    // x = 2: the ALU group.
    for (y, op) in alu.iter().enumerate() {
        for (z, name) in r.iter().enumerate() {
            out.push((0x80 | (y as u8) << 3 | z as u8, format!("{op}{name}")));
        }
    }

    // x = 3
    for (y, c) in cc.iter().enumerate() {
        let y = y as u8;
        out.push((0xc0 | y << 3, format!("ret {c}")));
        out.push((0xc2 | y << 3, format!("jp {c},0x2211")));
        out.push((0xc4 | y << 3, format!("call {c},0x2211")));
        out.push((0xc7 | y << 3, format!("rst {}", y * 8)));
    }
    for (y, op) in alu.iter().enumerate() {
        out.push((0xc6 | (y as u8) << 3, format!("{op}0x11")));
    }
    for (p, name) in rp2.iter().enumerate() {
        let p = p as u8;
        out.push((0xc1 | p << 4, format!("pop {name}")));
        out.push((0xc5 | p << 4, format!("push {name}")));
    }
    out.push((0xc3, "jp 0x2211".into()));
    out.push((0xc9, "ret".into()));
    out.push((0xcd, "call 0x2211".into()));
    out.push((0xd3, "out (0x11),a".into()));
    out.push((0xd9, "exx".into()));
    out.push((0xdb, "in a,(0x11)".into()));
    out.push((0xe3, "ex (sp),hl".into()));
    out.push((0xe9, "jp (hl)".into()));
    out.push((0xeb, "ex de,hl".into()));
    out.push((0xf3, "di".into()));
    out.push((0xf9, "ld sp,hl".into()));
    out.push((0xfb, "ei".into()));
    out
}

#[test]
fn the_z80_main_page_is_covered_exactly_once() {
    let mut seen: BTreeMap<u8, String> = BTreeMap::new();
    for (opcode, src) in main_page_corpus() {
        let bytes = text_for("z80", &src);
        assert_eq!(
            bytes.first().copied(),
            Some(opcode),
            "`{src}` should start with {opcode:02x}, got {}",
            hex(&bytes)
        );
        if let Some(prev) = seen.insert(opcode, src.clone()) {
            panic!("opcode {opcode:02x} produced twice: `{prev}` and `{src}`");
        }
    }
    // Everything but the four prefix bytes.
    let missing: Vec<String> = (0..=0xffu8)
        .filter(|b| !matches!(b, 0xcb | 0xdd | 0xed | 0xfd) && !seen.contains_key(b))
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(missing.is_empty(), "main page holes: {missing:?}");
    assert_eq!(seen.len(), 252);
}

#[test]
fn the_z80_cb_page_is_covered_exactly_once() {
    let r = ["b", "c", "d", "e", "h", "l", "(hl)", "a"];
    let rot = ["rlc", "rrc", "rl", "rr", "sla", "sra", "sll", "srl"];
    let mut seen: BTreeMap<u8, String> = BTreeMap::new();
    let mut check = |opcode: u8, src: String| {
        let bytes = text_for("z80", &src);
        assert_eq!(
            hex(&bytes),
            format!("cb {opcode:02x}"),
            "`{src}` should be cb {opcode:02x}"
        );
        if let Some(prev) = seen.insert(opcode, src.clone()) {
            panic!("CB opcode {opcode:02x} produced twice: `{prev}` and `{src}`");
        }
    };
    for (y, op) in rot.iter().enumerate() {
        for (z, name) in r.iter().enumerate() {
            check((y as u8) << 3 | z as u8, format!("{op} {name}"));
        }
    }
    for (x, op) in ["bit", "res", "set"].iter().enumerate() {
        let x = x as u8 + 1;
        for bit in 0..8u8 {
            for (z, name) in r.iter().enumerate() {
                check(x << 6 | bit << 3 | z as u8, format!("{op} {bit},{name}"));
            }
        }
    }
    assert_eq!(seen.len(), 256, "the CB page has no holes at all");
}

#[test]
fn the_z80_ed_page_is_distinct_and_correctly_prefixed() {
    let r = ["b", "c", "d", "e", "h", "l", "a"];
    let rp = ["bc", "de", "hl", "sp"];
    let mut srcs: Vec<String> = Vec::new();
    for (i, name) in r.iter().enumerate() {
        let _ = i;
        srcs.push(format!("in {name},(c)"));
        srcs.push(format!("out (c),{name}"));
    }
    for name in rp {
        srcs.push(format!("sbc hl,{name}"));
        srcs.push(format!("adc hl,{name}"));
        if name != "hl" {
            // HL's `(nn)` loads are the main-page 0x22 and 0x2a; the ED page
            // copies of them are undocumented.
            srcs.push(format!("ld (0x2211),{name}"));
            srcs.push(format!("ld {name},(0x2211)"));
        }
    }
    for name in [
        "neg", "retn", "reti", "rrd", "rld", "ldi", "ldd", "ldir", "lddr", "cpi", "cpd", "cpir",
        "cpdr", "ini", "ind", "inir", "indr", "outi", "outd", "otir", "otdr",
    ] {
        srcs.push(name.into());
    }
    for m in 0..3 {
        srcs.push(format!("im {m}"));
    }
    for s in ["ld i,a", "ld r,a", "ld a,i", "ld a,r"] {
        srcs.push(s.into());
    }

    let mut seen: BTreeMap<u8, String> = BTreeMap::new();
    for src in &srcs {
        let bytes = text_for("z80", src);
        assert_eq!(
            bytes.first().copied(),
            Some(0xed),
            "`{src}` is not on the ED page"
        );
        let op = bytes[1];
        if let Some(prev) = seen.insert(op, src.clone()) {
            panic!("ED opcode {op:02x} produced twice: `{prev}` and `{src}`");
        }
    }
    assert_eq!(
        seen.len(),
        56,
        "the documented ED page, minus the `(nn)` loads of HL, which live on the main page"
    );
}

#[test]
fn the_index_pages_prefix_the_main_page_encoding() {
    // `DD`/`FD` say "read HL as IX/IY", so the byte after the prefix is
    // exactly what the unprefixed instruction would have been.
    for (reg, prefix) in [("ix", "dd"), ("iy", "fd")] {
        enc(
            "z80",
            &format!("ld {reg},0x2211"),
            &format!("{prefix} 21 11 22"),
        );
        enc("z80", &format!("push {reg}"), &format!("{prefix} e5"));
        enc("z80", &format!("pop {reg}"), &format!("{prefix} e1"));
        enc("z80", &format!("inc {reg}"), &format!("{prefix} 23"));
        enc("z80", &format!("dec {reg}"), &format!("{prefix} 2b"));
        enc("z80", &format!("add {reg},bc"), &format!("{prefix} 09"));
        enc("z80", &format!("add {reg},{reg}"), &format!("{prefix} 29"));
        enc("z80", &format!("jp ({reg})"), &format!("{prefix} e9"));
        enc("z80", &format!("ld sp,{reg}"), &format!("{prefix} f9"));
        enc("z80", &format!("ex (sp),{reg}"), &format!("{prefix} e3"));
        enc(
            "z80",
            &format!("ld ({reg}+5),a"),
            &format!("{prefix} 77 05"),
        );
        enc(
            "z80",
            &format!("ld a,({reg}+5)"),
            &format!("{prefix} 7e 05"),
        );
        enc(
            "z80",
            &format!("ld ({reg}-1),0x42"),
            &format!("{prefix} 36 ff 42"),
        );
        enc("z80", &format!("inc ({reg}+0)"), &format!("{prefix} 34 00"));
        enc(
            "z80",
            &format!("add a,({reg}+2)"),
            &format!("{prefix} 86 02"),
        );
        // A bare `(ix)` still spends its displacement byte.
        enc("z80", &format!("ld a,({reg})"), &format!("{prefix} 7e 00"));
        // The four-byte DD CB form: displacement *before* the opcode.
        enc(
            "z80",
            &format!("rlc ({reg}+5)"),
            &format!("{prefix} cb 05 06"),
        );
        enc(
            "z80",
            &format!("bit 7,({reg}+5)"),
            &format!("{prefix} cb 05 7e"),
        );
        enc(
            "z80",
            &format!("res 0,({reg}-2)"),
            &format!("{prefix} cb fe 86"),
        );
        enc(
            "z80",
            &format!("set 3,({reg}+1)"),
            &format!("{prefix} cb 01 de"),
        );
    }
}

#[test]
fn z80_operand_spellings() {
    // The accumulator may be named or implied in the ALU group.
    encz80("add a,b", "80");
    encz80("add b", "80");
    encz80("sub a", "97");
    encz80("cp 0x20", "fe 20");
    // 16-bit adds are three different pages depending on the mnemonic.
    encz80("add hl,de", "19");
    encz80("adc hl,de", "ed 5a");
    encz80("sbc hl,de", "ed 52");
    // `(nn)` loads: HL has a main-page opcode, the others are ED.
    encz80("ld hl,(0x2211)", "2a 11 22");
    encz80("ld de,(0x2211)", "ed 5b 11 22");
    encz80("ld (0x2211),sp", "ed 73 11 22");
    // Relative jumps count from the following instruction.
    encz80("here: jr here", "18 fe");
    encz80("here: djnz here", "10 fe");
    encz80("jr nz,.+2", "20 00");
    // Ports.
    encz80("in a,(0x10)", "db 10");
    encz80("in b,(c)", "ed 40");
    encz80("out (0x10),a", "d3 10");
    encz80("out (c),d", "ed 51");
    // `rst` takes the target address, not the vector number.
    encz80("rst 0", "c7");
    encz80("rst 0x38", "ff");
}

#[test]
fn intel_8080_encodings() {
    enc8080("mov m,a", "77");
    enc8080("mvi a,0x42", "3e 42");
    enc8080("mvi m,0xff", "36 ff");
    enc8080("lxi h,0x1234", "21 34 12");
    enc8080("lxi sp,0xfff0", "31 f0 ff");
    enc8080("jnz 0x1234", "c2 34 12");
    enc8080("call 0x1234", "cd 34 12");
    enc8080("cpi 0xff", "fe ff");
    enc8080("rst 1", "cf");
    enc8080("in 0x10", "db 10");
    enc8080("out 0x10", "d3 10");
    enc8080("push psw", "f5");
    enc8080("pop h", "e1");
    // `call` with no operand is not an instruction.
    assert!(errors_for("i8080", "call").contains("invalid operands"));
    assert!(errors_for("i8080", "nop 1").contains("takes no operands"));
}

#[test]
fn register_names_are_not_symbols() {
    // Without this, each of these would assemble as a reference to an
    // undefined symbol and fail much later with a worse message.
    assert!(errors_for("z80", "ld (ix+1),hl").contains("invalid operands"));
    assert!(errors_for("z80", "jp nz").contains("invalid operands"));
    assert!(errors_for("i8080", "mvi a,b").contains("invalid operands"));
    assert!(errors_for("6502", "lda x").contains("is a register"));
}

#[test]
fn z80_range_and_shape_errors_are_diagnosed() {
    assert!(errors_for("z80", "ld (hl),(hl)").contains("halt"));
    assert!(
        errors_for("z80", "ld (ix+1),(iy+1)").contains("only one operand may be indexed"),
        "an instruction cannot carry two index prefixes"
    );
    assert!(errors_for("z80", "bit 8,a").contains("must be 0 to 7"));
    assert!(errors_for("z80", "im 3").contains("must be 0, 1 or 2"));
    assert!(errors_for("z80", "rst 7").contains("0, 8, 16"));
    assert!(errors_for("z80", "jr po,.+2").contains("only has the conditions"));
    assert!(errors_for("z80", "add ix,hl").contains("cannot mix"));
    assert!(errors_for("z80", "adc ix,bc").contains("no `ix`/`iy` form"));
    // An (IX+d) displacement is signed and one byte.
    assert!(errors_for("z80", "ld a,(ix+128)").contains("out of range"));
    assert!(errors_for("z80", "ld a,(ix-129)").contains("out of range"));
    assert!(errors_for("z80", "frobnicate").contains("unknown Z80 instruction"));
}

// ===========================================================================
// Intel 8080
// ===========================================================================

/// Every 8080 opcode, generated from the Intel mnemonic tables. The 8080
/// defines 244 of the 256 byte values; the twelve it leaves out are the ones
/// the Z80 later used for `EX AF,AF'`, `DJNZ`, `JR`, `EXX` and the four
/// prefixes.
fn i8080_corpus() -> Vec<(u8, String)> {
    let r = ["b", "c", "d", "e", "h", "l", "m", "a"];
    let rp = ["b", "d", "h", "sp"];
    let rp2 = ["b", "d", "h", "psw"];
    let alu_r = ["add", "adc", "sub", "sbb", "ana", "xra", "ora", "cmp"];
    let alu_i = ["adi", "aci", "sui", "sbi", "ani", "xri", "ori", "cpi"];
    let ret_cc = ["rnz", "rz", "rnc", "rc", "rpo", "rpe", "rp", "rm"];
    let jmp_cc = ["jnz", "jz", "jnc", "jc", "jpo", "jpe", "jp", "jm"];
    let call_cc = ["cnz", "cz", "cnc", "cc", "cpo", "cpe", "cp", "cm"];
    let mut out: Vec<(u8, String)> = Vec::new();

    for (d, dn) in r.iter().enumerate() {
        for (s, sn) in r.iter().enumerate() {
            if d == 6 && s == 6 {
                continue;
            }
            out.push((0x40 | (d as u8) << 3 | s as u8, format!("mov {dn},{sn}")));
        }
    }
    out.push((0x76, "hlt".into()));
    for (y, op) in alu_r.iter().enumerate() {
        for (z, name) in r.iter().enumerate() {
            out.push((0x80 | (y as u8) << 3 | z as u8, format!("{op} {name}")));
        }
    }
    for (y, op) in alu_i.iter().enumerate() {
        out.push((0xc6 | (y as u8) << 3, format!("{op} 0x11")));
    }
    for (y, name) in r.iter().enumerate() {
        let y = y as u8;
        out.push((0x06 | y << 3, format!("mvi {name},0x11")));
        out.push((0x04 | y << 3, format!("inr {name}")));
        out.push((0x05 | y << 3, format!("dcr {name}")));
    }
    for (p, name) in rp.iter().enumerate() {
        let p = p as u8;
        out.push((0x01 | p << 4, format!("lxi {name},0x2211")));
        out.push((0x09 | p << 4, format!("dad {name}")));
        out.push((0x03 | p << 4, format!("inx {name}")));
        out.push((0x0b | p << 4, format!("dcx {name}")));
    }
    for (p, name) in rp2.iter().enumerate() {
        let p = p as u8;
        out.push((0xc5 | p << 4, format!("push {name}")));
        out.push((0xc1 | p << 4, format!("pop {name}")));
    }
    for (p, name) in ["b", "d"].iter().enumerate() {
        let p = p as u8;
        out.push((0x02 | p << 4, format!("stax {name}")));
        out.push((0x0a | p << 4, format!("ldax {name}")));
    }
    for y in 0..8u8 {
        out.push((0xc0 | y << 3, ret_cc[y as usize].into()));
        out.push((0xc2 | y << 3, format!("{} 0x2211", jmp_cc[y as usize])));
        out.push((0xc4 | y << 3, format!("{} 0x2211", call_cc[y as usize])));
        out.push((0xc7 | y << 3, format!("rst {y}")));
    }
    for (name, opcode) in [
        ("nop", 0x00u8),
        ("rlc", 0x07),
        ("rrc", 0x0f),
        ("ral", 0x17),
        ("rar", 0x1f),
        ("daa", 0x27),
        ("cma", 0x2f),
        ("stc", 0x37),
        ("cmc", 0x3f),
        ("ret", 0xc9),
        ("pchl", 0xe9),
        ("xthl", 0xe3),
        ("xchg", 0xeb),
        ("sphl", 0xf9),
        ("di", 0xf3),
        ("ei", 0xfb),
    ] {
        out.push((opcode, name.into()));
    }
    for (name, opcode) in [
        ("shld", 0x22u8),
        ("lhld", 0x2a),
        ("sta", 0x32),
        ("lda", 0x3a),
        ("jmp", 0xc3),
        ("call", 0xcd),
    ] {
        out.push((opcode, format!("{name} 0x2211")));
    }
    out.push((0xdb, "in 0x11".into()));
    out.push((0xd3, "out 0x11".into()));
    out
}

#[test]
fn the_8080_defines_exactly_244_opcodes() {
    let mut seen: BTreeMap<u8, String> = BTreeMap::new();
    for (opcode, src) in i8080_corpus() {
        let bytes = text_for("i8080", &src);
        assert_eq!(
            bytes.first().copied(),
            Some(opcode),
            "`{src}` should start with {opcode:02x}, got {}",
            hex(&bytes)
        );
        if let Some(prev) = seen.insert(opcode, src.clone()) {
            panic!("opcode {opcode:02x} produced twice: `{prev}` and `{src}`");
        }
    }
    assert_eq!(
        seen.len(),
        244,
        "the 8080 defines 244 of the 256 byte values"
    );
    let holes: Vec<u8> = (0..=0xffu8).filter(|b| !seen.contains_key(b)).collect();
    assert_eq!(
        holes,
        vec![
            0x08, 0x10, 0x18, 0x20, 0x28, 0x30, 0x38, 0xcb, 0xd9, 0xdd, 0xed, 0xfd
        ],
        "the 8080's undefined byte values"
    );
}

#[test]
fn the_8080_and_the_z80_agree_on_every_shared_encoding() {
    // The Z80 was built to run 8080 code, so an Intel mnemonic and its Zilog
    // spelling must produce the same byte. Anything else would mean one of
    // the two tables has drifted.
    for (intel, zilog) in [
        ("mov a,b", "ld a,b"),
        ("mov m,c", "ld (hl),c"),
        ("mvi a,0x42", "ld a,0x42"),
        ("mvi m,0x42", "ld (hl),0x42"),
        ("lxi h,0x1234", "ld hl,0x1234"),
        ("lxi sp,0x1234", "ld sp,0x1234"),
        ("stax b", "ld (bc),a"),
        ("ldax d", "ld a,(de)"),
        ("inx h", "inc hl"),
        ("dcx d", "dec de"),
        ("dad b", "add hl,bc"),
        ("inr m", "inc (hl)"),
        ("dcr a", "dec a"),
        ("add m", "add a,(hl)"),
        ("ana b", "and b"),
        ("xra c", "xor c"),
        ("ora d", "or d"),
        ("cmp e", "cp e"),
        ("adi 0x11", "add a,0x11"),
        ("cpi 0x11", "cp 0x11"),
        ("jmp 0x1234", "jp 0x1234"),
        ("jnz 0x1234", "jp nz,0x1234"),
        ("jm 0x1234", "jp m,0x1234"),
        ("cnc 0x1234", "call nc,0x1234"),
        ("rz", "ret z"),
        ("rst 7", "rst 56"),
        ("push psw", "push af"),
        ("pop b", "pop bc"),
        ("shld 0x1234", "ld (0x1234),hl"),
        ("lhld 0x1234", "ld hl,(0x1234)"),
        ("sta 0x1234", "ld (0x1234),a"),
        ("lda 0x1234", "ld a,(0x1234)"),
        ("xchg", "ex de,hl"),
        ("xthl", "ex (sp),hl"),
        ("pchl", "jp (hl)"),
        ("sphl", "ld sp,hl"),
        ("in 0x11", "in a,(0x11)"),
        ("out 0x11", "out (0x11),a"),
        ("hlt", "halt"),
        ("ei", "ei"),
        ("di", "di"),
        ("nop", "nop"),
    ] {
        let a = hex(&text_for("i8080", intel));
        let b = hex(&text_for("z80", zilog));
        assert_eq!(a, b, "`{intel}` (8080) and `{zilog}` (Z80) must agree");
    }
}

#[test]
fn the_8080_backend_rejects_zilog_spellings() {
    // Silently accepting `ld` here would hide a real porting mistake.
    let err = errors_for("i8080", "ld a,b");
    assert!(err.contains("Z80 mnemonic"), "got: {err}");
    assert!(errors_for("i8080", "stax h").contains("takes `b` or `d`"));
    assert!(errors_for("i8080", "mov m,m").contains("hlt"));
    assert!(errors_for("i8080", "rst 8").contains("must be 0 to 7"));
    assert!(errors_for("i8080", "frobnicate").contains("unknown 8080 instruction"));
}

// ===========================================================================
// Output, padding and robustness
// ===========================================================================

#[test]
fn flat_binary_output_is_what_these_targets_produce() {
    // These machines predate ELF; `-f bin` at a fixed origin is the point.
    let asm = assemble_flat_for(
        "6502",
        "        .org 0x0600\n\
         start:  lda $0x01\n\
                 sta 0x0200\n\
                 jmp start\n",
        0,
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let bytes = asm.section_bytes(SectionId(0));
    assert_eq!(hex(&bytes[0x600..]), "a9 01 8d 00 02 4c 00 06");

    let asm = assemble_flat_for(
        "z80",
        "        .org 0x0100\n        ld hl,0x0100\n        ret\n",
        0,
    );
    let bytes = asm.section_bytes(SectionId(0));
    assert_eq!(hex(&bytes[0x100..]), "21 00 01 c9");
}

#[test]
fn alignment_padding_uses_the_real_no_op() {
    // Zero is `BRK` on a 6502, so padding with zeroes would not be padding.
    let bytes = text_for("6502", "nop\n.align 4\n");
    assert_eq!(hex(&bytes), "ea ea ea ea");
    // `NOP` really is zero on the Z80 and the 8080.
    assert_eq!(hex(&text_for("z80", "ld a,b\n.align 4\n")), "78 00 00 00");
    assert_eq!(
        hex(&text_for("i8080", "mov a,b\n.align 4\n")),
        "78 00 00 00"
    );
}

#[test]
fn a_pointer_is_two_bytes_wide() {
    // An address table on a 16-bit machine is a list of 2-byte words.
    let bytes = flat("6502", ".org 0x1234\nhere: .2byte here\n");
    assert_eq!(hex(&bytes[0x1234..]), "34 12");
    // With no relocation format, a reference that is never defined cannot be
    // deferred to a linker, so it is an error here.
    assert!(errors_for("6502", ".2byte elsewhere").contains("relocation"));
}

/// Malformed input must produce a diagnostic, never a panic. The assertion is
/// only that assembly terminated: what each line reports is not the point.
#[test]
fn malformed_input_never_panics() {
    let lines = [
        "lda",
        "lda ,",
        "lda (",
        "lda )",
        "lda (,x)",
        "lda (),y",
        "lda ((0x11)),y",
        "lda 0x11,",
        "lda 0x11,,x",
        "lda a,a,a",
        "asl a a",
        "jmp (",
        "bne",
        "brk 1",
        "sta $",
        "lda $$",
        "lda #",
        "ldx 0x11,y,z",
        "jmp (0x11,y)",
        "lda (0x11,y)",
        "lda (0x11),x",
        "ld",
        "ld a",
        "ld a,",
        "ld ,a",
        "ld (,)",
        "ld (ix+),a",
        "ld (ix+,a",
        "ld a,(ix",
        "ld (hl),(hl)",
        "bit",
        "bit ,a",
        "bit a,a",
        "rst",
        "rst -1",
        "im",
        "in",
        "out",
        "jr",
        "jr nz",
        "djnz",
        "ex",
        "ex af",
        "push",
        "pop sp",
        "add",
        "add hl",
        "add hl,af",
        "ld i,b",
        "ld b,i",
        "ld ix,(",
        "out (c),(hl)",
        "in (hl),(c)",
        "set 9,(ix+300)",
        "mov",
        "mov a",
        "mov a,",
        "mvi",
        "mvi a",
        "lxi",
        "lxi z,1",
        "rst",
        "stax",
        "ldax h",
        "push z",
        "in",
        "out",
        "jmp",
        "call",
        "dad z",
        "inr",
        "adi",
        "mov m,m",
        "",
        " ",
        "\t",
        "0x11",
        "(",
        ")",
        ",",
        "$",
        "#",
        ".+",
        "..",
    ];
    for archname in ["6502", "z80", "i8080"] {
        for line in lines {
            let a = arch::lookup(archname).expect("backend is compiled in");
            let mut asm = Assembler::new(a, Options::new());
            asm.assemble_str("fuzz.s", line);
            asm.finish();
            // Reaching here at all is the assertion.
            let _ = asm.section_bytes(SectionId(0));
        }
    }
}

#[test]
fn every_backend_name_and_alias_resolves() {
    for (name, canonical) in [
        ("z80", "z80"),
        ("Z80", "z80"),
        ("zilog-z80", "z80"),
        ("6502", "6502"),
        ("mos6502", "6502"),
        ("m6502", "6502"),
        ("i8080", "i8080"),
        ("8080", "i8080"),
        ("intel-8080", "i8080"),
    ] {
        let a = arch::lookup(name).unwrap_or_else(|| panic!("`{name}` should resolve"));
        assert_eq!(a.name(), canonical);
    }
    for n in ["z80", "6502", "i8080"] {
        assert!(arch::available().contains(&n), "`{n}` should be listed");
    }
}

// ---- helpers --------------------------------------------------------------

/// Assembles `src` as a flat binary based at zero and returns its bytes,
/// failing the test on any diagnostic.
#[track_caller]
fn flat(archname: &str, src: &str) -> Vec<u8> {
    let asm = assemble_flat_for(archname, src, 0);
    assert!(
        !asm.diags.has_errors(),
        "{}\nsource:\n{src}",
        asm.diags.render(&asm.sm, false)
    );
    asm.section_bytes(SectionId(0))
}

/// The first whitespace-separated word of a source line.
fn word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or(s)
}

/// Assembles one 6502 line in the NASM dialect, where `#` is a token rather
/// than the start of a comment.
fn nasm_6502(src: &str) -> Vec<u8> {
    let a = arch::lookup("6502").expect("backend is compiled in");
    let options = Options::new().with_dialect(Dialect::Nasm);
    let mut asm = Assembler::new(a, options);
    asm.assemble_str("t.s", src);
    asm.finish();
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm.section_bytes(SectionId(0))
}

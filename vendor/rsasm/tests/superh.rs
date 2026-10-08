//! SuperH encoding tests.
//!
//! Every expected byte string here came out of GNU as: `sh-elf-as` (binutils
//! 2.47) for `sh`, and `sh-elf-as -little` for `shl`, the references that
//! `tools/xas-diff/run.sh sh shl` compares against. Nothing in this file is
//! an encoding rsasm invented for itself.

#![cfg(feature = "superh")]

mod common;
use common::*;

/// Asserts that `src` assembles for big-endian SH to `want`.
#[track_caller]
fn be(src: &str, want: &str) {
    let got = hex(&text_for("sh", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// Asserts that `src` assembles for little-endian SH to `want`.
#[track_caller]
fn le(src: &str, want: &str) {
    let got = hex(&text_for("shl", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// Asserts that `src` assembles for big-endian SH, and that the bytes start
/// with `head` and end with `tail` around `len` bytes in all: for programs
/// whose middle is `.space`.
#[track_caller]
fn be_around(src: &str, head: &str, tail: &str, len: usize) {
    let b = text_for("sh", src);
    let got = hex(&b);
    assert_eq!(b.len(), len, "\nsource: {src}\n   got: {got}");
    assert!(
        got.starts_with(head) && got.ends_with(tail),
        "\nsource: {src}\n  head: {head}\n  tail: {tail}\n   got: {got}"
    );
}

// ---- data transfer ----------------------------------------------------------

#[test]
fn the_confirmed_starting_points() {
    be("mov #1, r0", "e0 01");
    le("mov #1, r0", "01 e0");
    be("rts", "00 0b");
    be("nop", "00 09");
}

#[test]
fn register_moves_and_register_spellings() {
    be("mov r3, r4", "64 33");
    // `sp` is r15; GNU as also takes the SH-DSP names `ix` for r8.
    be("mov sp, r0", "60 f3");
    be("mov ix, r0", "60 83");
    // Mnemonics and register names ignore case.
    be("CMP/EQ r1, r2", "32 10");
    be("Mov.L @R1+, R2", "62 16");
}

#[test]
fn eight_bit_immediates_take_either_spelling_of_a_byte() {
    be("mov #-1, r2", "e2 ff");
    be("mov #255, r3", "e3 ff");
    be("mov #'A', r6", "e6 41");
    be("add #-4, r15", "7f fc");
    be("cmp/eq #0x55, r0", "88 55");
    be("and #0xf0, r0", "c9 f0");
    be("tst.b #4, @(r0,gbr)", "cc 04");
    be("trapa #34", "c3 22");
}

#[test]
fn indirect_increment_decrement_and_indexed_modes() {
    be("mov.b @r1, r2", "62 10");
    be("mov.w r1, @r2", "22 11");
    be("mov.l @r1+, r2", "62 16");
    be("mov.l r2, @-r15", "2f 26");
    be("mov.l @(r0,r3), r4", "04 3e");
    be("mov.l r5, @(r0,r6)", "06 56");
}

#[test]
fn displacements_are_counted_in_units_of_the_operand_size() {
    // Four bits of count: 15 bytes, 30 bytes or 60 bytes of reach.
    be("mov.b @(15,r3), r0", "84 3f");
    be("mov.w @(30,r3), r0", "85 3f");
    be("mov.l @(60,r6), r7", "57 6f");
    be("mov.l r9, @(60,r10)", "1a 9f");
    be("mov.b r0, @(1,r1)", "80 11");
    be("mov.w r0, @(28,r2)", "81 2e");
    // Eight bits of count from GBR.
    be("mov.b @(200,gbr), r0", "c4 c8");
    be("mov.w @(400,gbr), r0", "c5 c8");
    be("mov.l @(1000,gbr), r0", "c6 fa");
    be("mov.l r0, @(1020,gbr)", "c2 ff");
}

#[test]
fn a_displacement_that_the_field_cannot_hold_names_its_limit() {
    let e = errors_for("sh", "mov.l @(64,r1), r2");
    assert!(e.contains("0 to 60"), "{e}");
    let e = errors_for("sh", "mov.l @(62,r1), r2");
    assert!(e.contains("not a multiple of 4"), "{e}");
    let e = errors_for("sh", "mov.w @(31,r1), r0");
    assert!(e.contains("not a multiple of 2"), "{e}");
    let e = errors_for("sh", "mov.b @(16,r1), r0");
    assert!(e.contains("0 to 15"), "{e}");
    let e = errors_for("sh", "mov.l @(1024,gbr), r0");
    assert!(e.contains("0 to 1020"), "{e}");
    // The fields are unsigned.
    let e = errors_for("sh", "mov.l @(-4,r1), r2");
    assert!(e.contains("0 to 60"), "{e}");
    let e = errors_for("sh", "mov #256, r0");
    assert!(e.contains("-128 to 255"), "{e}");
}

#[test]
fn only_r0_reaches_the_byte_and_word_displacement_forms() {
    let e = errors_for("sh", "mov.b @(4,r1), r2");
    assert!(
        e.contains("r0,@(disp,rm)") || e.contains("@(disp,rm),r0"),
        "{e}"
    );
}

#[test]
fn label_differences_and_equates_fill_immediates_and_displacements() {
    be(
        "mov #b-a, r1\nmov.l @(b-a,r2), r3\na: nop\nnop\nb: nop\nnop",
        "e1 04 53 21 00 09 00 09 00 09 00 09",
    );
    be(
        ".equ OFF, 8\n.set IMM, -3\nmov.l @(OFF,r1), r2\nmov #IMM, r3\n\
         mov.w @(OFF*2,gbr), r0\ntrapa #OFF",
        "52 12 e3 fd c5 08 c3 08",
    );
}

// ---- arithmetic, logic, shifts ----------------------------------------------

#[test]
fn slash_mnemonics_are_rejoined_from_the_lexer_tokens() {
    be("cmp/eq r1, r2", "32 10");
    be("cmp/hs r3, r4", "34 32");
    be("cmp/ge r5, r6", "36 53");
    be("cmp/hi r7, r8", "38 76");
    be("cmp/gt r9, r10", "3a 97");
    be("cmp/pz r11", "4b 11");
    be("cmp/pl r12", "4c 15");
    be("cmp/str r13, r14", "2e dc");
    be("fcmp/gt dr2, dr4", "f4 25");
    be("bf/s 0x100", "8f 7e");
}

#[test]
fn a_mnemonic_with_a_space_around_its_slash_is_not_one() {
    // GNU as reads a mnemonic up to the first space, so all three fail there.
    for src in ["cmp / eq r1, r2", "cmp/ eq r1, r2", "cmp /eq r1, r2"] {
        let _ = errors_for("sh", src);
    }
    let e = errors_for("sh", "cmp/xx r1, r2");
    assert!(e.contains("unknown instruction `cmp/xx`"), "{e}");
}

#[test]
fn arithmetic_and_multiply() {
    be("dt r1", "41 10");
    be("div0s r1, r2", "22 17");
    be("div0u", "00 19");
    be("div1 r1, r2", "32 14");
    be("dmuls.l r1, r2", "32 1d");
    be("dmulu.l r1, r2", "32 15");
    be("mul.l r1, r2", "02 17");
    be("muls.w r1, r2", "22 1f");
    be("mulu.w r1, r2", "22 1e");
    be("neg r1, r2", "62 1b");
    be("negc r1, r2", "62 1a");
    be("exts.b r1, r2", "62 1e");
    be("extu.w r1, r2", "62 1d");
    be("swap.b r1, r2", "62 18");
    be("swap.w r1, r2", "62 19");
    be("xtrct r1, r2", "22 1d");
    be("movt r5", "05 29");
    be("not r1, r2", "62 17");
}

#[test]
fn shifts() {
    be("shll2 r1", "41 08");
    be("shlr8 r1", "41 19");
    be("shll16 r1", "41 28");
    be("rotcl r1", "41 24");
    be("rotcr r1", "41 25");
    be("shad r1, r2", "42 1c");
    be("shld r3, r4", "44 3d");
}

// ---- system and control registers ---------------------------------------------

#[test]
fn control_register_loads_and_stores() {
    be("ldc r4, r0_bank", "44 8e");
    be("stc.l r6_bank, @-r15", "4f e3");
    be("ldc r1, ssr", "41 3e");
    be("stc sgr, r1", "01 3a");
    be("ldc r1, dbr", "41 fa");
    be("sts.l pr, @-r15", "4f 22");
    be("lds.l @r15+, pr", "4f 26");
    be("lds r1, fpul", "41 5a");
    be("sts.l fpscr, @-r1", "41 62");
}

#[test]
fn no_operand_instructions() {
    be("sleep", "00 1b");
    be("clrmac", "00 28");
    be("clrt", "00 08");
    be("sett", "00 18");
    be("clrs", "00 48");
    be("sets", "00 58");
    be("ldtlb", "00 38");
    be("synco", "00 ab");
    be("rts ; nop", "00 0b 00 09");
}

#[test]
fn sh4_and_sh4a_additions() {
    be("movca.l r0, @r1", "01 c3");
    be("movli.l @r1, r0", "01 63");
    be("movco.l r0, @r1", "01 73");
    be("movua.l @r1+, r0", "41 e9");
}

// ---- floating point -----------------------------------------------------------

#[test]
fn fpu_instructions() {
    be("fadd fr2, fr4", "f4 20");
    // A `dr` register is written with its first `fr` register's number.
    be("fmov dr2, dr4", "f4 2c");
    be("fmov.s @r1, fr2", "f2 18");
    be("fmov.d dr2, @-r15", "ff 2b");
    be("fipr fv4, fv8", "f9 ed");
    be("ftrv xmtrx, fv4", "f5 fd");
    be("fmac fr0, fr1, fr2", "f2 1e");
    be("fsca fpul, dr2", "f2 fd");
    be("fcnvds dr2, fpul", "f2 bd");
    be("frchg", "fb fd");
    be("fschg", "f3 fd");
    be("fpchg", "f7 fd");
}

#[test]
fn a_restricted_variant_refuses_what_its_cpu_lacks() {
    let e = errors_for("sh2", "fadd fr2, fr4");
    assert!(e.contains("FPU"), "{e}");
    let e = errors_for("sh1", "dt r1");
    assert!(e.contains("SH-2"), "{e}");
    let e = errors_for("sh3", "movca.l r0, @r1");
    assert!(e.contains("SH-4"), "{e}");
    let e = errors_for("sh4", "movli.l @r1, r0");
    assert!(e.contains("SH-4A"), "{e}");
    // What the CPU has still assembles, and `.arch` can widen it again.
    assert_eq!(hex(&text_for("sh2", "dt r1")), "41 10");
    assert_eq!(hex(&text_for("sh1", ".arch sh4\nfadd fr2, fr4")), "f4 20");
}

// ---- comments ----------------------------------------------------------------

#[test]
fn bang_comments_anywhere_and_hash_only_at_the_start_of_a_line() {
    be("nop ! a comment", "00 09");
    be("nop!no space", "00 09");
    be("  # a comment after indentation\nnop", "00 09");
    be("mov.l @(4,r1),r2 /* block */", "52 11");
}

// ---- branches -----------------------------------------------------------------

#[test]
fn branches_count_words_from_the_instruction_plus_four() {
    // A number is an address in the section, as a label there would be.
    be("bra 4098", "a7 ff");
    be("bra -4092", "a8 00");
    be("bt 0x102", "89 7f");
    be("bt -0xfc", "89 80");
    be("bt.s 8", "8d 02");
    be(
        "start:\nbt next\nnop\nnext: bf start\nbra start\nnop\nbsr next\nnop",
        "89 00 00 09 8b fc af fb 00 09 bf fb 00 09",
    );
    be("jmp @r0", "40 2b");
    be("jsr @r15", "4f 0b");
    be("braf r3", "03 23");
    be("bsrf r4", "04 03");
}

#[test]
fn bra_and_bsr_reach_twelve_bits_of_words() {
    let b = text_for(
        "sh",
        "bra far\nnop\n.space 4090\nfar: bsr back\nnop\n.space 4092\nback: nop",
    );
    assert_eq!(hex(&b[..4]), "a7 fd 00 09");
    assert_eq!(hex(&b[4094..4098]), "b7 fe 00 09");
    assert_eq!(hex(&b[b.len() - 2..]), "00 09");
    assert_eq!(b.len(), 8192);
    let e = errors_for("sh", "bra far\nnop\n.space 4096\nfar: nop");
    assert!(e.contains("out of range") && e.contains("-4096"), "{e}");
}

#[test]
fn an_out_of_reach_bt_becomes_the_opposite_branch_over_a_bra() {
    // Within reach: the plain two-byte form.
    be_around("bt far\n.space 254\nfar: nop", "89 7e 00", "00 00 09", 258);
    // One word past it: `bf .+6; bra far; nop`, exactly as GNU as does it.
    be_around(
        "bt far\n.space 258\nfar: nop",
        "8b 01 a0 81 00 09 00",
        "00 00 09",
        266,
    );
    // A delayed branch keeps its own slot instruction for the `bra`.
    be_around(
        "bt/s far\nmov r1, r2\n.space 300\nfar: nop",
        "8b 00 a0 96 62 13 00",
        "00 00 09",
        308,
    );
    // Backward too.
    be_around(
        "far: nop\n.space 400\nbf/s far\nadd #1, r1",
        "00 09 00",
        "00 89 00 af 34 71 01",
        408,
    );
}

#[test]
fn a_relaxed_branch_can_push_an_earlier_one_out_of_reach() {
    let b = text_for("sh", "bt b\nbt c\n.space 252\nb: nop\n.space 300\nc: nop");
    assert_eq!(hex(&b[..12]), "8b 01 a0 81 00 09 8b 01 a1 15 00 09");
    assert_eq!(b.len(), 12 + 252 + 2 + 300 + 2);
}

#[test]
fn branches_are_sized_in_order_as_gnu_as_sizes_them() {
    // The `bt` grows by four bytes, which the `.align 3` takes back. Sized
    // in order, the `bf` sees `back` already moved and stays short at -256;
    // sized from the previous pass it would have grown.
    let b = text_for(
        "sh",
        "bt far\nnop\nback: nop\n.space 246\n.align 3\nnop\nnop\nbf back\nfar: nop",
    );
    assert_eq!(b.len(), 264);
    assert_eq!(hex(&b[..8]), "8b 01 a0 80 00 09 00 09");
    assert_eq!(hex(&b[b.len() - 8..]), "00 09 00 09 8b 80 00 09");
    // Past an alignment, a label is not assumed to have moved with the
    // growth before it, so `bt x` stays short.
    let b = text_for(
        "sh",
        "bt far\nbt x\n.space 256\n.align 3\nx: nop\n.space 300\nfar: nop",
    );
    assert_eq!(b.len(), 568);
    assert_eq!(hex(&b[..8]), "8b 01 a1 18 00 09 89 7f");
    // And a branch that the growth has carried past such a label keeps its
    // size for the pass, so this `bf` stays short too.
    let b = text_for(
        "sh",
        "far: nop\n.space 300\n.rept 66\nbt far\n.endr\nbf y\n.align 2\ny: nop",
    );
    assert_eq!(b.len(), 704);
    assert_eq!(hex(&b[302..308]), "8b 01 af 66 00 09");
    assert_eq!(hex(&b[b.len() - 8..]), "00 09 8b ff 00 09 00 09");
}

#[test]
fn padding_ahead_of_a_long_counts_while_branches_are_sized() {
    // In the first layout the `.long` needs two bytes of padding, which GNU
    // as inserts before refusing it; the first `bf/s` grows past a limit
    // that only the padding put it over, and a branch never shrinks back.
    // In the final layout the padding is gone and nothing is misaligned.
    let b = text_for(
        "sh",
        "bf/s C0\nmov r1, r2\nbt/s C0\nmov r1, r2\nmov r3, r4\n.long 94\n.space 200\n\
         .word 0x7a5b\nbt/s C1\nmov r1, r2\nbt/s C0\nmov r1, r2\nbra C1\nnop\n.space 10\n\
         .long 96\nbf C0\nbf C1\nmov r3, r4\n.align 2\n.long 0x77826199\n.long 0xf2518742\n\
         C0:\nC1:",
    );
    assert_eq!(b.len(), 260);
    assert_eq!(
        hex(&b[..16]),
        "89 00 a0 7f 62 13 8d 7d 62 13 64 33 00 00 00 5e"
    );
    assert_eq!(
        hex(&b[b.len() - 40..]),
        "62 13 8d 11 62 13 a0 0f 00 09 00 00 00 00 00 00 00 00 00 00 \
         00 00 00 60 8b 06 8b 05 64 33 00 09 77 82 61 99 f2 51 87 42"
    );
}

#[test]
fn a_branch_beyond_even_the_relaxed_form_is_an_error() {
    let e = errors_for("sh", "bt far\n.space 5000\nfar: nop");
    assert!(e.contains("out of range"), "{e}");
}

#[test]
fn a_branch_to_an_odd_address_is_an_error_not_a_truncation() {
    let e = errors_for("sh", "bra odd\n.byte 1\nodd: nop");
    assert!(e.contains("not a multiple of 2"), "{e}");
}

#[test]
fn the_delay_slot_is_left_as_written() {
    be(
        "jsr @r1\nmov #0, r4\nbra 1f\nadd #1, r4\n1: rts\nmov r4, r0",
        "41 0b e4 00 a0 00 74 01 00 0b 60 43",
    );
}

// ---- PC-relative loads ----------------------------------------------------------

#[test]
fn mov_l_counts_longs_from_pc_plus_four_rounded_down() {
    // On a four-byte boundary the base is here + 4 ...
    be(
        "mov.l lit, r1\njmp @r1\nnop\nnop\nlit: .long 0x12345678",
        "d1 01 41 2b 00 09 00 09 12 34 56 78",
    );
    // ... and two bytes past one it is here + 2, so the field is the same.
    be(
        "nop\nmov.l lit, r1\njmp @r1\nnop\nlit: .long 0x89abcdef",
        "00 09 d1 01 41 2b 00 09 89 ab cd ef",
    );
    be(
        "nop\nmova tbl, r0\nmov.l tbl, r2\nrts\nnop\n.balign 4\ntbl: .long 1\n.long 2",
        "00 09 c7 02 d2 01 00 0b 00 09 00 09 00 00 00 01 00 00 00 02",
    );
    be_around(
        "mov.l lit, r0\nnop\n.space 1016\nlit: .long 7",
        "d0 fe 00 09 00",
        "00 00 00 00 07",
        1024,
    );
}

#[test]
fn a_relaxed_delayed_branch_can_move_a_load_to_the_other_boundary() {
    // The `bt/s` grows by two bytes after the load's first layout, which
    // flips the boundary the load sits on; GNU as and rsasm both follow.
    let b = text_for(
        "sh",
        "nop\nbt/s far\nnop\nmov.l lit, r1\nnop\n.balign 4\nlit: .long 5\n\
         .space 300\nfar: nop\nnop",
    );
    assert_eq!(
        hex(&b[..16]),
        "00 09 8b 00 a0 9a 00 09 d1 00 00 09 00 00 00 05"
    );
}

#[test]
fn mov_w_counts_words_from_pc_plus_four() {
    be(
        "mov.w w1, r1\nmov.w w2, r2\nrts\nnop\nw1: .word 0x1234\nw2: .word -2",
        "91 02 92 02 00 0b 00 09 12 34 ff fe",
    );
    be_around(
        "mov.w lit, r0\n.space 508\nlit: .word 7",
        "90 fd 00",
        "00 00 07",
        512,
    );
}

#[test]
fn at_disp_pc_is_an_offset_from_the_instruction() {
    be("mov.l @(4,pc), r2", "d2 00");
    be("mov.l @(1024,pc), r2", "d2 ff");
    be("mov.w @(514,pc), r2", "92 ff");
    be("mova @(8,pc), r0", "c7 01");
    // `@(n,pc)` is the address `. + n`, checked as a label there would be:
    // sh-elf-as says "pcrel too far" for the first two.
    let e = errors_for("sh", "mov.l @(1028,pc), r2");
    assert!(e.contains("offset 1024 is out of range (0 to 1020)"), "{e}");
    let e = errors_for("sh", "mov.w @(516,pc), r2");
    assert!(e.contains("offset 512 is out of range (0 to 510)"), "{e}");
    // `. + 8` from address 2 is not on a four-byte boundary ("offset to
    // unaligned destination"), and the message says so rather than quoting
    // a distance from somewhere else; `. + 2` is on one.
    let e = errors_for("sh", "nop\nmov.l @(8,pc), r2");
    assert!(
        e.contains("the target is not on a 4-byte boundary") && !e.contains("offset"),
        "{e}"
    );
    be("nop\nmov.l @(2,pc), r0", "00 09 d0 00");
}

#[test]
fn at_label_pc_is_the_deprecated_spelling_of_label() {
    let asm = assemble_for("sh", "mov.l @(lit,pc), r1\nnop\nlit: .long 5");
    assert!(!asm.diags.has_errors());
    assert!(
        asm.diags.render(&asm.sm, false).contains("deprecated"),
        "GNU as warns about this spelling, and so should rsasm"
    );
    assert_eq!(
        hex(&asm.section_bytes(rsasm::section::SectionId(0))),
        "d1 00 00 09 00 00 00 05"
    );
}

#[test]
fn a_misaligned_literal_is_an_error_not_a_rounding() {
    // `.4byte`, since a `.long` there would be refused as misaligned data
    // first. sh-elf-as: "offset to unaligned destination", from either
    // boundary.
    let e = errors_for("sh", "mov.l lit, r1\nnop\n.byte 0\nlit: .4byte 1");
    assert!(e.contains("not on a 4-byte boundary"), "{e}");
    let e = errors_for("sh", "nop\nmov.l lit, r1\n.byte 0\nlit: .4byte 1");
    assert!(e.contains("not on a 4-byte boundary"), "{e}");
}

#[test]
fn a_literal_before_the_load_is_out_of_range() {
    // The fields are unsigned. sh-elf-as: "negative offset" and "pcrel too
    // far" for `mov.l` and `mova`, "pcrel too far" for `mov.w`.
    let e = errors_for("sh", ".align 2\nlit: .long 1\nmov.l lit, r1");
    assert!(e.contains("offset -8 is out of range (0 to 1020)"), "{e}");
    let e = errors_for("sh", ".align 2\nlit: .long 1\nnop\nmova lit, r0");
    assert!(e.contains("offset -8 is out of range (0 to 1020)"), "{e}");
    let e = errors_for("sh", ".align 1\nlit: .word 1\nmov.w lit, r1");
    assert!(e.contains("offset -6 is out of range (0 to 510)"), "{e}");
    // The nearest literal a load two bytes past a boundary can have is the
    // very next word, at a displacement of zero.
    be(
        "nop\nmov.l lit, r0\nlit: .long 1",
        "00 09 d0 00 00 00 00 01",
    );
}

#[test]
fn a_literal_out_of_reach_names_the_limit() {
    // One error each, as sh-elf-as gives ("pcrel too far").
    let e = errors_for("sh", "mov.l lit, r0\nnop\n.space 1024\nlit: .long 7");
    assert!(e.contains("offset 1024 is out of range (0 to 1020)"), "{e}");
    assert_eq!(e.matches("error").count(), 1, "{e}");
    let e = errors_for("sh", "mov.w lit, r0\n.space 514\nlit: .word 7");
    assert!(e.contains("offset 512 is out of range (0 to 510)"), "{e}");
    assert_eq!(e.matches("error").count(), 1, "{e}");
}

#[test]
fn a_load_from_an_undefined_symbol_is_reported_once() {
    let e = errors_for("sh", "mov.l undefined_symbol, r0");
    assert_eq!(e.matches("error").count(), 1, "{e}");
}

#[test]
fn a_load_at_an_odd_address_takes_gnu_as_base() {
    // An instruction at an odd address can never run; sh-elf-as still
    // assembles it, measuring from (PC + 5) & ~3, and so does rsasm.
    be(
        ".byte 1, 2, 3\nmova lit, r0\n.align 2\nlit: .long 1",
        "01 02 03 c7 00 00 00 09 00 00 00 01",
    );
}

// ---- data alignment -------------------------------------------------------------

#[test]
fn word_and_long_data_must_start_on_their_own_boundary() {
    // sh-elf-as: "misaligned data", in any section.
    for src in [
        "nop\n.byte 1\n.word 2",
        ".byte 1\n.short 2",
        "nop\n.long 1",
        ".byte 1\n.int 2",
        ".data\n.byte 1\n.quad 2",
    ] {
        let e = errors_for("sh", src);
        assert!(e.contains("misaligned data"), "{src}: {e}");
    }
    // `.2byte`, `.4byte`, `.8byte` and the `.ua` spellings are unaligned.
    be(
        ".byte 1\n.ualong 2\n.uaword 3\n.4byte 4\n.2byte 5\n.8byte 6\n.uaquad 7",
        "01 00 00 00 02 00 03 00 00 00 04 00 05 00 00 00 00 00 00 00 06 00 00 00 00 00 00 00 07",
    );
}

#[test]
fn aligned_data_raises_the_section_alignment() {
    // sh-elf-as marks `.text` 2 and `.data` 4 here, and pads the code
    // section's tail to its alignment.
    be(".long 1\nnop", "00 00 00 01 00 09 00 09");
    le(".long 1\nnop", "01 00 00 00 09 00 09 00");
    let asm = assemble_for("sh", ".data\n.long 1\n.text\n.word 1\nnop");
    assert!(!asm.diags.has_errors());
    let align = |name: &str| {
        asm.sections
            .iter()
            .find(|s| asm.interner.get(s.name) == name)
            .map(|s| s.align)
    };
    assert_eq!(align(".text"), Some(2));
    assert_eq!(align(".data"), Some(4));
}

// ---- byte order ---------------------------------------------------------------

#[test]
fn sh_and_shl_emit_the_same_words_byte_swapped() {
    let sources = [
        "mov #1, r0",
        "mov.l @(60,r6), r7",
        "cmp/str r13, r14",
        "bt 0x102",
        "bra -4092",
        "mov.l @(1024,pc), r2",
        "ldc r4, r0_bank",
        "fipr fv4, fv8",
        "rts ; nop",
        "start:\nbt next\nnop\nnext: bf start\nbra start\nnop\nbsr next\nnop",
    ];
    for src in sources {
        let big = text_for("sh", src);
        let mut swapped = text_for("shl", src);
        assert_eq!(big.len() % 2, 0, "{src}");
        for pair in swapped.chunks_mut(2) {
            pair.swap(0, 1);
        }
        assert_eq!(hex(&big), hex(&swapped), "{src}");
    }
}

#[test]
fn little_endian_words_and_data() {
    le("rts", "0b 00");
    le("mov.l r2, @-r15", "26 2f");
    le("fpchg", "fd f7");
    le(
        "mov.l x, r0\nrts\nnop\nnop\nx: .long 0x01020304\n.word 0x0506\n.byte 7, 8",
        "01 d0 0b 00 09 00 09 00 04 03 02 01 06 05 07 08",
    );
    le(
        "bt/s far\nmov r1, r2\n.space 300\nfar: nop",
        &format!("00 8b 96 a0 13 62 {}09 00", "00 ".repeat(300)),
    );
}

// ---- padding and object properties ----------------------------------------------

#[test]
fn code_alignment_pads_with_nops_after_a_zero_byte_for_an_odd_gap() {
    be(
        ".byte 1\n.balign 4\nnop\nnop\nnop\nnop",
        "01 00 00 09 00 09 00 09 00 09 00 09",
    );
    le(
        ".byte 1\n.balign 4\nnop\nnop\nnop\nnop",
        "01 00 09 00 09 00 09 00 09 00 09 00",
    );
}

#[test]
fn code_is_not_aligned_for_you() {
    // GNU as puts this `nop` at offset 1, and so does rsasm.
    be(".byte 1\nnop", "01 00 09");
}

#[test]
fn object_level_properties() {
    for name in ["sh", "shl"] {
        let a = rsasm::arch::lookup(name).expect("backend present");
        assert_eq!(a.name(), name);
        assert_eq!(a.elf_machine(), 42, "{name}"); // EM_SH
        assert_eq!(a.data_reloc(4, false), Some(1), "{name}"); // R_SH_DIR32
        assert_eq!(a.data_reloc(4, true), Some(2), "{name}"); // R_SH_REL32
        assert_eq!(a.data_reloc(2, false), Some(33), "{name}"); // R_SH_DIR16
        assert_eq!(a.data_reloc(1, false), Some(34), "{name}"); // R_SH_DIR8
        assert_eq!(a.modifier_reloc("GOT", 4, false), Some(160), "{name}");
        assert_eq!(a.modifier_reloc("PLT", 4, false), Some(161), "{name}");
        assert_eq!(a.modifier_reloc("GOTOFF", 4, false), Some(166), "{name}");
        assert_eq!(a.modifier_reloc("TPOFF", 4, false), Some(148), "{name}");
        assert_eq!(a.pointer_bytes(&a.initial_state()), 4);
        assert_eq!(a.word_bytes(), 2);
        assert_eq!(a.align_unit(), 1);
    }
    assert_eq!(
        rsasm::arch::lookup("sh").unwrap().endian(),
        rsasm::arch::Endian::Big
    );
    assert_eq!(
        rsasm::arch::lookup("shl").unwrap().endian(),
        rsasm::arch::Endian::Little
    );
}

#[test]
fn an_external_long_gets_r_sh_dir32() {
    let asm = assemble_for("sh", "nop\n.p2align 2\n.long x");
    assert!(!asm.diags.has_errors());
    assert_eq!(asm.relocs.len(), 1);
    assert_eq!(asm.relocs[0].kind, 1);
    assert_eq!(asm.relocs[0].offset, 4);
}

// ---- e_flags ------------------------------------------------------------------
//
// GNU as for SH writes the least capable CPU that has every instruction in the
// file into `e_flags`. Every value in the tables below is what `sh-elf-as`
// wrote for that source, and `sh-elf-as -little` wrote the same; for the CPU
// names, what `sh-elf-as --isa=<cpu>` wrote (`--isa=sh` for `sh1`).

/// `e_flags` of the ELF object `arch` makes of `src`.
#[track_caller]
fn e_flags(arch: &str, src: &str) -> u32 {
    let asm = assemble_for(arch, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let elf = rsasm::output::elf::build(&asm).expect("ELF output");
    let b: [u8; 4] = elf[0x24..0x28].try_into().unwrap();
    if elf[5] == 2 {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    }
}

/// Follows each form in the tables, so a PC-relative load has a literal to
/// load and a branch somewhere to go.
const LITERAL: &str = "\n.space 4\n.align 2\nlit: .long 0";

#[test]
fn every_form_marks_the_least_capable_cpu_that_has_it() {
    for (form, want) in EVERY_FORM {
        let src = format!("{form}{LITERAL}");
        for arch in ["sh", "shl"] {
            assert_eq!(e_flags(arch, &src), *want, "{arch}: {form}");
        }
    }
}

#[test]
fn two_forms_mark_the_least_capable_cpu_that_has_both() {
    // One form for each pair of distinct CPU sets the table uses, both ways
    // round: `movca.l` with `ldtlb` is an SH-4 without an FPU, which neither
    // says on its own.
    for (a, b, want) in SET_PAIRS {
        for src in [format!("{a}\n{b}{LITERAL}"), format!("{b}\n{a}{LITERAL}")] {
            for arch in ["sh", "shl"] {
                assert_eq!(e_flags(arch, &src), *want, "{arch}: {src}");
            }
        }
    }
}

#[test]
fn data_labels_and_sections_do_not_count() {
    // A file with no instructions is SH-1, and instructions count wherever
    // they are.
    for (src, want) in WHOLE_FILES {
        for arch in ["sh", "shl"] {
            assert_eq!(e_flags(arch, src), *want, "{arch}: {src}");
        }
    }
}

#[test]
fn a_cpu_name_marks_that_cpu_whatever_the_file_uses() {
    for (arch, src, want) in CPU_NAMES {
        assert_eq!(e_flags(arch, src), *want, "{arch}: {src}");
    }
}

#[test]
fn fsqrt_needs_an_sh3e() {
    // `sh-elf-as --isa=sh2e` and `--isa=sh3` both refuse `fsqrt fr1`: the
    // SH-2E FPU has no square root, though it has `fadd` and the rest.
    let e = errors_for("sh2e", "fsqrt fr1");
    assert!(e.contains("SH-3E"), "{e}");
    let e = errors_for("sh3", "fsqrt fr1");
    assert!(e.contains("SH-3E"), "{e}");
    assert_eq!(hex(&text_for("sh2e", "fadd fr2, fr4")), "f4 20");
}

const EVERY_FORM: &[(&str, u32)] = &[
    ("mov #4,r2", 0x1),
    ("mov r3,r2", 0x1),
    ("mov.b r3,@r4", 0x1),
    ("mov.b r3,@-r4", 0x1),
    ("mov.b r3,@(r0,r4)", 0x1),
    ("mov.b r0,@(3,r5)", 0x1),
    ("mov.b r0,@(5,gbr)", 0x1),
    ("mov.b @r5,r2", 0x1),
    ("mov.b @r5+,r2", 0x1),
    ("mov.b @(r0,r5),r2", 0x1),
    ("mov.b @(3,r5),r0", 0x1),
    ("mov.b @(5,gbr),r0", 0x1),
    ("mov.w r3,@r4", 0x1),
    ("mov.w r3,@-r4", 0x1),
    ("mov.w r3,@(r0,r4)", 0x1),
    ("mov.w r0,@(6,r5)", 0x1),
    ("mov.w r0,@(10,gbr)", 0x1),
    ("mov.w @r5,r2", 0x1),
    ("mov.w @r5+,r2", 0x1),
    ("mov.w @(r0,r5),r2", 0x1),
    ("mov.w @(6,r5),r0", 0x1),
    ("mov.w @(10,gbr),r0", 0x1),
    ("mov.w lit,r2", 0x1),
    ("mov.l r3,@r4", 0x1),
    ("mov.l r3,@-r4", 0x1),
    ("mov.l r3,@(r0,r4)", 0x1),
    ("mov.l r3,@(8,r4)", 0x1),
    ("mov.l r0,@(20,gbr)", 0x1),
    ("mov.l @r5,r2", 0x1),
    ("mov.l @r5+,r2", 0x1),
    ("mov.l @(r0,r5),r2", 0x1),
    ("mov.l @(12,r5),r2", 0x1),
    ("mov.l @(20,gbr),r0", 0x1),
    ("mov.l lit,r2", 0x1),
    ("mova lit,r0", 0x1),
    ("movt r2", 0x1),
    ("swap.b r3,r2", 0x1),
    ("swap.w r3,r2", 0x1),
    ("xtrct r3,r2", 0x1),
    ("movca.l r0,@r4", 0x12),
    ("movli.l @r4,r0", 0x11),
    ("movco.l r0,@r4", 0x11),
    ("movua.l @r4,r0", 0x11),
    ("movua.l @r4+,r0", 0x11),
    ("add #4,r2", 0x1),
    ("add r3,r2", 0x1),
    ("addc r3,r2", 0x1),
    ("addv r3,r2", 0x1),
    ("sub r3,r2", 0x1),
    ("subc r3,r2", 0x1),
    ("subv r3,r2", 0x1),
    ("cmp/eq #4,r0", 0x1),
    ("cmp/eq r3,r2", 0x1),
    ("cmp/hs r3,r2", 0x1),
    ("cmp/ge r3,r2", 0x1),
    ("cmp/hi r3,r2", 0x1),
    ("cmp/gt r3,r2", 0x1),
    ("cmp/pz r2", 0x1),
    ("cmp/pl r2", 0x1),
    ("cmp/str r3,r2", 0x1),
    ("div0s r3,r2", 0x1),
    ("div0u", 0x1),
    ("div1 r3,r2", 0x1),
    ("dmuls.l r3,r2", 0x2),
    ("dmulu.l r3,r2", 0x2),
    ("mul.l r3,r2", 0x2),
    ("muls.w r3,r2", 0x1),
    ("muls r3,r2", 0x1),
    ("mulu.w r3,r2", 0x1),
    ("mulu r3,r2", 0x1),
    ("mac.w @r5+,@r4+", 0x1),
    ("mac.l @r5+,@r4+", 0x2),
    ("neg r3,r2", 0x1),
    ("negc r3,r2", 0x1),
    ("dt r2", 0x2),
    ("exts.b r3,r2", 0x1),
    ("exts.w r3,r2", 0x1),
    ("extu.b r3,r2", 0x1),
    ("extu.w r3,r2", 0x1),
    ("and #4,r0", 0x1),
    ("and r3,r2", 0x1),
    ("and.b #4,@(r0,gbr)", 0x1),
    ("or #4,r0", 0x1),
    ("or r3,r2", 0x1),
    ("or.b #4,@(r0,gbr)", 0x1),
    ("xor #4,r0", 0x1),
    ("xor r3,r2", 0x1),
    ("xor.b #4,@(r0,gbr)", 0x1),
    ("tst #4,r0", 0x1),
    ("tst r3,r2", 0x1),
    ("tst.b #4,@(r0,gbr)", 0x1),
    ("not r3,r2", 0x1),
    ("tas.b @r4", 0x1),
    ("shal r2", 0x1),
    ("shar r2", 0x1),
    ("shll r2", 0x1),
    ("shlr r2", 0x1),
    ("shll2 r2", 0x1),
    ("shlr2 r2", 0x1),
    ("shll8 r2", 0x1),
    ("shlr8 r2", 0x1),
    ("shll16 r2", 0x1),
    ("shlr16 r2", 0x1),
    ("rotl r2", 0x1),
    ("rotr r2", 0x1),
    ("rotcl r2", 0x1),
    ("rotcr r2", 0x1),
    ("shad r3,r2", 0x16),
    ("shld r3,r2", 0x16),
    ("bt lit", 0x1),
    ("bf lit", 0x1),
    ("bt/s lit", 0x2),
    ("bt.s lit", 0x2),
    ("bf/s lit", 0x2),
    ("bf.s lit", 0x2),
    ("bra lit", 0x1),
    ("bsr lit", 0x1),
    ("braf r2", 0x2),
    ("bsrf r2", 0x2),
    ("jmp @r4", 0x1),
    ("jsr @r4", 0x1),
    ("rts", 0x1),
    ("rte", 0x1),
    ("trapa #4", 0x1),
    ("nop", 0x1),
    ("sleep", 0x1),
    ("clrmac", 0x1),
    ("clrt", 0x1),
    ("sett", 0x1),
    ("clrs", 0x14),
    ("sets", 0x14),
    ("ldtlb", 0x3),
    ("pref @r4", 0x16),
    ("ocbi @r4", 0x12),
    ("ocbp @r4", 0x12),
    ("ocbwb @r4", 0x12),
    ("icbi @r4", 0x11),
    ("prefi @r4", 0x11),
    ("synco", 0x11),
    ("ldc r2,sr", 0x1),
    ("ldc r2,gbr", 0x1),
    ("ldc r2,vbr", 0x1),
    ("ldc r2,ssr", 0x14),
    ("ldc r2,spc", 0x14),
    ("ldc r2,sgr", 0x12),
    ("ldc r2,dbr", 0x12),
    ("ldc r2,r3_bank", 0x14),
    ("ldc.l @r4+,sr", 0x1),
    ("ldc.l @r4+,gbr", 0x1),
    ("ldc.l @r4+,vbr", 0x1),
    ("ldc.l @r4+,ssr", 0x14),
    ("ldc.l @r4+,spc", 0x14),
    ("ldc.l @r4+,sgr", 0x12),
    ("ldc.l @r4+,dbr", 0x12),
    ("ldc.l @r4+,r3_bank", 0x14),
    ("stc sr,r2", 0x1),
    ("stc gbr,r2", 0x1),
    ("stc vbr,r2", 0x1),
    ("stc ssr,r2", 0x14),
    ("stc spc,r2", 0x14),
    ("stc sgr,r2", 0x12),
    ("stc dbr,r2", 0x12),
    ("stc r3_bank,r2", 0x14),
    ("stc.l sr,@-r4", 0x1),
    ("stc.l gbr,@-r4", 0x1),
    ("stc.l vbr,@-r4", 0x1),
    ("stc.l ssr,@-r4", 0x14),
    ("stc.l spc,@-r4", 0x14),
    ("stc.l sgr,@-r4", 0x12),
    ("stc.l dbr,@-r4", 0x12),
    ("stc.l r3_bank,@-r4", 0x14),
    ("lds r2,mach", 0x1),
    ("lds r2,macl", 0x1),
    ("lds r2,pr", 0x1),
    ("lds r2,fpul", 0xb),
    ("lds r2,fpscr", 0xb),
    ("lds.l @r4+,mach", 0x1),
    ("lds.l @r4+,macl", 0x1),
    ("lds.l @r4+,pr", 0x1),
    ("lds.l @r4+,fpul", 0xb),
    ("lds.l @r4+,fpscr", 0xb),
    ("sts mach,r2", 0x1),
    ("sts macl,r2", 0x1),
    ("sts pr,r2", 0x1),
    ("sts fpul,r2", 0xb),
    ("sts fpscr,r2", 0xb),
    ("sts.l mach,@-r4", 0x1),
    ("sts.l macl,@-r4", 0x1),
    ("sts.l pr,@-r4", 0x1),
    ("sts.l fpul,@-r4", 0xb),
    ("sts.l fpscr,@-r4", 0xb),
    ("fabs fr2", 0xb),
    ("fabs dr2", 0x17),
    ("fadd fr3,fr2", 0xb),
    ("fadd dr4,dr2", 0x17),
    ("fsub fr3,fr2", 0xb),
    ("fsub dr4,dr2", 0x17),
    ("fmul fr3,fr2", 0xb),
    ("fmul dr4,dr2", 0x17),
    ("fdiv fr3,fr2", 0xb),
    ("fdiv dr4,dr2", 0x17),
    ("fcmp/eq fr3,fr2", 0xb),
    ("fcmp/eq dr4,dr2", 0x17),
    ("fcmp/gt fr3,fr2", 0xb),
    ("fcmp/gt dr4,dr2", 0x17),
    ("fneg fr2", 0xb),
    ("fneg dr2", 0x17),
    ("fsqrt fr2", 0x18),
    ("fsqrt dr2", 0x17),
    ("fldi0 fr2", 0xb),
    ("fldi1 fr2", 0xb),
    ("flds fr2,fpul", 0xb),
    ("fsts fpul,fr2", 0xb),
    ("float fpul,fr2", 0xb),
    ("float fpul,dr2", 0x17),
    ("ftrc fr2,fpul", 0xb),
    ("ftrc dr2,fpul", 0x17),
    ("fcnvds dr2,fpul", 0x17),
    ("fcnvsd fpul,dr2", 0x17),
    ("fmac fr0,fr3,fr2", 0xb),
    ("fsca fpul,dr2", 0x9),
    ("fsrra fr2", 0x9),
    ("fipr fv8,fv4", 0x9),
    ("ftrv xmtrx,fv4", 0x9),
    ("frchg", 0x9),
    ("fschg", 0x17),
    ("fpchg", 0xc),
    ("fmov fr3,fr2", 0xb),
    ("fmov @r5,fr2", 0xb),
    ("fmov fr3,@r4", 0xb),
    ("fmov @r5+,fr2", 0xb),
    ("fmov fr3,@-r4", 0xb),
    ("fmov @(r0,r5),fr2", 0xb),
    ("fmov fr3,@(r0,r4)", 0xb),
    ("fmov dr4,dr2", 0x17),
    ("fmov @r5,dr2", 0x17),
    ("fmov dr4,@r4", 0x17),
    ("fmov @r5+,dr2", 0x17),
    ("fmov dr4,@-r4", 0x17),
    ("fmov @(r0,r5),dr2", 0x17),
    ("fmov dr4,@(r0,r4)", 0x17),
    ("fmov.s @r5,fr2", 0xb),
    ("fmov.s fr3,@r4", 0xb),
    ("fmov.s @r5+,fr2", 0xb),
    ("fmov.s fr3,@-r4", 0xb),
    ("fmov.s @(r0,r5),fr2", 0xb),
    ("fmov.s fr3,@(r0,r4)", 0xb),
    ("fmov.d @r5,dr2", 0x17),
    ("fmov.d dr4,@r4", 0x17),
    ("fmov.d @r5+,dr2", 0x17),
    ("fmov.d dr4,@-r4", 0x17),
    ("fmov.d @(r0,r5),dr2", 0x17),
    ("fmov.d dr4,@(r0,r4)", 0x17),
];

const SET_PAIRS: &[(&str, &str, u32)] = &[
    ("mov #4,r2", "movca.l r0,@r4", 0x12),
    ("mov #4,r2", "movli.l @r4,r0", 0x11),
    ("mov #4,r2", "dmuls.l r3,r2", 0x2),
    ("mov #4,r2", "shad r3,r2", 0x16),
    ("mov #4,r2", "clrs", 0x14),
    ("mov #4,r2", "ldtlb", 0x3),
    ("mov #4,r2", "lds r2,fpul", 0xb),
    ("mov #4,r2", "fabs dr2", 0x17),
    ("mov #4,r2", "fsqrt fr2", 0x18),
    ("mov #4,r2", "fsca fpul,dr2", 0x9),
    ("mov #4,r2", "fpchg", 0xc),
    ("movca.l r0,@r4", "movli.l @r4,r0", 0x11),
    ("movca.l r0,@r4", "dmuls.l r3,r2", 0x12),
    ("movca.l r0,@r4", "shad r3,r2", 0x12),
    ("movca.l r0,@r4", "clrs", 0x12),
    ("movca.l r0,@r4", "ldtlb", 0x10),
    ("movca.l r0,@r4", "lds r2,fpul", 0x9),
    ("movca.l r0,@r4", "fabs dr2", 0x9),
    ("movca.l r0,@r4", "fsqrt fr2", 0x9),
    ("movca.l r0,@r4", "fsca fpul,dr2", 0x9),
    ("movca.l r0,@r4", "fpchg", 0xc),
    ("movli.l @r4,r0", "dmuls.l r3,r2", 0x11),
    ("movli.l @r4,r0", "shad r3,r2", 0x11),
    ("movli.l @r4,r0", "clrs", 0x11),
    ("movli.l @r4,r0", "ldtlb", 0x11),
    ("movli.l @r4,r0", "lds r2,fpul", 0xc),
    ("movli.l @r4,r0", "fabs dr2", 0xc),
    ("movli.l @r4,r0", "fsqrt fr2", 0xc),
    ("movli.l @r4,r0", "fsca fpul,dr2", 0xc),
    ("movli.l @r4,r0", "fpchg", 0xc),
    ("dmuls.l r3,r2", "shad r3,r2", 0x16),
    ("dmuls.l r3,r2", "clrs", 0x14),
    ("dmuls.l r3,r2", "ldtlb", 0x3),
    ("dmuls.l r3,r2", "lds r2,fpul", 0xb),
    ("dmuls.l r3,r2", "fabs dr2", 0x17),
    ("dmuls.l r3,r2", "fsqrt fr2", 0x18),
    ("dmuls.l r3,r2", "fsca fpul,dr2", 0x9),
    ("dmuls.l r3,r2", "fpchg", 0xc),
    ("shad r3,r2", "clrs", 0x14),
    ("shad r3,r2", "ldtlb", 0x3),
    ("shad r3,r2", "lds r2,fpul", 0x18),
    ("shad r3,r2", "fabs dr2", 0x17),
    ("shad r3,r2", "fsqrt fr2", 0x18),
    ("shad r3,r2", "fsca fpul,dr2", 0x9),
    ("shad r3,r2", "fpchg", 0xc),
    ("clrs", "ldtlb", 0x3),
    ("clrs", "lds r2,fpul", 0x8),
    ("clrs", "fabs dr2", 0x9),
    ("clrs", "fsqrt fr2", 0x8),
    ("clrs", "fsca fpul,dr2", 0x9),
    ("clrs", "fpchg", 0xc),
    ("ldtlb", "lds r2,fpul", 0x8),
    ("ldtlb", "fabs dr2", 0x9),
    ("ldtlb", "fsqrt fr2", 0x8),
    ("ldtlb", "fsca fpul,dr2", 0x9),
    ("ldtlb", "fpchg", 0xc),
    ("lds r2,fpul", "fabs dr2", 0x17),
    ("lds r2,fpul", "fsqrt fr2", 0x18),
    ("lds r2,fpul", "fsca fpul,dr2", 0x9),
    ("lds r2,fpul", "fpchg", 0xc),
    ("fabs dr2", "fsqrt fr2", 0x17),
    ("fabs dr2", "fsca fpul,dr2", 0x9),
    ("fabs dr2", "fpchg", 0xc),
    ("fsqrt fr2", "fsca fpul,dr2", 0x9),
    ("fsqrt fr2", "fpchg", 0xc),
    ("fsca fpul,dr2", "fpchg", 0xc),
];

const WHOLE_FILES: &[(&str, u32)] = &[
    ("", 0x1),
    (".long 1", 0x1),
    (".data\n.long 1\n.byte 2", 0x1),
    ("l: .align 2", 0x1),
    (".section .foo,\"ax\"\n.word 9", 0x1),
    (".text\n.space 16", 0x1),
    (".bss\n.space 4", 0x1),
    ("x = 3", 0x1),
    (
        "nop\n.section .t2,\"ax\"\nmovca.l r0,@r1\n.data\n.long 0",
        0x12,
    ),
    (".section .t2,\"ax\"\nfsqrt fr1\n.text\nldtlb", 0x8),
    ("ldtlb\nmovca.l r0,@r1", 0x10),
    ("clrs\nfmov.s @r1,fr2", 0x8),
    ("movli.l @r1,r0\nfadd fr2,fr4\n.long 0", 0xc),
    ("dt r1\npref @r2\nfsqrt fr3", 0x18),
    ("bt/s far\nnop\n.space 1000\nfar: rts\nnop", 0x2),
    ("bt far\n.space 1000\nfar: rts\nnop", 0x1),
];

const CPU_NAMES: &[(&str, &str, u32)] = &[
    ("sh1", "", 0x1),
    ("sh1", "nop", 0x1),
    ("sh2", "", 0x2),
    ("sh2", "nop", 0x2),
    ("sh2", "dt r1", 0x2),
    ("sh2e", "", 0xb),
    ("sh2e", "nop", 0xb),
    ("sh2e", "dt r1", 0xb),
    ("sh2e", "fadd fr2,fr4", 0xb),
    ("sh3", "", 0x3),
    ("sh3", "nop", 0x3),
    ("sh3", "dt r1", 0x3),
    ("sh3", "ldtlb", 0x3),
    ("sh3e", "", 0x8),
    ("sh3e", "nop", 0x8),
    ("sh3e", "dt r1", 0x8),
    ("sh3e", "ldtlb", 0x8),
    ("sh3e", "fadd fr2,fr4", 0x8),
    ("sh3e", "fsqrt fr1", 0x8),
    ("sh4", "", 0x9),
    ("sh4", "nop", 0x9),
    ("sh4", "dt r1", 0x9),
    ("sh4", "ldtlb", 0x9),
    ("sh4", "fadd fr2,fr4", 0x9),
    ("sh4", "fsqrt fr1", 0x9),
    ("sh4", "movca.l r0,@r1", 0x9),
    ("sh4", "fipr fv0,fv4", 0x9),
    ("sh4a", "", 0xc),
    ("sh4a", "nop", 0xc),
    ("sh4a", "dt r1", 0xc),
    ("sh4a", "ldtlb", 0xc),
    ("sh4a", "fadd fr2,fr4", 0xc),
    ("sh4a", "fsqrt fr1", 0xc),
    ("sh4a", "movca.l r0,@r1", 0xc),
    ("sh4a", "fipr fv0,fv4", 0xc),
    ("sh4a", "synco", 0xc),
];

// ---- diagnostics ----------------------------------------------------------------

#[test]
fn bad_operands_are_diagnosed() {
    let e = errors_for("sh", "mov 1, r0");
    assert!(e.contains("needs a `#`"), "{e}");
    let e = errors_for("sh", "jmp r1");
    assert!(e.contains("jmp @rn"), "{e}");
    let e = errors_for("sh", "mova lit, r1\nlit: .long 0");
    assert!(e.contains("mova label,r0"), "{e}");
    let e = errors_for("sh", "mov.l @(r1,r2), r3");
    assert!(e.contains("@(r0,"), "{e}");
    let e = errors_for("sh", "mov.l @-sr, r3");
    assert!(e.contains("r0`-`r15"), "{e}");
    let e = errors_for("sh", "mov.l tbr, r0");
    assert!(e.contains("SH-2A"), "{e}");
    let e = errors_for("sh", "bra");
    assert!(e.contains("bra label"), "{e}");
}

const BAD: &[&str] = &[
    "",
    "@",
    ",",
    "mov",
    "mov ,",
    "mov r1,",
    "mov , r1",
    "mov #",
    "mov #, r0",
    "mov @",
    "mov.l @(",
    "mov.l @()",
    "mov.l @(,)",
    "mov.l @(4",
    "mov.l @(4,",
    "mov.l @(4,r1",
    "mov.l @(4 r1), r2",
    "mov.l @(r0",
    "mov.l @(r0,",
    "mov.l @(r0,4), r1",
    "mov.l @(4,sr), r1",
    "mov.l @-, r1",
    "mov.l @+r1, r2",
    "mov.l @r1++, r2",
    "mov.l @r16, r2",
    "cmp/",
    "cmp//eq r1, r2",
    "cmp/1 r1, r2",
    "/eq",
    "bt",
    "bt/s",
    "bt/s/x 4",
    "bra #",
    "bra @r1",
    "bra 1",
    "bt 3",
    "bt undefined_symbol",
    "mov.l undefined_symbol, r0",
    "mov.w undefined_symbol, r0",
    "mov.l @(undefined_symbol,pc), r0",
    "mov.l @(a-b,pc), r0\na: b:",
    "mov.l @(99999999999,pc), r0",
    "mov.l @(-99999999999,r1), r0",
    "mov #0x7fffffffffffffff, r0",
    "trapa #-9223372036854775807",
    "mov.l @(1f,r1), r0\n1:",
    "fmov fr16, fr1",
    "fipr fv1, fv2",
    "ldc r1, r9_bank",
    "ldc.l @r1+, pc",
    "stc pc, r1",
    "mov.b #1, @(r0,gbr)",
    "xor.b #1, @(r1,gbr)",
    "and #1, r1",
    "mov a0, r0",
    "\u{1F600}",
    "mov r1, \u{1F600}",
];

#[test]
fn malformed_input_never_panics() {
    for arch in ["sh", "shl", "sh1"] {
        for src in BAD {
            // Either outcome is fine; a panic or a hang is not.
            let _ = try_text_for(arch, src);
        }
    }
}

#[test]
fn malformed_input_is_reported() {
    for arch in ["sh", "shl"] {
        for src in &BAD[1..] {
            assert!(
                try_text_for(arch, src).is_err(),
                "`{src}` should not assemble for {arch}"
            );
        }
    }
}

#[test]
fn a_flat_image_at_a_real_load_address_lays_out_the_same() {
    // The boundary checks look at the absolute address, which on a
    // four-byte-aligned base agrees with the section offset GNU as uses.
    let src = "nop\nmov.l lit, r1\njmp @r1\nnop\nlit: .long 0x89abcdef";
    let asm = assemble_flat_for("sh", src, 0x8c01_0000);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(
        hex(&asm.section_bytes(rsasm::section::SectionId(0))),
        "00 09 d1 01 41 2b 00 09 89 ab cd ef"
    );
}

#[test]
fn addends_live_in_the_field_except_for_dir16() {
    // sh-elf-as: DIR32 `x + 0` over field 8, DIR16 `x + 3` over a zero field,
    // DIR8 `x + 0` over field 1, REL32 `x + 0` over field 4.
    let asm = assemble_for(
        "sh",
        ".data\n.long x+8\n.word x+3\n.byte x+1\n.byte 0\n.long x-.+4\n",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let got: Vec<(u64, u32, i64)> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.addend))
        .collect();
    assert_eq!(got, vec![(0, 1, 0), (4, 33, 3), (6, 34, 0), (8, 2, 0)]);
    assert_eq!(
        hex(&section(&asm, ".data")),
        "00 00 00 08 00 00 01 00 00 00 00 04"
    );
}

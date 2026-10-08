//! SPARC encoding tests.
//!
//! Every expected byte string here came out of a `tools/mc-diff/run.sh sparc`
//! run, which compares rsasm against `llvm-mc -triple=sparc`. The V9 cases at
//! the bottom cannot go through that harness — its shared `ARCHES` table maps
//! the `sparc` key to the 32-bit triple, and llvm-mc rejects every V9
//! mnemonic there — so those were checked by hand against
//! `llvm-mc -triple=sparcv9` instead.

#![cfg(feature = "sparc")]

mod common;
use common::*;
use rsasm::arch;

/// Asserts that `src` assembles for V8 to `want`, written as hex bytes.
#[track_caller]
fn enc(src: &str, want: &str) {
    let got = hex(&text_for("sparc", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// The same, for a V9 target.
#[track_caller]
fn enc9(src: &str, want: &str) {
    let got = hex(&text_for("sparcv9", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

// ---- the three instruction formats ----------------------------------------

#[test]
fn format_1_is_call_and_its_30_bit_word_displacement() {
    // `call` reaches by word offset, so a jump to the next instruction is 1.
    enc(
        "fwd:\n call fwd\n call end\n nop\nend:\n nop",
        "40 00 00 00 40 00 00 02 01 00 00 00 01 00 00 00",
    );
}

#[test]
fn format_2_is_sethi_and_the_branches() {
    enc("sethi 0, %g0", "01 00 00 00");
    enc("sethi 1023, %g1", "03 00 03 ff");
    enc("sethi 4194303, %g1", "03 3f ff ff");
    enc("sethi %hi(0x40000), %o0", "11 00 01 00");
}

#[test]
fn format_3_selects_a_register_or_an_immediate_with_the_i_bit() {
    // Bit 13 clear: the second source is `%g2`.
    enc("add %g1, %g2, %g3", "86 00 40 02");
    // Bit 13 set: the second source is a 13-bit signed immediate.
    enc("add %g1, 1, %g2", "84 00 60 01");
    enc("add %g1, -1, %g2", "84 00 7f ff");
    enc("add %g1, 4095, %g2", "84 00 6f ff");
    enc("add %g1, -4096, %g2", "84 00 70 00");
}

// ---- registers -------------------------------------------------------------

#[test]
fn the_four_register_windows_are_numbered_in_order() {
    enc("add %g0, %g7, %o0", "90 00 00 07");
    enc("add %o0, %o7, %l0", "a0 02 00 0f");
    enc("add %l0, %l7, %i0", "b0 04 00 17");
    enc("add %i0, %i7, %g1", "82 06 00 1f");
}

#[test]
fn sp_fp_and_the_flat_r_names_are_the_same_registers() {
    // `%sp` is `%o6` (r14) and `%fp` is `%i6` (r30).
    assert_eq!(
        text_for("sparc", "add %sp, %fp, %g1"),
        text_for("sparc", "add %o6, %i6, %g1")
    );
    assert_eq!(
        text_for("sparc", "add %sp, %fp, %g1"),
        text_for("sparc", "add %r14, %r30, %g1")
    );
    enc("add %sp, %fp, %g1", "82 03 80 1e");
    enc("add %r0, %r15, %r16", "a0 00 00 0f");
    // llvm-mc reads `%r31` as the start of a relocation specifier and rejects
    // it, so this spelling cannot go through the differential harness.
    assert_eq!(
        text_for("sparc", "add %r31, %g0, %g1"),
        text_for("sparc", "add %i7, %g0, %g1")
    );
}

#[test]
fn float_registers_have_their_own_numbering() {
    enc("fadds %f0, %f1, %f2", "85 a0 08 21");
    enc("fadds %f30, %f31, %f28", "b9 a7 88 3f");
}

// ---- arithmetic and logic --------------------------------------------------

#[test]
fn the_arithmetic_and_logic_group() {
    enc("add %g1, %g2, %g3", "86 00 40 02");
    enc("addcc %g1, %g2, %g3", "86 80 40 02");
    enc("addx %g1, %g2, %g3", "86 40 40 02");
    enc("addxcc %g1, %g2, %g3", "86 c0 40 02");
    enc("sub %g1, %g2, %g3", "86 20 40 02");
    enc("subcc %g1, %g2, %g3", "86 a0 40 02");
    enc("subx %g1, %g2, %g3", "86 60 40 02");
    enc("subxcc %g1, %g2, %g3", "86 e0 40 02");
    enc("and %g1, %g2, %g3", "86 08 40 02");
    enc("andcc %g1, %g2, %g3", "86 88 40 02");
    enc("andn %g1, %g2, %g3", "86 28 40 02");
    enc("andncc %g1, %g2, %g3", "86 a8 40 02");
    enc("or %g1, %g2, %g3", "86 10 40 02");
    enc("orcc %g1, %g2, %g3", "86 90 40 02");
    enc("orn %g1, %g2, %g3", "86 30 40 02");
    enc("orncc %g1, %g2, %g3", "86 b0 40 02");
    enc("xor %g1, %g2, %g3", "86 18 40 02");
    enc("xorcc %g1, %g2, %g3", "86 98 40 02");
    enc("xnor %g1, %g2, %g3", "86 38 40 02");
    enc("xnorcc %g1, %g2, %g3", "86 b8 40 02");
    enc("umul %g1, %g2, %g3", "86 50 40 02");
    enc("umulcc %g1, %g2, %g3", "86 d0 40 02");
    enc("smul %g1, %g2, %g3", "86 58 40 02");
    enc("smulcc %g1, %g2, %g3", "86 d8 40 02");
    enc("udiv %g1, %g2, %g3", "86 70 40 02");
    enc("udivcc %g1, %g2, %g3", "86 f0 40 02");
    enc("sdiv %g1, %g2, %g3", "86 78 40 02");
    enc("sdivcc %g1, %g2, %g3", "86 f8 40 02");
}

#[test]
fn v9_renamed_the_carry_forms_but_kept_the_opcodes() {
    assert_eq!(
        text_for("sparc", "addc %g1, %g2, %g3"),
        text_for("sparc", "addx %g1, %g2, %g3")
    );
    assert_eq!(
        text_for("sparc", "subc %g1, %g2, %g3"),
        text_for("sparc", "subx %g1, %g2, %g3")
    );
    assert_eq!(
        text_for("sparc", "addccc %g1, %g2, %g3"),
        text_for("sparc", "addxcc %g1, %g2, %g3")
    );
}

#[test]
fn shifts() {
    enc("sll %g1, %g2, %g3", "87 28 40 02");
    enc("srl %g1, %g2, %g3", "87 30 40 02");
    enc("sra %g1, %g2, %g3", "87 38 40 02");
    enc("sll %g1, 0, %g2", "85 28 60 00");
    enc("sll %g1, 3, %g2", "85 28 60 03");
    enc("srl %g1, 16, %g2", "85 30 60 10");
    enc("sra %g1, 31, %g2", "85 38 60 1f");
}

// ---- sethi, %hi and %lo ----------------------------------------------------

#[test]
fn hi_and_lo_split_a_constant_the_way_sethi_needs() {
    // 0x12345 = (0x48 << 10) | 0x345.
    enc("sethi %hi(0x12345), %o0", "11 00 00 48");
    enc("or %o0, %lo(0x12345), %o0", "90 12 23 45");
    enc("add %g1, %lo(0x12345), %g2", "84 00 63 45");
    enc("ld [%g1 + %lo(0x12345)], %g2", "c4 00 63 45");
}

#[test]
fn hi_of_a_symbol_becomes_a_relocation_with_a_zero_field() {
    // The opcode bits are still there; only the 22-bit field is left blank.
    enc("sethi %hi(elsewhere), %o0", "11 00 00 00");
    enc("or %o0, %lo(elsewhere), %o0", "90 12 20 00");
}

// ---- loads and stores ------------------------------------------------------

#[test]
fn loads_and_stores_in_all_three_addressing_forms() {
    enc("ld [%o0], %o1", "d2 02 00 00");
    enc("ld [%o0 + 4], %o1", "d2 02 20 04");
    enc("ld [%o0 - 4], %o1", "d2 02 3f fc");
    enc("ld [%o0 + %o1], %o2", "d4 02 00 09");
    enc("ldub [%g1], %g2", "c4 08 40 00");
    enc("ldsb [%g1], %g2", "c4 48 40 00");
    enc("lduh [%g1 + 2], %g2", "c4 10 60 02");
    enc("ldsh [%g1 + 2], %g2", "c4 50 60 02");
    enc("ldd [%sp], %o2", "d4 1b 80 00");
    enc("st %o1, [%o0]", "d2 22 00 00");
    enc("st %o1, [%sp + 64]", "d2 23 a0 40");
    enc("st %o1, [%fp - 8]", "d2 27 bf f8");
    enc("stb %g3, [%g1 + %g2]", "c6 28 40 02");
    enc("sth %g3, [%g1 + 2]", "c6 30 60 02");
    enc("std %o2, [%sp]", "d4 3b 80 00");
}

#[test]
fn a_bare_bracket_form_is_not_the_same_word_as_an_explicit_zero() {
    // `[%o0]` sets `i` = 0 and `rs2` = `%g0`; `[%o0 + 0]` sets `i` = 1.
    enc("ld [%o0], %o1", "d2 02 00 00");
    enc("ld [%o0 + 0], %o1", "d2 02 20 00");
}

#[test]
fn ld_and_st_pick_the_float_opcode_from_the_register_class() {
    enc("ld [%o0], %f1", "c3 02 00 00");
    enc("st %f1, [%o0]", "c3 22 00 00");
    enc("ldd [%o0], %f0", "c1 1a 00 00");
    enc("std %f0, [%o0]", "c1 3a 00 00");
    // GNU as also spells these out; llvm-mc has no such mnemonics, so they
    // are checked against the forms above rather than through the harness.
    assert_eq!(
        text_for("sparc", "ldf [%o0], %f1"),
        text_for("sparc", "ld [%o0], %f1")
    );
    assert_eq!(
        text_for("sparc", "stf %f1, [%o0]"),
        text_for("sparc", "st %f1, [%o0]")
    );
    assert_eq!(
        text_for("sparc", "lddf [%o0], %f0"),
        text_for("sparc", "ldd [%o0], %f0")
    );
    assert_eq!(
        text_for("sparc", "stdf %f0, [%o0]"),
        text_for("sparc", "std %f0, [%o0]")
    );
}

// ---- branches --------------------------------------------------------------

#[test]
fn every_integer_condition_branch() {
    let src = "back:\n\
               ba back\n bn back\n be back\n bne back\n\
               bg back\n ble back\n bge back\n bl back\n\
               bgu back\n bleu back\n bcc back\n bcs back\n\
               bpos back\n bneg back\n bvc back\n bvs back";
    enc(
        src,
        "10 80 00 00 00 bf ff ff 02 bf ff fe 12 bf ff fd \
         14 bf ff fc 04 bf ff fb 16 bf ff fa 06 bf ff f9 \
         18 bf ff f8 08 bf ff f7 1a bf ff f6 0a bf ff f5 \
         1c bf ff f4 0c bf ff f3 1e bf ff f2 0e bf ff f1",
    );
}

#[test]
fn the_annul_bit_is_bit_29() {
    enc("back:\n ba,a back", "30 80 00 00");
    enc("back:\n bne,a back", "32 80 00 00");
    // Condition aliases assemble to the same words as their canonical names.
    assert_eq!(
        text_for("sparc", "back:\n beq back"),
        text_for("sparc", "back:\n be back")
    );
    assert_eq!(
        text_for("sparc", "back:\n bnz back"),
        text_for("sparc", "back:\n bne back")
    );
    assert_eq!(
        text_for("sparc", "back:\n blt back"),
        text_for("sparc", "back:\n bl back")
    );
    assert_eq!(
        text_for("sparc", "back:\n bgeu back"),
        text_for("sparc", "back:\n bcc back")
    );
}

#[test]
fn a_forward_branch_counts_in_instructions() {
    enc(
        "ba fwd\n nop\n nop\nfwd:\n nop",
        "10 80 00 03 01 00 00 00 01 00 00 00 01 00 00 00",
    );
}

#[test]
fn jumps_and_returns() {
    enc("jmpl %o7 + 8, %g0", "81 c3 e0 08");
    enc("jmpl %g1 + %g2, %g3", "87 c0 40 02");
    enc("jmpl %o7, %g0", "81 c3 c0 00");
    enc("jmp %o7 + 8", "81 c3 e0 08");
    // `ret` comes back through `%i7` (the window was rotated by `save`),
    // `retl` through `%o7` (a leaf never rotated it).
    enc("ret", "81 c7 e0 08");
    enc("retl", "81 c3 e0 08");
    assert_eq!(
        text_for("sparc", "ret"),
        text_for("sparc", "jmpl %i7 + 8, %g0")
    );
    assert_eq!(
        text_for("sparc", "retl"),
        text_for("sparc", "jmpl %o7 + 8, %g0")
    );
    // An indirect `call` is `jmpl` linking into `%o7`.
    enc("call %o7", "9f c3 c0 00");
    enc("call %g1 + %g2", "9f c0 40 02");
}

// ---- register windows ------------------------------------------------------

#[test]
fn save_and_restore_rotate_the_register_window() {
    // The canonical prologue: rotate the window and drop the stack pointer by
    // 96 bytes (the minimum frame) in the same instruction.
    enc("save %sp, -96, %sp", "9d e3 bf a0");
    enc("save %g1, %g2, %g3", "87 e0 40 02");
    enc("restore %g1, %g2, %g3", "87 e8 40 02");
    // Written bare they add `%g0` to `%g0` into `%g0`, i.e. rotate only.
    enc("save", "81 e0 00 00");
    enc("restore", "81 e8 00 00");
    assert_eq!(
        text_for("sparc", "save"),
        text_for("sparc", "save %g0, %g0, %g0")
    );
}

// ---- traps and state registers ---------------------------------------------

#[test]
fn traps_and_the_y_register() {
    enc("ta 3", "91 d0 20 03");
    enc("tn 1", "81 d0 20 01");
    enc("te 3", "83 d0 20 03");
    enc("rd %y, %g1", "83 40 00 00");
    enc("wr %g1, %y", "81 80 00 01");
    enc("wr %g1, %g2, %y", "81 80 40 02");
    enc("flush %g1", "81 d8 40 00");
    enc("flush %g1 + 8", "81 d8 60 08");
    enc("unimp 0", "00 00 00 00");
}

// ---- floating point --------------------------------------------------------

#[test]
fn floating_point_arithmetic() {
    enc("fadds %f0, %f1, %f2", "85 a0 08 21");
    enc("faddd %f0, %f2, %f4", "89 a0 08 42");
    enc("faddq %f0, %f4, %f8", "91 a0 08 64");
    enc("fsubs %f0, %f1, %f2", "85 a0 08 a1");
    enc("fmuls %f0, %f1, %f2", "85 a0 09 21");
    enc("fdivs %f0, %f1, %f2", "85 a0 09 a1");
    enc("fsqrts %f1, %f2", "85 a0 05 21");
    enc("fmovs %f1, %f2", "85 a0 00 21");
    enc("fnegs %f1, %f2", "85 a0 00 a1");
    enc("fabss %f1, %f2", "85 a0 01 21");
    enc("fitos %f1, %f2", "85 a0 18 81");
    enc("fstoi %f1, %f2", "85 a0 1a 21");
    enc("fstod %f1, %f2", "85 a0 19 21");
    enc("fdtos %f0, %f2", "85 a0 18 c0");
    enc("fcmps %f0, %f1", "81 a8 0a 21");
    enc("fcmpd %f0, %f2", "81 a8 0a 42");
}

// ---- synthetics ------------------------------------------------------------

#[test]
fn the_synthetics_are_real_instructions_with_g0_in_a_slot() {
    enc("nop", "01 00 00 00");
    assert_eq!(text_for("sparc", "nop"), text_for("sparc", "sethi 0, %g0"));
    enc("mov %g1, %g2", "84 10 00 01");
    enc("mov 1, %g2", "84 10 20 01");
    enc("mov -1, %g2", "84 10 3f ff");
    enc("cmp %o0, %o1", "80 a2 00 09");
    enc("cmp %o0, 1", "80 a2 20 01");
    enc("tst %g1", "80 90 40 00");
    enc("clr %g1", "82 10 00 00");
    enc("clr [%o0]", "c0 22 00 00");
    enc("not %g1", "82 38 40 00");
    enc("not %g1, %g2", "84 38 40 00");
    enc("neg %g1", "82 20 00 01");
    enc("neg %g1, %g2", "84 20 00 01");
    enc("inc %g1", "82 00 60 01");
    enc("inc 4, %g1", "82 00 60 04");
    enc("dec %g1", "82 20 60 01");
    enc("dec 4, %g1", "82 20 60 04");
    enc("btst 1, %g1", "80 88 60 01");
    enc("bset 2, %g1", "82 10 60 02");
    enc("bclr 4, %g1", "82 28 60 04");
    enc("btog 8, %g1", "82 18 60 08");
    // ... and each expands to exactly the instruction it stands for.
    assert_eq!(
        text_for("sparc", "mov %g1, %g2"),
        text_for("sparc", "or %g0, %g1, %g2")
    );
    assert_eq!(
        text_for("sparc", "cmp %o0, %o1"),
        text_for("sparc", "subcc %o0, %o1, %g0")
    );
    assert_eq!(
        text_for("sparc", "tst %g1"),
        text_for("sparc", "orcc %g1, %g0, %g0")
    );
    assert_eq!(
        text_for("sparc", "clr [%o0]"),
        text_for("sparc", "st %g0, [%o0]")
    );
    assert_eq!(
        text_for("sparc", "not %g1"),
        text_for("sparc", "xnor %g1, %g0, %g1")
    );
    assert_eq!(
        text_for("sparc", "neg %g1"),
        text_for("sparc", "sub %g0, %g1, %g1")
    );
    assert_eq!(
        text_for("sparc", "btst 1, %g1"),
        text_for("sparc", "andcc %g1, 1, %g0")
    );
}

#[test]
fn mov_to_and_from_y_is_a_different_instruction_entirely() {
    assert_eq!(
        text_for("sparc", "mov %y, %g1"),
        text_for("sparc", "rd %y, %g1")
    );
    assert_eq!(
        text_for("sparc", "mov %g1, %y"),
        text_for("sparc", "wr %g1, %y")
    );
}

#[test]
fn set_expands_to_one_or_two_instructions_by_the_value() {
    // Fits `simm13`: one `or`.
    enc("set 0, %o0", "90 10 20 00");
    enc("set 1, %o0", "90 10 20 01");
    enc("set 4095, %o0", "90 10 2f ff");
    enc("set -4096, %o0", "90 10 30 00");
    enc("set -1, %o0", "90 10 3f ff");
    // 0xffffffff is the same 32-bit value as -1.
    enc("set 0xffffffff, %o0", "90 10 3f ff");
    // Low ten bits clear: one `sethi`.
    enc("set 4096, %o0", "11 00 00 04");
    enc("set 0x40000, %o0", "11 00 01 00");
    enc("set 0x80000000, %o0", "11 20 00 00");
    // Neither: `sethi` plus `or`.
    enc("set 0x12345, %o0", "11 00 00 48 90 12 23 45");
    enc("set -4097, %o0", "11 3f ff fb 90 12 23 ff");
    enc("set -5000, %o0", "11 3f ff fb 90 12 20 78");
    enc("set 2147483647, %o0", "11 1f ff ff 90 12 23 ff");
    // A symbol's value is unknown, so it always takes the two-word form.
    enc("set elsewhere, %o0", "11 00 00 00 90 12 20 00");
}

// ---- alignment and object-level properties ---------------------------------

#[test]
fn alignment_padding_in_a_text_section_is_nops() {
    enc(
        "nop\n.align 16\nnop",
        "01 00 00 00 01 00 00 00 01 00 00 00 01 00 00 00 01 00 00 00",
    );
}

#[test]
fn instructions_are_written_big_endian() {
    // `sethi %hi(0x40000), %o0` is the word 0x11000100; a little-endian
    // backend would emit it reversed.
    assert_eq!(
        text_for("sparc", "sethi %hi(0x40000), %o0"),
        vec![0x11, 0x00, 0x01, 0x00]
    );
}

#[test]
fn the_elf_machine_and_pointer_width_follow_the_target() {
    let v8 = arch::lookup("sparc").expect("no `sparc` backend");
    let v9 = arch::lookup("sparcv9").expect("no `sparcv9` backend");
    assert_eq!(v8.elf_machine(), 2); // EM_SPARC
    assert_eq!(v9.elf_machine(), 43); // EM_SPARCV9
    assert_eq!(v8.pointer_bytes(&v8.initial_state()), 4);
    assert_eq!(v9.pointer_bytes(&v9.initial_state()), 8);
    // R_SPARC_32 / R_SPARC_64 / R_SPARC_DISP32.
    assert_eq!(v8.data_reloc(4, false), Some(3));
    assert_eq!(v8.data_reloc(8, false), Some(32));
    assert_eq!(v8.data_reloc(4, true), Some(6));
}

// ---- V9 --------------------------------------------------------------------
//
// Checked against `llvm-mc -triple=sparcv9`, one instruction at a time; the
// mc-diff harness only has a 32-bit SPARC row.

#[test]
fn v9_64_bit_arithmetic_and_memory() {
    enc9("mulx %g1, %g2, %g3", "86 48 40 02");
    enc9("mulx %g1, 5, %g3", "86 48 60 05");
    enc9("sdivx %g1, %g2, %g3", "87 68 40 02");
    enc9("udivx %g1, %g2, %g3", "86 68 40 02");
    enc9("ldx [%g1 + %g2], %g3", "c6 58 40 02");
    enc9("ldx [%sp + 2047], %g2", "c4 5b a7 ff");
    enc9("stx %g3, [%sp - 8]", "c6 73 bf f8");
    enc9("ldsw [%g1 + %g2], %g3", "c6 40 40 02");
}

#[test]
fn v9_shifts_set_the_x_bit() {
    // `sllx` is `sll` with bit 12 set and a six-bit count.
    enc9("sllx %g1, 3, %g2", "85 28 70 03");
    enc9("srlx %g1, 3, %g2", "85 30 70 03");
    enc9("srax %g1, 3, %g2", "85 38 70 03");
    enc9("sllx %g1, 63, %g2", "85 28 70 3f");
    enc9("sllx %g1, %g2, %g3", "87 28 50 02");
}

#[test]
fn v9_branch_on_register_splits_its_displacement() {
    enc9("back:\n brz %g1, back", "02 c8 40 00");
    enc9("back:\n brnz %g1, back", "0a c8 40 00");
    enc9("back:\n brlz %g1, back", "06 c8 40 00");
    enc9("back:\n brgz %g1, back", "0c c8 40 00");
    enc9("back:\n brlez %g1, back", "04 c8 40 00");
    enc9("back:\n brgez %g1, back", "0e c8 40 00");
    enc9("back:\n brz,a %g1, back", "22 c8 40 00");
    // `,pn` clears the predict bit, which is otherwise 1.
    enc9("back:\n brz,pn %g1, back", "02 c0 40 00");
    // The 16-bit field really is split: bits 15-14 sit above `rs1`.
    enc9("back:\n nop\n brz %g1, back", "01 00 00 00 02 f8 7f ff");
}

#[test]
fn v9_predicted_branches_take_a_condition_code_bank() {
    enc9("back:\n be %icc, back", "02 48 00 00");
    enc9("back:\n bne %icc, back", "12 48 00 00");
    enc9("back:\n be %xcc, back", "02 68 00 00");
    enc9("back:\n be,pn %icc, back", "02 40 00 00");
    enc9("back:\n be,a %icc, back", "22 48 00 00");
    enc9("back:\n be,a,pn %icc, back", "22 40 00 00");
    // The `bp<cc>` spelling GNU as accepts means the same thing; llvm-mc has
    // no such mnemonic, so it is compared against `be %icc` here.
    assert_eq!(
        text_for("sparcv9", "back:\n bpe %icc, back"),
        text_for("sparcv9", "back:\n be %icc, back")
    );
}

#[test]
fn v9_conditional_moves() {
    enc9("movne %icc, 1, %g1", "83 66 60 01");
    enc9("move %icc, %g2, %g1", "83 64 40 02");
    enc9("movl %icc, 3, %g1", "83 64 e0 03");
    enc9("movgu %xcc, %g2, %g1", "83 67 10 02");
    enc9("mova %xcc, -1024, %g1", "83 66 34 00");
    enc9("movrz %g1, %g2, %g3", "87 78 44 02");
    enc9("movrnz %g1, %g2, %g3", "87 78 54 02");
    enc9("movrlez %g1, 5, %g3", "87 78 68 05");
}

#[test]
fn v9_return_and_the_double_precision_moves() {
    enc9("return %i7 + 8", "81 cf e0 08");
    enc9("return %i7", "81 cf c0 00");
    enc9("fmovd %f0, %f2", "85 a0 00 40");
    enc9("fnegd %f0, %f2", "85 a0 00 c0");
    enc9("fabsd %f0, %f2", "85 a0 01 40");
}

#[test]
fn v9_only_instructions_are_refused_on_a_v8_target() {
    for src in [
        "mulx %g1, %g2, %g3",
        "ldx [%g1], %g2",
        "sllx %g1, 1, %g2",
        "brz %g1, .",
        "movne %icc, 1, %g1",
        "fmovd %f0, %f2",
        "be %icc, .",
    ] {
        let e = errors_for("sparc", src);
        assert!(
            e.contains("V9"),
            "expected a V9 diagnostic for `{src}`, got:\n{e}"
        );
    }
}

#[test]
fn unresolved_references_become_the_matching_r_sparc_relocations() {
    // Relocation numbers as llvm-mc -triple=sparcv9 emits them for the same
    // source (checked with llvm-readobj). V9 is used because the core only
    // writes ELF64 so far.
    let asm = assemble_for(
        "sparcv9",
        "call external\n nop\n sethi %hi(external), %o0\n or %o0, %lo(external), %o0\n\
         be external\n nop\n brz %g1, external\n be %xcc, external",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let got: Vec<(u64, u32)> = asm.relocs.iter().map(|r| (r.offset, r.kind)).collect();
    assert_eq!(
        got,
        vec![
            (0, 7),   // R_SPARC_WDISP30
            (8, 9),   // R_SPARC_HI22
            (12, 12), // R_SPARC_LO10
            (16, 8),  // R_SPARC_WDISP22
            (24, 40), // R_SPARC_WDISP16
            (28, 41), // R_SPARC_WDISP19
        ]
    );
}

// ---- diagnostics -----------------------------------------------------------

#[test]
fn out_of_range_values_name_their_limit() {
    let e = errors_for("sparc", "add %g1, 5000, %g2");
    assert!(e.contains("13-bit signed field (-4096..4095)"), "{e}");
    let e = errors_for("sparc", "sll %g1, 32, %g2");
    assert!(e.contains("shift count 32 is out of range (0..31)"), "{e}");
    let e = errors_for("sparcv9", "sllx %g1, 64, %g2");
    assert!(e.contains("(0..63)"), "{e}");
    let e = errors_for("sparc", "set 0x100000000, %o0");
    assert!(e.contains("does not fit in 32 bits"), "{e}");
}

#[test]
fn branch_range_and_alignment_are_checked() {
    // A `Bicc` displacement is 22 bits of word offset: +-8 MiB.
    let e = errors_for("sparc", "ba . + 0x800000");
    assert!(e.contains("out of range"), "{e}");
    text_for("sparc", "ba . + 0x7ffffc");
    // A target that is not a multiple of four cannot be encoded at all.
    let e = errors_for("sparc", "ba . + 1");
    assert!(e.contains("not a multiple of 4"), "{e}");
}

#[test]
fn misused_operands_are_reported_rather_than_guessed() {
    for (src, needle) in [
        ("add %q9, %g2, %g3", "unknown register"),
        ("ld %o0, %o1", "brackets"),
        ("add %g1, %hi(4), %g2", "%hi()"),
        ("add %g1, %g2", "operand"),
        ("fadds %g1, %g2, %g3", "float register"),
        ("add %f1, %g2, %g3", "integer register"),
        ("zzz %g1", "unknown instruction"),
        ("sethi %lo(4), %g1", "high 22 bits"),
    ] {
        let e = errors_for("sparc", src);
        assert!(
            e.contains(needle),
            "`{src}` should mention `{needle}`, got:\n{e}"
        );
    }
}

/// Nonsense in some way. The only requirement is that the assembler reports
/// something and returns, rather than panicking or hanging.
#[test]
fn malformed_input_never_panics() {
    const BAD: &[&str] = &[
        "add",
        "add ,",
        "add , ,",
        "add %",
        "add %%",
        "add %g1",
        "add %g1,",
        "add %g1, ,%g2",
        "add %g1 %g2 %g3",
        "add %g1, %g2, %g3, %g4",
        "add %g99, %g2, %g3",
        "add %, %g2, %g3",
        "ld [",
        "ld []",
        "ld [%",
        "ld [%o0",
        "ld [%o0 +",
        "ld [%o0 + ], %o1",
        "ld [%o0 + %], %o1",
        "ld [%o0 %o1], %o2",
        "ld [4], %o1",
        "ld [%f0], %o1",
        "ld [%icc], %o1",
        "st [%o0], %o1",
        "sethi",
        "sethi %hi(",
        "sethi %hi()",
        "sethi %hi(1",
        "sethi %lo(1), ",
        "sethi 1, 2",
        "set",
        "set ,",
        "set %g1, %g2",
        "set 1",
        "set 1, 2",
        "ba",
        "ba,",
        "ba,a",
        "ba ,",
        "ba %icc",
        "ba %icc,",
        "be %g1, x",
        "be,pn x",
        "brz",
        "brz %g1",
        "call",
        "call ,",
        "call %hi(x)",
        "jmpl",
        "jmpl %o7",
        "jmpl 4, %g0",
        "save %g1",
        "save %g1, %g2",
        "restore %g1, %g2",
        "mov",
        "mov %g1",
        "mov 1, 2",
        "cmp",
        "tst",
        "tst 1",
        "clr",
        "clr 1",
        "not",
        "inc",
        "inc 1, 2",
        "btst 1",
        "rd",
        "rd %g1, %g2",
        "wr",
        "wr %y",
        "movne",
        "movne %g1, 1, %g2",
        "movrz %g1",
        "ta",
        "ta %f0",
        "unimp",
        "flush",
        "fadds",
        "fadds %f0",
        "fcmps %f0",
        "%g1",
        "%hi(1)",
        "[%g1]",
        ",",
        "( )",
        "add %g1, %lo(, %g2",
        "add %g1, 1 1, %g2",
        "ba . +",
        "ba .+.+.",
        "set . , %g1",
        "sll %g1, -1, %g2",
        "sll %g1, x, %g2",
    ];
    for src in BAD {
        let asm = assemble_for("sparc", src);
        assert!(
            asm.diags.has_errors()
                || asm
                    .section_bytes(rsasm::section::SectionId(0))
                    .len()
                    .is_multiple_of(4),
            "`{src}` produced neither a diagnostic nor a whole number of words"
        );
        let asm = assemble_for("sparcv9", src);
        let _ = asm.diags.has_errors();
    }
}

/// Expected bytes are from `llvm-mc -triple=sparc`.
#[test]
fn bang_comments_and_first_column_hash_comments() {
    assert_eq!(
        hex(&text_for("sparc", "add %g1, 1, %g2 ! bump\n")),
        "84 00 60 01"
    );
    assert_eq!(hex(&text_for("sparc", "# 1 \"x.c\"\nnop\n")), "01 00 00 00");
}

//! ARM (A32) and Thumb (T32) encoding tests.
//!
//! Every expected byte string here was produced by a run of
//! `tools/mc-diff/run.sh arm thumb`, which compares rsasm against
//! `llvm-mc -triple=armv7` and `-triple=thumbv7`, or of
//! `tools/xas-diff/run.sh arm thumb`, which compares it against GNU as for
//! `-march=armv7ve -mfpu=neon-vfpv4`. The handful of forms that cannot
//! appear in either corpus — the ones whose llvm-mc spelling needs a `#`,
//! which this assembler's GAS-dialect lexer eats as a comment — were checked
//! separately with `llvm-mc -show-encoding` and are called out where they
//! appear.

#![cfg(feature = "arm")]

mod common;
use common::*;

/// Asserts that `src` assembles for A32 to `want`, written as hex bytes.
#[track_caller]
fn enc(src: &str, want: &str) {
    let got = hex(&text_for("arm", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// The same, in Thumb mode.
#[track_caller]
fn tenc(src: &str, want: &str) {
    let got = hex(&text_for("thumb", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// The condition is the top nibble of every A32 word, and the `s` suffix goes
/// between the mnemonic and it: `add` + `s` + `eq` is `addseq`.
#[test]
fn every_condition_code_and_the_s_flag() {
    enc("add r0, r1, r2", "02 00 81 e0");
    enc("addeq r0, r1, r2", "02 00 81 00");
    enc("addne r0, r1, r2", "02 00 81 10");
    enc("addcs r0, r1, r2", "02 00 81 20");
    enc("addhs r0, r1, r2", "02 00 81 20");
    enc("addcc r0, r1, r2", "02 00 81 30");
    enc("addlo r0, r1, r2", "02 00 81 30");
    enc("addmi r0, r1, r2", "02 00 81 40");
    enc("addpl r0, r1, r2", "02 00 81 50");
    enc("addvs r0, r1, r2", "02 00 81 60");
    enc("addvc r0, r1, r2", "02 00 81 70");
    enc("addhi r0, r1, r2", "02 00 81 80");
    enc("addls r0, r1, r2", "02 00 81 90");
    enc("addge r0, r1, r2", "02 00 81 a0");
    enc("addlt r0, r1, r2", "02 00 81 b0");
    enc("addgt r0, r1, r2", "02 00 81 c0");
    enc("addle r0, r1, r2", "02 00 81 d0");
    enc("addal r0, r1, r2", "02 00 81 e0");
    enc("adds r0, r1, r2", "02 00 91 e0");
    enc("addseq r0, r1, r2", "02 00 91 00");
    enc("subsle r3, r4, r5", "05 30 54 d0");
    enc("movsmi r0, r1", "01 00 b0 41");
    enc("bicscc r0, r1, r2", "02 00 d1 31");
    enc("ldrls r0, [r1]", "00 00 91 95");
    enc("muleq r0, r1, r2", "91 02 00 00");
}

/// A data-processing immediate is an 8-bit value rotated right by an even
/// amount, and the rotation may wrap around the top of the word.
#[test]
fn modified_immediates() {
    enc("mov r0, 0", "00 00 a0 e3");
    enc("mov r0, 1", "01 00 a0 e3");
    enc("mov r0, 255", "ff 00 a0 e3");
    enc("mov r0, 256", "01 0c a0 e3");
    enc("mov r0, 1020", "ff 0f a0 e3");
    enc("mov r0, 4096", "01 0a a0 e3");
    enc("mov r0, 65536", "01 08 a0 e3");
    enc("mov r0, 0x00ff0000", "ff 08 a0 e3");
    enc("mov r0, 0xff000000", "ff 04 a0 e3");
    enc("mov r0, 0x81000000", "81 04 a0 e3");
    enc("mov r0, 0xf000000f", "ff 02 a0 e3");
    enc("add r0, r1, 4", "04 00 81 e2");
    enc("add r0, r1, 260", "41 0f 81 e2");
    enc("sub r0, r1, 4096", "01 0a 41 e2");
    enc("and r0, r1, 0xff", "ff 00 01 e2");
    enc("orr r0, r1, 0x0f000000", "0f 04 81 e3");
    enc("cmp r0, 255", "ff 00 50 e3");
    enc("tst r0, 1", "01 00 10 e3");
}

/// `add rd, rn, #-1` has no encoding; every ARM assembler rewrites it as
/// `sub rd, rn, #1`, and likewise for the other four pairs.
#[test]
fn immediates_that_only_the_complementary_operation_can_hold() {
    enc("mov r0, -1", "00 00 e0 e3");
    enc("mvn r0, 0", "00 00 e0 e3");
    enc("mov r0, 0xffffffff", "00 00 e0 e3");
    enc("add r0, r1, -1", "01 00 41 e2");
    enc("sub r0, r1, -1", "01 00 81 e2");
    enc("cmp r0, -1", "01 00 70 e3");
    enc("cmn r0, -1", "01 00 50 e3");
    enc("and r0, r1, -2", "01 00 c1 e3");
    enc("bic r0, r1, -2", "01 00 01 e2");
    enc("adc r0, r1, -1", "00 00 c1 e2");
    enc("sbc r0, r1, -1", "00 00 a1 e2");
}

/// The second operand of a data-processing instruction can carry a shift, by
/// a constant or by a register. The shift amount is written without the usual
/// `#`, which this assembler's GAS-dialect lexer would eat as a comment; the
/// encodings were confirmed against llvm-mc using the `#` spelling.
#[test]
fn barrel_shifter_operands() {
    enc("add r0, r1, r2, lsl 3", "82 01 81 e0");
    enc("add r0, r1, r2, lsr 4", "22 02 81 e0");
    enc("add r0, r1, r2, asr 5", "c2 02 81 e0");
    enc("add r0, r1, r2, ror 6", "62 03 81 e0");
    enc("add r0, r1, r2, rrx", "62 00 81 e0");
    enc("mov r0, r1, lsl 2", "01 01 a0 e1");
    enc("eors r0, r1, r2, lsl 31", "82 0f 31 e0");
    enc("cmp r0, r1, asr 32", "41 00 50 e1");
    enc("sub r0, r1, r2, lsl 1", "82 00 41 e0");
    enc("bic r0, r1, r2, ror 8", "62 04 c1 e1");
    enc("mvn r0, r1, asr 1", "c1 00 e0 e1");
    enc("tst r0, r1, lsl 4", "01 02 10 e1");
    enc("add r0, r1, r2, lsl r3", "12 03 81 e0");
    enc("add r0, r1, r2, asr r3", "52 03 81 e0");
    enc("mov r0, r1, lsl r2", "11 02 a0 e1");
    enc("mov r0, r1, rrx", "61 00 a0 e1");
}

#[test]
fn shift_mnemonics_are_forms_of_mov() {
    enc("lsl r0, r1, 3", "81 01 a0 e1");
    enc("lsl r0, r1, 31", "81 0f a0 e1");
    enc("lsr r0, r1, 1", "a1 00 a0 e1");
    enc("lsr r0, r1, 32", "21 00 a0 e1");
    enc("asr r0, r1, 5", "c1 02 a0 e1");
    enc("asr r0, r1, 32", "41 00 a0 e1");
    enc("ror r0, r1, 1", "e1 00 a0 e1");
    enc("rrx r0, r1", "61 00 a0 e1");
    enc("lsl r0, r1, r2", "11 02 a0 e1");
    enc("lsls r0, r1, 3", "81 01 b0 e1");
}

#[test]
fn loads_and_stores() {
    enc("ldr r0, [r1]", "00 00 91 e5");
    enc("ldr r0, [r1, 4]", "04 00 91 e5");
    enc("ldr r0, [r1, 4095]", "ff 0f 91 e5");
    enc("ldr r0, [r1, -4]", "04 00 11 e5");
    enc("ldr r0, [r1, 4]!", "04 00 b1 e5");
    enc("ldr r0, [r1], 4", "04 00 91 e4");
    enc("ldr r0, [r1], -4", "04 00 11 e4");
    enc("str r0, [r1, 8]", "08 00 81 e5");
    enc("ldrb r0, [r1, 255]", "ff 00 d1 e5");
    enc("strb r0, [r1, 4095]", "ff 0f c1 e5");
    enc("ldr r0, [r1, r2]", "02 00 91 e7");
    enc("ldr r0, [r1, -r2]", "02 00 11 e7");
    enc("ldr r0, [r1, r2, lsl 2]", "02 01 91 e7");
    enc("str r0, [r1, -r2, lsl 1]", "82 00 01 e7");
    enc("ldr r0, [r1, r2, asr 3]!", "c2 01 b1 e7");
    enc("ldr r0, [sp, 16]", "10 00 9d e5");
    enc("str lr, [sp, 4]", "04 e0 8d e5");
}

/// These predate the main load encoding and were squeezed into a gap in the
/// data-processing space, so their offset is split into two nibbles around a
/// `1SH1` marker.
#[test]
fn halfword_and_signed_loads() {
    enc("ldrh r0, [r1]", "b0 00 d1 e1");
    enc("ldrh r0, [r1, 255]", "bf 0f d1 e1");
    enc("ldrh r0, [r1, -8]", "b8 00 51 e1");
    enc("strh r0, [r1, 2]", "b2 00 c1 e1");
    enc("ldrsb r0, [r1, 1]", "d1 00 d1 e1");
    enc("ldrsh r0, [r1, 8]", "f8 00 d1 e1");
    enc("ldrh r0, [r1, r2]", "b2 00 91 e1");
    enc("strh r0, [r1, r2]", "b2 00 81 e1");
    enc("ldrsb r0, [r1, -r2]", "d2 00 11 e1");
    enc("ldrsh r0, [r1, r2]", "f2 00 91 e1");
    enc("ldrh r0, [r1, 4]!", "b4 00 f1 e1");
    enc("ldrh r0, [r1], 4", "b4 00 d1 e0");
}

/// `push`/`pop` are `stmdb sp!` and `ldmia sp!`; a one-register list is
/// rewritten as a plain indexed store or load, which is what GNU as and LLVM
/// both emit.
#[test]
fn register_lists() {
    enc("push {r0}", "04 00 2d e5");
    enc("push {r0, r1, lr}", "03 40 2d e9");
    enc("push {r4-r11, lr}", "f0 4f 2d e9");
    enc("pop {r0-r3, pc}", "0f 80 bd e8");
    enc("pop {pc}", "04 f0 9d e4");
    enc("ldm r0, {r1, r2}", "06 00 90 e8");
    enc("ldmia r0!, {r1, r2}", "06 00 b0 e8");
    enc("ldmib r0, {r1-r3}", "0e 00 90 e9");
    enc("ldmda r0, {r1-r3}", "0e 00 10 e8");
    enc("ldmdb r0, {r1-r3}", "0e 00 10 e9");
    enc("stmia r0!, {r1, r2}", "06 00 a0 e8");
    enc("stmfd sp!, {r4, r5}", "30 00 2d e9");
    enc("ldmfd sp!, {r4, r5}", "30 00 bd e8");
}

#[test]
fn multiplies() {
    enc("mul r0, r1, r2", "91 02 00 e0");
    enc("muls r0, r1, r2", "91 02 10 e0");
    enc("mla r0, r1, r2, r3", "91 32 20 e0");
    enc("mlas r0, r1, r2, r3", "91 32 30 e0");
    enc("mls r0, r1, r2, r3", "91 32 60 e0");
    enc("umull r0, r1, r2, r3", "92 03 81 e0");
    enc("umulls r0, r1, r2, r3", "92 03 91 e0");
    enc("smull r0, r1, r2, r3", "92 03 c1 e0");
    enc("umlal r0, r1, r2, r3", "92 03 a1 e0");
    enc("smlal r0, r1, r2, r3", "92 03 e1 e0");
}

#[test]
fn move_wide_and_the_small_unary_operations() {
    enc("movw r0, 0", "00 00 00 e3");
    enc("movw r0, 65535", "ff 0f 0f e3");
    enc("movw r5, 4660", "34 52 01 e3");
    enc("movt r0, 4660", "34 02 41 e3");
    enc("movt r9, 65535", "ff 9f 4f e3");
    enc("clz r0, r1", "11 0f 6f e1");
    enc("rev r0, r1", "31 0f bf e6");
    enc("rev16 r0, r1", "b1 0f bf e6");
    enc("revsh r0, r1", "b1 0f ff e6");
    enc("uxtb r0, r1", "71 00 ef e6");
    enc("sxtb r0, r1", "71 00 af e6");
    enc("uxth r0, r1", "71 00 ff e6");
    enc("sxth r0, r1", "71 00 bf e6");
}

#[test]
fn system_instructions_and_hints() {
    enc("nop", "00 f0 20 e3");
    enc("nopeq", "00 f0 20 03");
    enc("svc 0", "00 00 00 ef");
    enc("svc 16777215", "ff ff ff ef");
    enc("bkpt 0", "70 00 20 e1");
    enc("bkpt 65535", "7f ff 2f e1");
    enc("mrs r0, cpsr", "00 00 0f e1");
    enc("mrs r4, spsr", "00 40 4f e1");
    enc("msr cpsr_f, r0", "00 f0 28 e1");
    enc("msr cpsr_fsxc, r0", "00 f0 2f e1");
    enc("msr spsr_fsxc, r2", "02 f0 6f e1");
    enc("dmb", "5f f0 7f f5");
    enc("dmb ish", "5b f0 7f f5");
    enc("dmb ishst", "5a f0 7f f5");
    enc("dsb", "4f f0 7f f5");
    enc("isb", "6f f0 7f f5");
    enc("bx lr", "1e ff 2f e1");
    enc("bxeq r0", "10 ff 2f 01");
    enc("blx r3", "33 ff 2f e1");
}

// ---- branches, which need a label to aim at --------------------------------

#[test]
fn a_backward_branch_counts_from_two_instructions_ahead() {
    enc(
        "start:\n        mov     r0, 0\n        add     r0, r0, 1\n        b       start\n",
        "00 00 a0 e3 01 00 80 e2 fc ff ff ea",
    );
}

#[test]
fn a_forward_branch() {
    enc(
        "        b       done\n        mov     r0, 1\n        mov     r1, 2\ndone:\n        nop\n",
        "01 00 00 ea 01 00 a0 e3 02 10 a0 e3 00 f0 20 e3",
    );
}

#[test]
fn conditional_branches_around_a_loop() {
    enc(
        "loop:\n        subs    r0, r0, 1\n        bne     loop\n        bgt     out\n        cmp     r0, 0\n        beq     loop\nout:\n        nop\n",
        "01 00 50 e2 fd ff ff 1a 01 00 00 ca 00 00 50 e3 fa ff ff 0a 00 f0 20 e3",
    );
}

#[test]
fn a_branch_relative_to_the_location_counter() {
    enc(
        "        b       . + 8\n        nop\n        nop\n        b       . - 4\n",
        "00 00 00 ea 00 f0 20 e3 00 f0 20 e3 fd ff ff ea",
    );
}

// ---- Thumb -----------------------------------------------------------------

#[test]
fn thumb_moves_and_arithmetic() {
    tenc("movs r0, 5", "05 20");
    tenc("movs r7, 255", "ff 27");
    tenc("mov r0, r1", "08 46");
    tenc("mov r8, r9", "c8 46");
    tenc("movs r0, r1", "08 00");
    tenc("mvns r0, r1", "c8 43");
    tenc("adds r0, r1, r2", "88 18");
    tenc("subs r0, r1, r2", "88 1a");
    tenc("adds r0, r1, 3", "c8 1c");
    tenc("subs r0, r1, 3", "c8 1e");
    tenc("adds r0, 200", "c8 30");
    tenc("subs r0, 200", "c8 38");
    tenc("add r0, r1", "08 44");
    tenc("add r8, r9", "c8 44");
    tenc("add sp, 16", "04 b0");
    tenc("sub sp, 16", "84 b0");
    tenc("add r0, sp, 8", "02 a8");
    tenc("add r7, sp, 1020", "ff af");
    tenc("cmp r0, 200", "c8 28");
    tenc("cmp r0, r1", "88 42");
    tenc("cmp r8, r9", "c8 45");
    tenc("cmn r0, r1", "c8 42");
    tenc("tst r0, r1", "08 42");
    tenc("ands r0, r1", "08 40");
    tenc("eors r0, r1", "48 40");
    tenc("adcs r0, r1", "48 41");
    tenc("sbcs r0, r1", "88 41");
    tenc("orrs r0, r1", "08 43");
    tenc("bics r0, r1", "88 43");
    tenc("muls r0, r1, r0", "48 43");
    tenc("rsbs r0, r1, 0", "48 42");
}

#[test]
fn thumb_shifts() {
    tenc("lsls r0, r1, 3", "c8 00");
    tenc("lsls r0, r1, 31", "c8 07");
    tenc("lsrs r0, r1, 1", "48 08");
    tenc("lsrs r0, r1, 32", "08 08");
    tenc("asrs r0, r1, 5", "48 11");
    tenc("asrs r0, r1, 32", "08 10");
    tenc("lsls r0, r1", "88 40");
    tenc("lsrs r0, r1", "c8 40");
    tenc("asrs r0, r1", "08 41");
    tenc("rors r0, r1", "c8 41");
}

#[test]
fn thumb_loads_stores_and_stack() {
    tenc("ldr r0, [r1, 4]", "48 68");
    tenc("ldr r0, [r1, 124]", "c8 6f");
    tenc("str r0, [r1, 124]", "c8 67");
    tenc("ldrb r0, [r1, 31]", "c8 7f");
    tenc("ldrh r0, [r1, 62]", "c8 8f");
    tenc("ldr r0, [sp, 8]", "02 98");
    tenc("str r0, [sp, 1020]", "ff 90");
    tenc("ldr r0, [r1, r2]", "88 58");
    tenc("str r0, [r1, r2]", "88 50");
    tenc("ldrsb r0, [r1, r2]", "88 56");
    tenc("ldrsh r0, [r1, r2]", "88 5e");
    tenc("push {r0}", "01 b4");
    tenc("push {r4, lr}", "10 b5");
    tenc("pop {r0, pc}", "01 bd");
    tenc("pop {r0-r7, pc}", "ff bd");
    tenc("stmia r0!, {r1, r2}", "06 c0");
    tenc("ldmia r0!, {r1, r2}", "06 c8");
    tenc("bx lr", "70 47");
    tenc("blx r3", "98 47");
    tenc("nop", "00 bf");
    tenc("svc 3", "03 df");
    tenc("bkpt 255", "ff be");
    tenc("rev r0, r1", "08 ba");
    tenc("revsh r0, r1", "c8 ba");
    tenc("uxtb r0, r1", "c8 b2");
    tenc("sxth r0, r1", "08 b2");
}

/// Thumb-2 picks up where the 16-bit forms stop: a constant no 16-bit
/// encoding holds becomes `mov.w`, then `movw`, and `add` falls back through
/// the expandable-immediate form to `addw`.
#[test]
fn thumb_32_bit_encodings() {
    tenc("mov r0, 1", "4f f0 01 00");
    tenc("mov r0, 255", "4f f0 ff 00");
    tenc("mov r0, 291", "40 f2 23 10");
    tenc("mov r0, 65535", "4f f6 ff 70");
    tenc("mov r0, 16711935", "4f f0 ff 10");
    tenc("mov r0, 0xabababab", "4f f0 ab 30");
    tenc("movw r0, 291", "40 f2 23 10");
    tenc("movt r0, 4660", "c1 f2 34 20");
    tenc("add r0, r1, 1", "01 f1 01 00");
    tenc("add r0, 1", "00 f1 01 00");
    tenc("adds r0, r1, 8", "11 f1 08 00");
    tenc("adds r0, 300", "10 f5 96 70");
    tenc("sub r0, r1, 1", "a1 f1 01 00");
    tenc("subs r0, 300", "b0 f5 96 70");
    tenc("add r0, r1, 291", "01 f2 23 10");
    tenc("add r0, r1, 4095", "01 f6 ff 70");
    tenc("sub r0, r1, 4095", "a1 f6 ff 70");
    tenc("add r1, sp, 1024", "0d f5 80 61");
    tenc("cmp r0, 300", "b0 f5 96 7f");
    tenc("movs r0, 300", "5f f4 96 70");
    tenc("add r0, r1, r2", "01 eb 02 00");
    tenc("sub r0, r1, r2", "a1 eb 02 00");
    tenc("mul r0, r1, r2", "01 fb 02 f0");
    tenc("mla r0, r1, r2, r3", "01 fb 02 30");
    tenc("mls r0, r1, r2, r3", "01 fb 12 30");
    tenc("umull r0, r1, r2, r3", "a2 fb 03 01");
    tenc("smull r0, r1, r2, r3", "82 fb 03 01");
    tenc("clz r0, r1", "b1 fa 81 f0");
    tenc("ldr r0, [r1, 291]", "d1 f8 23 01");
    tenc("str r0, [r1, 291]", "c1 f8 23 01");
    tenc("ldrb r0, [r1, 291]", "91 f8 23 01");
    tenc("ldr.w r0, [r1, 4]", "d1 f8 04 00");
    tenc("nop.w", "af f3 00 80");
}

/// A constant that only encodes negated or complemented switches operation,
/// as in A32: `add` and `sub` trade places, `cmp` becomes `cmn`, and `mov`
/// falls back to `mvn` last. llvm-mc's Thumb parser needs `#` before a
/// negative immediate, so these were confirmed with that spelling.
#[test]
fn thumb_negative_immediates() {
    tenc("mov r0, -1", "4f f0 ff 30");
    tenc("mov r0, -2", "6f f0 01 00");
    tenc("mov r0, -256", "6f f0 ff 00");
    tenc("cmp r0, -1", "b0 f1 ff 3f");
    tenc("cmp r1, -2", "11 f1 02 0f");
    tenc("cmp r0, -300", "10 f5 96 7f");
    // A value the twelve-bit encoding holds as written stays as written;
    // only one it cannot becomes the other operation with the sign off.
    tenc("add r0, r1, -1", "01 f1 ff 30");
    tenc("adds r0, r1, -1", "11 f1 ff 30");
    tenc("subs r0, r1, -300", "11 f5 96 70");
    tenc("add r0, -1", "00 f1 ff 30");
    tenc("adds r0, -200", "c8 38");
    tenc("add r0, r1, -4095", "a1 f6 ff 70");
}

#[test]
fn a_short_thumb_branch_uses_the_16_bit_form() {
    tenc(
        "start:\n        movs    r0, 0\n        adds    r0, 1\n        b       start\n",
        "00 20 01 30 fc e7",
    );
}

#[test]
fn bl_is_always_the_32_bit_pair() {
    tenc(
        "target:\n        nop\n        bl      target\n",
        "00 bf ff f7 fd ff",
    );
}

#[test]
fn an_explicit_w_suffix_forces_the_wide_form() {
    tenc(
        "here:\n        b.w     here\n        bne.w   here\n        nop\n",
        "ff f7 fe bf 7f f4 fc af 00 bf",
    );
}

#[test]
fn thumb_conditional_branches_around_a_loop() {
    tenc(
        "loop:\n        subs    r0, 1\n        bne     loop\n        beq     out\n        cmp     r0, 0\n        bgt     loop\nout:\n        nop\n",
        "01 38 fd d1 01 d0 00 28 fa dc 00 bf",
    );
}

// ---- instruction-set switching ----------------------------------------------

/// `.arm`/`.thumb` and the `.code 32`/`.code 16` spelling both select the
/// instruction set for the rest of the file, like x86's `.code64`.
#[test]
fn the_instruction_set_can_be_switched_mid_file() {
    let want = "02 00 81 e0 88 18 07 23 02 00 81 e0";
    let got = hex(&text_for(
        "arm",
        "        add     r0, r1, r2\n        .thumb\n        adds    r0, r1, r2\n\
         \n        movs    r3, 7\n        .arm\n        add     r0, r1, r2\n",
    ));
    assert_eq!(got, want);
    let got = hex(&text_for(
        "arm",
        "        add     r0, r1, r2\n        .code   16\n        adds    r0, r1, r2\n\
         \n        movs    r3, 7\n        .code   32\n        add     r0, r1, r2\n",
    ));
    assert_eq!(got, want);
    // Starting in Thumb is what the `thumb` architecture name means; `.arm`
    // switches away from it just the same. (The switch is kept on a word
    // boundary: GNU as and LLVM also align to four bytes at `.arm`, which a
    // backend directive has no way to do.)
    let got = hex(&text_for(
        "thumb",
        "        adds    r0, r1, r2\n        movs    r3, 7\n        .arm\n\
         \n        add     r0, r1, r2\n",
    ));
    assert_eq!(got, "88 18 07 23 02 00 81 e0");
}

/// Padding in an executable section has to stay executable, so it is filled
/// with whichever no-op the current mode uses.
#[test]
fn alignment_padding_uses_real_no_ops() {
    assert_eq!(
        hex(&text_for("arm", "mov r0, r0\n.balign 16\nmov r0, r0\n")),
        "00 00 a0 e1 00 f0 20 e3 00 f0 20 e3 00 f0 20 e3 00 00 a0 e1"
    );
    // Ending on a word, since llvm-mc does not pad the end of the section
    // and GNU as does; see the next test.
    assert_eq!(
        hex(&text_for(
            "thumb",
            "movs r0, r0\n.balign 8\nmovs r0, r0\nmovs r0, r0\n"
        )),
        "00 00 00 bf 00 bf 00 bf 00 00 00 00"
    );
}

/// GNU as pads the end of a code section to the section's alignment, but only
/// up to a word, with the no-ops of the mode the file ends in; and it takes
/// an odd remainder as zeros, ahead of the no-ops (checked with
/// `arm-none-eabi-as -march=armv7-a`, whose Thumb padding otherwise uses
/// 32-bit no-ops where this uses two 16-bit ones).
#[test]
fn code_sections_are_padded_to_a_word_at_the_end() {
    assert_eq!(
        hex(&text_for("thumb", "movs r0, r0\n.balign 8\nmovs r0, r0\n")),
        "00 00 00 bf 00 bf 00 bf 00 00 00 bf"
    );
    assert_eq!(
        hex(&text_for(
            "arm",
            "mov r0, r0\n.byte 1\n.p2align 3\nmov r1, r1\n"
        )),
        "00 00 a0 e1 01 00 00 00 01 10 a0 e1"
    );
}

/// A Thumb branch is offered to layout as a 16-bit form and a 32-bit one; the
/// wide encoding is chosen only when the short one cannot reach.
#[test]
fn a_thumb_branch_grows_when_the_target_is_out_of_reach() {
    let near = text_for(
        "thumb",
        "far:\n        nop\n        b       far\n        bne     far\n",
    );
    assert_eq!(hex(&near), "00 bf fd e7 fc d1");

    let far = text_for(
        "thumb",
        "far:\n        nop\n        .space  4096\n        b       far\n        bne     far\n",
    );
    assert_eq!(far.len(), 2 + 4096 + 4 + 4);
    assert_eq!(hex(&far[far.len() - 8..]), "fe f7 fd bf 7e f4 fb af");
}

// ---- diagnostics -------------------------------------------------------------

/// Anything that cannot be encoded has to say why, naming the limit that was
/// exceeded.
#[test]
fn out_of_range_operands_are_diagnosed() {
    let e = errors_for("arm", "movs r0, 0x101");
    assert!(e.contains("not an ARM modified immediate"), "{e}");
    assert!(e.contains("rotated right by an even amount"), "{e}");

    assert!(errors_for("arm", "ldr r0, [r1, 5000]").contains("-4095 to 4095"));
    assert!(errors_for("arm", "ldrh r0, [r1, 300]").contains("-255 to 255"));
    assert!(errors_for("arm", "movw r0, 65536").contains("0 to 65535"));
    assert!(errors_for("arm", "add r0, r1, r2, lsl 33").contains("0 to 31"));
    assert!(errors_for("arm", "mov r0, r1, lsr 33").contains("0 to 32"));
    assert!(errors_for("arm", "push {r3-r1}").contains("runs backwards"));
    assert!(errors_for("arm", "bogus r0").contains("unknown instruction"));
    assert!(errors_for("arm", "add r0").contains("takes 2 or 3 operand(s)"));
    assert!(errors_for("arm", "b 0x8000000").contains("out of range"));
    assert!(errors_for("arm", "addeqs r0, r1, r2").contains("unknown instruction"));
    assert!(errors_for("arm", "bkpteq 1").contains("cannot be conditional"));
    assert!(errors_for("arm", "cmps r0, r1").contains("unknown instruction"));
    assert!(errors_for("arm", "add r0!, r1, r2").contains("writeback"));
    assert!(errors_for("arm", "movws r0, 1").contains("unknown instruction"));
    assert!(errors_for("arm", "ldrs r0, [r1]").contains("unknown instruction"));
    // A vector instruction cannot be conditional, and one that carries a
    // data type has no width suffix.
    assert!(errors_for("arm", "vaddne.i8 d0, d1, d2").contains("cannot be conditional"));
    assert!(errors_for("arm", "add.w r0, r1, r2").contains("invalid in ARM mode"));
    assert!(errors_for("thumb", "vadd.i8.w d0, d1, d2").contains("unknown instruction"));
    assert!(errors_for("arm", "vadd.i8 q0, q1, d2").contains("expected a `q` register"));
    assert!(errors_for("arm", "vpadd.i8 q0, q1, q2").contains("expected a `d` register"));
    assert!(errors_for("arm", "vext.8 d0, d1, d2, 9").contains("out of range"));
    assert!(errors_for("arm", "vshll.s8 q0, d1, 0").contains("1 to 7"));
    assert!(errors_for("arm", "vmov.i16 d0, 0x12345").contains("outside the element size"));
    assert!(errors_for("arm", "vmov.i32 d0, 0x12345").contains("no vector immediate"));
    assert!(errors_for("arm", "vld1.8 {d0, d1, d2, d3, d4}, [r0]").contains("bad list type"));
    assert!(errors_for("arm", "vld3.8 {d0, d1}, [r0]").contains("bad list type"));
    assert!(errors_for("arm", "vld1.8 {d0}, [r0:128]").contains("alignment"));
    assert!(errors_for("arm", "vldmia r0, {d5-d5}").contains("one register to another"));
}

/// Thumb has narrower fields and no predication outside an `it` block, and
/// says so rather than silently encoding something else.
#[test]
fn thumb_restrictions_are_diagnosed() {
    assert!(errors_for("thumb", "addeq r0, r1, r2").contains("`it` block"));
    assert!(errors_for("thumb", "push {sp}").contains("base register"));
    assert!(errors_for("thumb", "ldm r0!, {r0, r1}").contains("base register"));
    assert!(errors_for("thumb", "movs r0, 0x101").contains("Thumb expandable immediate"));
    assert!(errors_for("thumb", "lsls r0, r1, 32").contains("0 to 31"));
    assert!(errors_for("thumb", "orn sp, r1, r2").contains("not allowed here"));
    assert!(errors_for("thumb", "cmp r0, 0x101").contains("neither is its complement"));
    assert!(errors_for("thumb", "strd r0, r1, [r2, 7]").contains("steps of 4"));
    assert!(errors_for("thumb", "cbz r8, .").contains("only tests r0-r7"));
    // An offset past 32 bits must not wrap around into a small one.
    assert!(errors_for("thumb", "ldr r0, [r1, 0x100000004]").contains("-255 to 255"));
    assert!(errors_for("thumb", "ldr r0, [r1, 124].n").contains("unexpected token"));
}

/// The load/store exclusives, the swap they replaced, and the saturating and
/// packing group.
#[test]
fn exclusives_saturating_and_packing() {
    enc("ldrex r0, [r1]", "9f 0f 91 e1");
    enc("strexd r0, r2, r3, [r4]", "92 0f a4 e1");
    enc("clrex", "1f f0 7f f5");
    enc("swp r0, r1, [r2]", "91 00 02 e1");
    enc("ssat r0, 1, r1, lsl 3", "91 01 a0 e6");
    enc("ssat r0, 32, r1, asr 5", "d1 02 bf e6");
    enc("usat r0, 0, r1", "11 00 e0 e6");
    enc("pkhbt r0, r1, r2, lsl 3", "92 01 81 e6");
    enc("pkhtb r0, r1, r2, asr 5", "d2 02 81 e6");
    // `pkhtb` with no shift is `pkhbt` with the sources the other way round,
    // which is how GNU as writes it.
    enc("pkhtb r0, r1, r2", "11 00 82 e6");
    enc("uadd8 r0, r1, r2", "92 0f 51 e6");
    enc("sel r0, r1, r2", "b2 0f 81 e6");
    enc("qdsub r0, r1, r2", "51 00 62 e1");
    tenc("ldrex r0, [r1, 4]", "51 e8 01 0f");
    tenc("strexd r0, r2, r3, [r1]", "c1 e8 70 23");
    tenc("ssat r0, 1, r1, asr 3", "21 f3 c0 00");
    tenc("pkhtb r0, r1, r2, asr 5", "c1 ea 62 10");
    tenc("uadd8 r0, r1, r2", "81 fa 42 f0");
}

/// The bitfield moves, the extends and the halfword multiplies.
#[test]
fn bitfields_extends_and_the_halfword_multiplies() {
    enc("sxtab r0, r1, r2, ror 8", "72 04 a1 e6");
    enc("uxth r0, r1, ror 16", "71 08 ff e6");
    enc("bfi r0, r1, 4, 8", "11 02 cb e7");
    enc("bfc r0, 0, 32", "1f 00 df e7");
    enc("sbfx r0, r1, 7, 9", "d1 03 a8 e7");
    enc("rbit r0, r1", "31 0f ff e6");
    enc("smlabb r0, r1, r2, r3", "81 32 00 e1");
    enc("smlald r0, r1, r2, r3", "12 03 41 e7");
    enc("smmulr r0, r1, r2", "31 f2 50 e7");
    enc("umaal r0, r1, r2, r3", "92 03 41 e0");
    enc("sdiv r0, r1, r2", "11 f2 10 e7");
    tenc("sxtb.w r0, r1, ror 8", "4f fa 91 f0");
    tenc("bfi r0, r1, 4, 8", "61 f3 0b 10");
    tenc("rev.w r0, r1", "91 fa 81 f0");
    tenc("clz r0, r1", "b1 fa 81 f0");
    tenc("mul r0, r1", "01 fb 00 f0");
    tenc("smlald r0, r1, r2, r3", "c2 fb c3 01");
    tenc("sdiv r0, r1, r2", "91 fb f2 f0");
}

/// The coprocessor instructions, whose operands are a coprocessor number,
/// its registers and an opcode; a Thumb one is the ARM word, halfword by
/// halfword, with `al` left in the condition field.
#[test]
fn coprocessor_instructions() {
    enc("mcr p15, 0, r0, c1, c0, 0", "10 0f 01 ee");
    enc("mrc p15, 0, r0, c1, c0, 0", "10 0f 11 ee");
    enc("mrc p15, 0, APSR_nzcv, c1, c0, 0", "10 ff 11 ee");
    enc("cdp p1, 2, c3, c4, c5, 6", "c5 31 24 ee");
    enc("mcrr p1, 2, r3, r4, c5", "25 31 44 ec");
    enc("ldc p1, c2, [r3, 8]!", "02 21 b3 ed");
    enc("stc2l p1, c2, [r3], {255}", "ff 21 c3 fc");
    tenc("mcr p15, 0, r0, c1, c0, 0", "01 ee 10 0f");
    tenc("ldc p1, c2, [r3], {8}", "93 ec 08 21");
}

/// The hint and barrier space, and the system instructions.
#[test]
fn hints_barriers_and_the_system_space() {
    enc("nop {5}", "05 f0 20 e3");
    enc("dbg 3", "f3 f0 20 e3");
    enc("dmb ish", "5b f0 7f f5");
    enc("isb", "6f f0 7f f5");
    enc("setend be", "00 02 01 f1");
    enc("cpsie f, 16", "50 00 0a f1");
    enc("srsdb sp!, 19", "13 05 6d f9");
    enc("rfeia r0!", "00 0a b0 f8");
    enc("ldmia r0!, {r1, pc}^", "02 80 f0 e8");
    enc("smc 15", "7f 00 60 e1");
    enc("hvc 0xffff", "7f ff 4f e1");
    enc("eret", "6e 00 60 e1");
    tenc("nop.w", "af f3 00 80");
    tenc("dmb ish", "bf f3 5b 8f");
    tenc("setend be", "58 b6");
    tenc("cpsid a, 31", "af f3 9f 87");
    tenc("srsdb sp!, 0", "2d e8 00 c0");
    tenc("rfeia r0!", "b0 e9 00 c0");
    tenc("udf.w 0x1234", "f1 f7 34 a2");
    tenc("smc 15", "ff f7 00 80");
    tenc("hvc 0x1234", "e1 f7 34 82");
}

/// `mrs` and `msr` reach the status registers by field, `apsr` by bit name,
/// and every mode's banked registers by name.
#[test]
fn the_status_registers() {
    enc("mrs r0, spsr_fiq", "00 02 4e e1");
    enc("mrs r0, elr_hyp", "00 03 0e e1");
    enc("mrs r0, lr_irq", "00 03 00 e1");
    enc("msr elr_hyp, r1", "01 f3 2e e1");
    enc("msr cpsr_f, 255", "ff f0 28 e3");
    enc("msr apsr_g, r0", "00 f0 24 e1");
    enc("msr apsr_nzcvqg, r0", "00 f0 2c e1");
    enc("msr cpsr_all, r0", "00 f0 29 e1");
    tenc("mrs r0, sp_usr", "e5 f3 20 80");
    tenc("msr apsr_g, r0", "80 f3 00 84");
    // `movs pc, lr` is the exception return, which is `subs pc, lr, #0`.
    tenc("movs pc, lr", "de f3 00 8f");
}

/// The preloads address memory and load nothing, so their transfer register
/// field is all ones.
#[test]
fn preloads_and_the_other_transfers() {
    enc("pld [r0, -4]", "04 f0 50 f5");
    enc("pli [r0, r1]", "01 f0 d0 f6");
    enc("ldrd r0, r1, [r2, 8]!", "d8 00 e2 e1");
    enc("strd r0, r1, [r2], r3", "f3 00 82 e0");
    enc("ldrt r0, [r1], 4", "04 00 b1 e4");
    enc("neg r0, r1", "00 00 61 e2");
    tenc("pld [r0, -4]", "10 f8 04 fc");
    tenc("pli [r0, r1]", "10 f9 01 f0");
    tenc("ldrd r0, r1, [r2, 1020]", "d2 e9 ff 01");
    tenc("strd r0, r1, [r2, -8]!", "62 e9 02 01");
    tenc("ldrt r0, [r1, 4]", "51 f8 04 0e");
}

/// Thumb-2's 32-bit data-processing forms, and the width the assembler picks
/// where both exist.
#[test]
fn thumb_32_bit_data_processing() {
    tenc("and.w r0, r1, r2", "01 ea 02 00");
    tenc("and r0, r1, 0xfffffffe", "21 f0 01 00");
    tenc("orn r0, r1, r2", "61 ea 02 00");
    tenc("teq r0, 255", "90 f0 ff 0f");
    tenc("cmp.w r0, r1", "b0 eb 01 0f");
    tenc("mvn.w r0, r1", "6f ea 01 00");
    tenc("rsb r0, r1, 255", "c1 f1 ff 00");
    tenc("addw r0, r1, 4095", "01 f6 ff 70");
    tenc("subw r0, r1, 100", "a1 f2 64 00");
    tenc("add r0, pc, 4", "01 a0");
    tenc("lsl.w r0, r1, 3", "4f ea c1 00");
    tenc("ror.w r0, r1, 15", "4f ea f1 30");
    tenc("rrx r0, r1", "4f ea 31 00");
    tenc("lsr r0, r1, r2", "21 fa 02 f0");
    // `mov` with a shifted source is the shift instruction itself.
    tenc("movs r7, r5, lsl 29", "6f 07");
}

/// Thumb-2's addressing modes, which the 16-bit encodings have none of.
#[test]
fn thumb_32_bit_loads_and_stores() {
    tenc("ldr r0, [r1, 4]!", "51 f8 04 0f");
    tenc("ldr r0, [r1], -4", "51 f8 04 09");
    tenc("ldr.w r0, [r1, r2, lsl 2]", "51 f8 22 00");
    tenc("ldrb r0, [r1, -8]", "11 f8 08 0c");
    tenc("ldrsb.w r0, [r1, r2]", "11 f9 02 00");
    // A PC-relative address is the literal form, whose offset is twelve
    // whole bits with the sign in the first halfword.
    tenc("ldrb r5, [pc, -64]", "1f f8 40 50");
    tenc("ldm.w r0!, {r1, r8}", "b0 e8 02 01");
    tenc("ldmdb r0!, {r1, r2}", "30 e9 06 00");
    tenc("push {r0, r8}", "2d e9 01 01");
    tenc("pop.w {r4, r5}", "bd e8 30 00");
    // A one-register block transfer is the load or store that means the same
    // thing, which is how it reaches a 16-bit encoding.
    tenc("ldm r6, {r7}", "37 68");
    tenc("tbb [r0, r1]", "d0 e8 01 f0");
    tenc("tbh [r0, r1, lsl 1]", "d0 e8 11 f0");
}

/// `cbz` and `cbnz` reach 4 to 130 bytes forward and have no relocation, so
/// a target further off is an error rather than a wider instruction.
#[test]
fn compare_and_branch_on_zero() {
    tenc(
        "cbz r0, 1f\n        nop\n        nop\n1:      nop\n",
        "08 b1 00 bf 00 bf 00 bf",
    );
    tenc(
        "cbnz r7, 1f\n        nop\n1:      nop\n",
        "07 b9 00 bf 00 bf",
    );
    // A branch to the next instruction is a no-op, as GNU as writes it.
    tenc("cbz r0, 1f\n1:      nop\n", "00 bf 00 bf");
    assert!(errors_for("thumb", "cbz r0, .").contains("4 to 130 bytes forward"));
}

/// Malformed input must produce a diagnostic, never a panic. Nothing here is
/// expected to assemble; the only requirement is that the assembler returns.
#[test]
fn malformed_input_never_panics() {
    const BAD: &[&str] = &[
        "add",
        "add,",
        "add r0,",
        "add r0, ,",
        "add r0, r1, r2,",
        "add r0, r1, lsl",
        "add r0, r1, lsl lsl",
        "add r0, r1, r2, lsl",
        "add r0, r1, r2, lsl #",
        "add r0, r1, r2, lsl r",
        "mov r0, r1, rrx r2",
        "mov r16, r1",
        "mov r0, r1, foo 3",
        "ldr",
        "ldr r0",
        "ldr r0, [",
        "ldr r0, []",
        "ldr r0, [r1",
        "ldr r0, [r1,",
        "ldr r0, [r1, ]",
        "ldr r0, [r1, r2",
        "ldr r0, [r1]!",
        "ldr r0, [r1],",
        "ldr r0, [r1], [r2]",
        "ldr r0, [r1, -]",
        "ldr r0, [r1, +]",
        "ldr r0, =",
        "ldr =1",
        "ldr r0, =1, 2",
        "ldr r0, [=1]",
        "str r0, =1",
        "adr r0",
        "adr r0, [r1]",
        "adrl r0, =1",
        "it",
        "it foo",
        "it eq, ne",
        "itt eq\nmoveq r0, r1\n.ltorg\nit ne",
        "ite al\naddal r0, r1\naddal r0, r1",
        "blx",
        "blx [r0]",
        "ldr r0, [sp, undefined_symbol]",
        "push",
        "push {",
        "push {}",
        "push {r0",
        "push {r0,",
        "push {r0-}",
        "push {-r0}",
        "push {r0, }",
        "push {r0 r1}",
        "push r0",
        "ldm",
        "ldm r0",
        "ldm r0!",
        "ldm {r0}",
        "b",
        "b b",
        "b r0",
        "bx",
        "bx 1",
        "blx",
        "bl",
        "mul",
        "mul r0",
        "mul r0, r1",
        "mla r0, r1, r2",
        "umull r0, r1, r2",
        "movw",
        "movw r0",
        "movw r0, r1",
        "movt r0, -1",
        "clz",
        "clz r0",
        "nop r0",
        "svc",
        "svc -1",
        "bkpt",
        "mrs",
        "mrs r0",
        "mrs r0, bogus",
        "msr bogus, r0",
        "msr cpsr_q, r0",
        "msr cpsr_z, r0",
        "dmb bogus",
        "dmb r0",
        "adds",
        "addseq",
        "adds.x r0, r1, r2",
        "add.w",
        "s",
        "eq",
        "r0",
        ".code",
        ".code 8",
        ".code r0",
        ".arm r0",
        "mov r0, 1 1",
        "mov r0, (",
        "mov r0, )",
        "mov r0, {r1}",
        "mov {r0}, r1",
        "lsl",
        "lsl r0",
        "rrx",
        "rrx r0",
        "rrx r0, r1, r2",
        "\u{e9}eq r0",
        "add\u{e9} r0, r1",
        "mov r0, 99999999999999999999",
        "add r0, r1, -9223372036854775808",
        "adds r0, -9223372036854775808",
        "ldr r0, [r1, -9223372036854775808]",
        "b 0xffffffffffffffff",
        "b.w 1",
        "bne.n 0x100000",
        "push {r0}!",
        "ldm r0!!, {r1}",
    ];
    for src in BAD {
        for arch in ["arm", "thumb"] {
            let _ = try_text_for(arch, src);
        }
    }
}

// ---- GNU-style ARM spelling ----------------------------------------------
//
// Expected bytes are from `llvm-mc -triple=armv7`. Before comments became a
// per-target setting, none of these could be written at all: `#` started a
// comment, so `mov r0, #1` reached the backend as `mov r0,`.

#[test]
fn hash_immediates_are_not_comments() {
    assert_eq!(hex(&text_for("arm", "mov r0, #1\n")), "01 00 a0 e3");
    assert_eq!(
        hex(&text_for("arm", "add r0, r1, r2, lsl #3\n")),
        "82 01 81 e0"
    );
    assert_eq!(hex(&text_for("arm", "ldr r0, [r1, #-4]\n")), "04 00 11 e5");
}

#[test]
fn at_sign_comments_and_first_column_hash_comments() {
    assert_eq!(
        hex(&text_for("arm", "mov r0, #1 @ load one\n")),
        "01 00 a0 e3"
    );
    assert_eq!(
        hex(&text_for("arm", "# 1 \"file.c\"\nmov r0, #2\n")),
        "02 00 a0 e3"
    );
    assert_eq!(
        hex(&text_for("arm", "   # indented\nmov r0, #3\n")),
        "03 00 a0 e3"
    );
}

#[test]
fn word_is_four_bytes_on_arm() {
    assert_eq!(hex(&text_for("arm", ".word 0x11223344\n")), "44 33 22 11");
}

/// The floating-point unit: the arithmetic, the conversions, the transfers
/// and the moves between core and vector registers.
#[test]
fn the_floating_point_unit() {
    enc("vadd.f32 s0, s1, s2", "81 0a 30 ee");
    enc("vmla.f64 d0, d1, d2", "02 0b 01 ee");
    enc("vsqrt.f32 s7, s8", "c4 3a f1 ee");
    enc("vcmpe.f64 d0, d1", "c1 0b b4 ee");
    enc("vcmp.f32 s0, 0", "40 0a b5 ee");
    enc("vmov s0, s1", "60 0a b0 ee");
    enc("vmov.f64 d0, d1", "41 0b b0 ee");
    enc("vcvt.f64.f32 d0, s1", "e0 0a b7 ee");
    enc("vcvt.s32.f32 s0, s1", "e0 0a bd ee");
    enc("vcvtr.u32.f64 s0, d1", "41 0b bc ee");
    // The fixed-point conversions name the number of fraction bits, which
    // the field holds counted down from the element size.
    enc("vcvt.f32.s16 s0, s0, 4", "46 0a ba ee");
    enc("vldr d0, [r1, 8]", "02 0b 91 ed");
    enc("vstr s0, [r1, 1020]", "ff 0a 81 ed");
    enc("vldmia r0!, {s0-s3}", "04 0a b0 ec");
    enc("vpush {d0-d3}", "08 0b 2d ed");
    enc("vpop {s0}", "01 0a bd ec");
    // The deprecated `fldmx` transfers, whose list is one word longer than
    // the registers in it.
    enc("fldmiax r0!, {d0-d3}", "09 0b b0 ec");
    enc("vmov r0, r1, d2", "12 0b 51 ec");
    enc("vmov s2, s3, r0, r1", "11 0a 41 ec");
    enc("vmov.s8 r1, d0[3]", "70 1b 50 ee");
    enc("vmov.32 d0[1], r1", "10 1b 20 ee");
    enc("vmrs apsr_nzcv, fpscr", "10 fa f1 ee");
    enc("vmsr fpscr, r0", "10 0a e1 ee");
    // A VFP instruction takes a condition, unlike a NEON one.
    enc("vaddeq.f32 s0, s1, s2", "81 0a 30 0e");
}

/// NEON data processing: the three-register arithmetic over `d` and `q`
/// registers, the shifts, the widening and narrowing forms, the scalar
/// multiplies and the permutes.
#[test]
fn neon_data_processing() {
    enc("vadd.i8 d0, d1, d2", "02 08 01 f2");
    enc("vadd.i16 q0, q1, q2", "44 08 12 f2");
    enc("vsub.f32 q0, q1, q2", "44 0d 22 f2");
    enc("vmul.p8 d0, d1, d2", "12 09 01 f3");
    enc("vand q0, q1, q2", "54 01 02 f2");
    enc("vbsl d0, d1, d2", "12 01 11 f3");
    enc("vceq.i8 d0, d1, 0", "01 01 b1 f3");
    // `vcle` and `vclt` are `vcge` and `vcgt` with the sources the other way
    // round, which is how GNU as writes them.
    enc("vcle.s8 d0, d1, d2", "11 03 02 f2");
    enc("vcgt.f32 d0, d1, 0", "01 04 b9 f3");
    enc("vqadd.s8 d0, d1, d2", "12 00 01 f2");
    enc("vhadd.s8 d0, d1, d2", "02 00 01 f2");
    enc("vmax.s8 d0, d1, d2", "02 06 01 f2");
    enc("vpadd.i8 d0, d1, d2", "12 0b 01 f2");
    enc("vabd.f32 q0, q1, q2", "44 0d 22 f3");
    enc("vshl.i32 q0, q1, 31", "52 05 bf f2");
    // A right shift counts down from the element size, so the widest one
    // leaves the field empty.
    enc("vshr.s8 d0, d1, 8", "11 00 88 f2");
    enc("vsra.s32 d0, d1, 32", "11 01 a0 f2");
    enc("vsri.16 q0, q1, 15", "52 04 91 f3");
    enc("vqshlu.s8 d0, d1, 7", "11 06 8f f3");
    enc("vshrn.i16 d0, q1, 8", "12 08 88 f2");
    enc("vqshrun.s16 d0, q1, 8", "12 08 88 f3");
    // `vshll` by the element size is an encoding of its own, and takes the
    // type suffix under any of its three spellings.
    enc("vshll.s8 q0, d1, 8", "01 03 b2 f3");
    enc("vshll.i16 q0, d1, 16", "01 03 b6 f3");
    enc("vmovl.s8 q0, d1", "11 0a 88 f2");
    enc("vqmovun.s16 d0, q1", "42 02 b2 f3");
    enc("vaddl.s8 q0, d1, d2", "02 00 81 f2");
    enc("vmull.p8 q0, d1, d2", "02 0e 81 f2");
    enc("vqdmlal.s16 q0, d1, d2", "02 09 91 f2");
    enc("vmul.i16 d0, d1, d2[1]", "4a 08 91 f2");
    enc("vmlal.s16 q0, d1, d2[3]", "6a 02 91 f2");
    enc("vqdmulh.s16 d0, d1, d2[2]", "62 0c 91 f2");
    // `vext` takes three bits of immediate over `d` registers and four over
    // `q` ones.
    enc("vext.8 d0, d1, d2, 3", "02 03 b1 f2");
    enc("vext.8 q0, q1, q2, 9", "44 09 b2 f2");
    enc("vrev64.32 d0, d1", "01 00 b8 f3");
    enc("vcnt.8 d0, d1", "01 05 b0 f3");
    enc("vmvn q0, q1", "c2 05 b0 f3");
    enc("vpaddl.s8 d0, d1", "01 02 b0 f3");
    enc("vrecpe.f32 q0, q1", "42 05 bb f3");
    enc("vcvt.s32.f32 q0, q1, 3", "52 0f bd f2");
    enc("vdup.8 d0, d1[3]", "01 0c b7 f3");
    enc("vdup.16 q0, r1", "30 1b a0 ee");
    enc("vtbl.8 d0, {d1-d3}, d4", "04 0a b1 f3");
    enc("vtbx.8 d0, {d1-d2}, d3", "43 09 b1 f3");
    // `vzip.32` and `vuzp.32` on `d` registers are both `vtrn.32`.
    enc("vzip.32 d0, d1", "81 00 ba f3");
    enc("vuzp.8 q0, q1", "42 01 b2 f3");
    // The middle operand may be left out, and then it is the destination.
    enc("vadd.i8 d0, d1", "01 08 00 f2");
    enc("vmla.f32 q0, q1", "52 0d 00 f2");
}

/// The NEON modified immediate, whose `cmode` comes from the value written,
/// and the structure loads and stores, whose list shape picks the encoding.
#[test]
fn the_neon_immediate_and_structure_transfers() {
    enc("vmov.i8 d0, 0x12", "12 0e 81 f2");
    enc("vmov.i32 d0, 0xff0000", "1f 04 87 f3");
    enc("vmov.i64 q0, 0xff00ff00ff00ff00", "7a 0e 82 f3");
    enc("vmvn.i32 d0, 0xff", "3f 00 87 f3");
    enc("vorr.i16 q0, 0x100", "51 0b 80 f2");
    enc("vbic.i32 d0, 0xff0000", "3f 05 87 f3");
    // `vmov dd, dm` is `vorr dd, dm, dm`, which is what GNU as writes.
    enc("vmov d0, d1", "11 01 21 f2");
    enc("vld1.8 {d0}, [r0]", "0f 07 20 f4");
    enc("vld1.16 {d0, d1}, [r0:64]!", "5d 0a 20 f4");
    enc("vld2.32 {d0, d2}, [r0], r1", "81 09 20 f4");
    enc("vld3.8 {d0[1], d1[1], d2[1]}, [r0]", "2f 02 a0 f4");
    enc("vld4.16 {d0[], d1[], d2[], d3[]}, [r0:64]", "5f 0f a0 f4");
    enc("vst1.8 {d0-d3}, [r0:256]", "3f 02 00 f4");
    enc("vst2.16 {d0[2], d1[2]}, [r0:32]!", "9d 05 80 f4");
    enc("vst4.32 {d0, d1, d2, d3}, [r0:128]", "af 00 00 f4");
}

/// A T32 vector instruction is the A32 one with its top byte translated, and
/// the two exception-return forms Thumb has of its own.
#[test]
fn thumb_vector_instructions() {
    tenc("vadd.f32 s0, s1, s2", "30 ee 81 0a");
    tenc("vldr d0, [r1, 8]", "91 ed 02 0b");
    tenc("vpush {d0-d3}", "2d ed 08 0b");
    tenc("vmov.32 d0[1], r1", "20 ee 10 1b");
    tenc("vmrs r0, fpscr", "f1 ee 10 0a");
    tenc("vadd.i8 d0, d1, d2", "01 ef 02 08");
    tenc("vmul.f32 q0, q1, q2", "02 ff 54 0d");
    tenc("vshr.s8 d0, d1, 8", "88 ef 11 00");
    tenc("vmov.i32 d0, 0xff0000", "87 ff 1f 04");
    tenc("vext.8 q0, q1, q2, 9", "b2 ef 44 09");
    tenc("vcvt.f32.s32 q0, q1", "bb ff 42 06");
    tenc("vtbl.8 d0, {d1-d3}, d4", "b1 ff 04 0a");
    tenc("vld1.8 {d0}, [r0]", "20 f9 0f 07");
    tenc("vst2.16 {d0[2], d1[2]}, [r0:32]!", "80 f9 9d 05");
    tenc("vld4.16 {d0[], d1[], d2[], d3[]}, [r0:64]", "a0 f9 5f 0f");
    // `subs pc, lr, #imm` is the exception return; `movs pc, lr` is the
    // same encoding with no immediate.
    tenc("subs pc, lr, 4", "de f3 04 8f");
    tenc("movs pc, lr", "de f3 00 8f");
    // `mvns rd, #x` where only the complement is expandable is `movs`.
    tenc("mvns r8, -2", "5f f0 01 08");
}

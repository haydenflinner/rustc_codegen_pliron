//! AArch64 (A64) encoding tests.
//!
//! Every expected byte string here was produced by `tools/mc-diff/run.sh
//! aarch64` agreeing with llvm-mc 22, or by a one-off comparison against it
//! for cases too large for the corpus (the branch-range boundaries). The
//! system instructions and the literal pools are GNU as's, from
//! `tools/xas-diff/run.sh aarch64` and `tools/flat-diff/run.sh aarch64-gas`;
//! the relocation types were compared against llvm-mc's and GNU as's objects.
//!
//! Immediates in the general-purpose tests are written without `#`, which
//! both references accept; the SIMD and SVE tests write it.

#![cfg(feature = "aarch64")]

mod common;
use common::*;

const ARCH: &str = "aarch64";

/// Asserts that each source line assembles to its expected bytes, written as
/// space-separated hex.
#[track_caller]
fn check(cases: &[(&str, &str)]) {
    for (src, want) in cases {
        let got = hex(&text_for(ARCH, src));
        assert_eq!(&got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
    }
}

#[track_caller]
fn program(src: &str, want: &str) {
    let got = hex(&text_for(ARCH, src));
    assert_eq!(got, want, "\nsource:\n{src}");
}

/// Asserts that `src` fails to assemble with a diagnostic containing every
/// one of `needles`.
#[track_caller]
fn rejects(src: &str, needles: &[&str]) {
    let err = errors_for(ARCH, src);
    for n in needles {
        assert!(
            err.contains(n),
            "diagnostic for `{src}` should mention `{n}`:\n{err}"
        );
    }
}

/// Add and subtract in all three operand shapes: immediate, shifted
/// register and extended register.
#[test]
fn add_and_subtract() {
    check(&[
        ("add x0, x1, 1", "20 04 00 91"),
        ("add w0, w1, 4095", "20 fc 3f 11"),
        ("add x0, x1, 1, lsl 12", "20 04 40 91"),
        ("add x0, x1, -1", "20 04 00 d1"),
        ("sub x0, x1, -1", "20 04 00 91"),
        ("add sp, sp, 16", "ff 43 00 91"),
        ("add x0, sp, 8", "e0 23 00 91"),
        ("adds w9, w10, 42", "49 a9 00 31"),
        ("subs w0, w1, 4095, lsl 12", "20 fc 7f 71"),
        ("cmp x0, 4095", "1f fc 3f f1"),
        ("cmn w0, 1", "1f 04 00 31"),
        ("cmp x0, -1", "1f 04 00 b1"),
        ("add x0, x1, x2", "20 00 02 8b"),
        ("add x0, x1, x2, lsl 3", "20 0c 02 8b"),
        ("add x0, x1, x2, lsr 63", "20 fc 42 8b"),
        ("adds x0, x1, x2, lsr 1", "20 04 42 ab"),
        ("cmp w0, w1, lsl 4", "1f 10 01 6b"),
        ("neg x0, x1", "e0 03 01 cb"),
        ("neg w0, w1, lsl 3", "e0 0f 01 4b"),
        ("negs x0, x1", "e0 03 01 eb"),
        ("add x0, x1, w2, uxtb", "20 00 22 8b"),
        ("add x0, x1, w2, uxtw 2", "20 48 22 8b"),
        ("add x0, x1, x2, sxtx", "20 e0 22 8b"),
        ("add x0, sp, x1", "e0 63 21 8b"),
        ("add sp, sp, x1", "ff 63 21 8b"),
        ("cmp x0, w1, uxtb", "1f 00 21 eb"),
    ]);
}

#[test]
fn add_and_subtract_with_carry() {
    check(&[
        ("adc x0, x1, x2", "20 00 02 9a"),
        ("sbcs w0, w1, w2", "20 00 02 7a"),
        ("ngc x0, x1", "e0 03 01 da"),
    ]);
}

/// The logical group and the aliases that ride on it. `mov` between two
/// registers is `orr` from the zero register, except when either side is the
/// stack pointer.
#[test]
fn logical_register() {
    check(&[
        ("and x0, x1, x2", "20 00 02 8a"),
        ("orr w3, w4, w5", "83 00 05 2a"),
        ("eor x6, x7, x8, lsl 4", "e6 10 08 ca"),
        ("ands x9, x10, x11, asr 63", "49 fd 8b ea"),
        ("bic x0, x1, x2", "20 00 22 8a"),
        ("orn w0, w1, w2", "20 00 22 2a"),
        ("eon x0, x1, x2", "20 00 22 ca"),
        ("and x0, x1, x2, ror 32", "20 80 c2 8a"),
        ("tst x0, x1", "1f 00 01 ea"),
        ("mvn x0, x1", "e0 03 21 aa"),
        ("mov x0, x1", "e0 03 01 aa"),
        ("mov w0, w1", "e0 03 01 2a"),
        ("mov sp, x0", "1f 00 00 91"),
        ("mov x0, sp", "e0 03 00 91"),
    ]);
}

/// The N:immr:imms bitmask immediate, across several element widths.
#[test]
fn logical_immediate() {
    check(&[
        ("and x0, x1, 0xff", "20 1c 40 92"),
        ("and w0, w1, 0xff", "20 1c 00 12"),
        ("and x0, x1, 1", "20 00 40 92"),
        ("orr x0, x1, 0xffff0000ffff0000", "20 3c 10 b2"),
        ("orr w0, w1, 0x55555555", "20 f0 00 32"),
        ("eor x0, x1, 0x5555555555555555", "20 f0 00 d2"),
        ("ands w0, w1, 0x3ffc", "20 2c 1e 72"),
        ("tst x0, 0xf", "1f 0c 40 f2"),
        ("and x0, x1, 0xf00000000000000f", "20 1c 44 92"),
        ("and x0, x1, 0x7c0000007c", "20 10 1e 92"),
        ("and x0, x1, 0xfffffffffffffffe", "20 f8 7f 92"),
        ("orr sp, x0, 0xfff", "1f 2c 40 b2"),
    ]);
}

/// `bic`, `bics`, `orn` and `eon` with an immediate, which are the plain
/// mnemonic of the complement. Bytes from llvm-mc; GNU as writes the same
/// for `bic`, the only one of the four it takes.
#[test]
fn inverted_logical_immediate() {
    check(&[
        ("bic x0, x1, 0xff", "20 dc 78 92"),
        ("bic w0, w1, 1", "20 78 1f 12"),
        ("bic sp, x1, 0xff", "3f dc 78 92"),
        ("bics x0, x1, 0xff", "20 dc 78 f2"),
        ("bics xzr, x1, 0xff", "3f dc 78 f2"),
        ("orn x0, x1, 0xff", "20 dc 78 b2"),
        ("orn w0, w1, 0xf", "20 6c 1c 32"),
        ("eon x0, x1, 0xff", "20 dc 78 d2"),
    ]);
    // The complement has to be encodable: both references refuse these.
    for src in ["bic x0, x1, 0", "bic w0, w1, 0xffffffff", "eon x0, x1, -1"] {
        let e = errors_for("aarch64", src);
        assert!(e.contains("complement"), "`{src}`: {e}");
    }
}

/// Move-wide, and the `mov #imm` alias choosing between `movz`, `movn` and
/// a logical immediate.
#[test]
fn moves() {
    check(&[
        ("movz x0, 1", "20 00 80 d2"),
        ("movz x0, 1, lsl 16", "20 00 a0 d2"),
        ("movz x0, 0xabcd, lsl 48", "a0 79 f5 d2"),
        ("movk w0, 0x1234, lsl 16", "80 46 a2 72"),
        ("movn x0, 0", "00 00 80 92"),
        ("movn w0, 65535, lsl 16", "e0 ff bf 12"),
        ("mov x0, 1", "20 00 80 d2"),
        ("mov x0, 65535", "e0 ff 9f d2"),
        ("mov x0, 0x10000", "20 00 a0 d2"),
        ("mov w0, 0x10000", "20 00 a0 52"),
        ("mov x0, -1", "00 00 80 92"),
        ("mov x0, 0xffff00000000", "e0 ff df d2"),
        ("mov x0, 0xffff0000ffff0000", "e0 3f 10 b2"),
    ]);
}

/// The bitfield moves and the eight aliases the ARM ARM defines on them.
#[test]
fn bitfield() {
    check(&[
        ("sbfm x0, x1, 3, 7", "20 1c 43 93"),
        ("ubfm w0, w1, 4, 9", "20 24 04 53"),
        ("bfm x0, x1, 8, 12", "20 30 48 b3"),
        ("sbfx x0, x1, 3, 5", "20 1c 43 93"),
        ("ubfx w0, w1, 2, 6", "20 1c 02 53"),
        ("bfxil x0, x1, 4, 8", "20 2c 44 b3"),
        ("sbfiz x0, x1, 4, 8", "20 1c 7c 93"),
        ("bfi x0, x1, 4, 8", "20 1c 7c b3"),
        ("lsl x0, x1, 3", "20 f0 7d d3"),
        ("lsr w0, w1, 5", "20 7c 05 53"),
        ("asr x0, x1, 9", "20 fc 49 93"),
        ("ror x0, x1, 5", "20 14 c1 93"),
        ("extr x0, x1, x2, 8", "20 20 c2 93"),
        ("sxtb x0, w1", "20 1c 40 93"),
        ("sxth w0, w1", "20 3c 00 13"),
        ("sxtw x0, w1", "20 7c 40 93"),
        ("uxtb w0, w1", "20 1c 00 53"),
        ("uxth w0, w1", "20 3c 00 53"),
    ]);
}

#[test]
fn multiply_divide_and_variable_shifts() {
    check(&[
        ("lsl x0, x1, x2", "20 20 c2 9a"),
        ("lsr w0, w1, w2", "20 24 c2 1a"),
        ("asr x0, x1, x2", "20 28 c2 9a"),
        ("ror x0, x1, x2", "20 2c c2 9a"),
        ("mul x0, x1, x2", "20 7c 02 9b"),
        ("madd x0, x1, x2, x3", "20 0c 02 9b"),
        ("msub w0, w1, w2, w3", "20 8c 02 1b"),
        ("mneg x0, x1, x2", "20 fc 02 9b"),
        ("smull x0, w1, w2", "20 7c 22 9b"),
        ("umull x0, w1, w2", "20 7c a2 9b"),
        ("smaddl x0, w1, w2, x3", "20 0c 22 9b"),
        ("smulh x0, x1, x2", "20 7c 42 9b"),
        ("umulh x0, x1, x2", "20 7c c2 9b"),
        ("sdiv x0, x1, x2", "20 0c c2 9a"),
        ("udiv w0, w1, w2", "20 08 c2 1a"),
    ]);
}

#[test]
fn one_source_data_processing() {
    check(&[
        ("rbit x0, x1", "20 00 c0 da"),
        ("rev x0, x1", "20 0c c0 da"),
        ("rev w0, w1", "20 08 c0 5a"),
        ("rev16 w0, w1", "20 04 c0 5a"),
        ("rev32 x0, x1", "20 08 c0 da"),
        ("clz x0, x1", "20 10 c0 da"),
        ("cls w0, w1", "20 14 c0 5a"),
    ]);
}

#[test]
fn conditional() {
    check(&[
        ("csel x0, x1, x2, eq", "20 00 82 9a"),
        ("csinc w0, w1, w2, ne", "20 14 82 1a"),
        ("csinv x0, x1, x2, lt", "20 b0 82 da"),
        ("csneg x0, x1, x2, ge", "20 a4 82 da"),
        ("cset w0, eq", "e0 17 9f 1a"),
        ("csetm x0, ne", "e0 03 9f da"),
        ("cinc x0, x1, eq", "20 14 81 9a"),
        ("cinv x0, x1, vs", "20 70 81 da"),
        ("cneg w0, w1, mi", "20 54 81 5a"),
        ("ccmp x0, x1, 3, eq", "03 00 41 fa"),
        ("ccmp w0, 5, 0, ne", "00 18 45 7a"),
        ("ccmn x0, x1, 15, al", "0f e0 41 ba"),
    ]);
}

#[test]
fn register_branches() {
    check(&[
        ("br x0", "00 00 1f d6"),
        ("blr x1", "20 00 3f d6"),
        ("ret", "c0 03 5f d6"),
        ("ret x0", "00 00 5f d6"),
        ("eret", "e0 03 9f d6"),
    ]);
}

/// Every addressing mode: scaled unsigned offset, unscaled, pre- and
/// post-index, and register offset with an extend.
#[test]
fn loads_and_stores() {
    check(&[
        ("ldr x0, [x1]", "20 00 40 f9"),
        ("ldr x0, [x1, 8]", "20 04 40 f9"),
        ("ldr w0, [x1, 4]", "20 04 40 b9"),
        ("ldr x0, [x1, 32760]", "20 fc 7f f9"),
        ("str x0, [x1, 8]", "20 04 00 f9"),
        ("strb w0, [x1, 1]", "20 04 00 39"),
        ("ldrb w0, [x1, 4095]", "20 fc 7f 39"),
        ("ldrh w0, [x1, 8190]", "20 fc 7f 79"),
        ("strh w0, [x1, 2]", "20 04 00 79"),
        ("ldrsw x0, [x1, 4]", "20 04 80 b9"),
        ("ldrsb x0, [x1, 1]", "20 04 80 39"),
        ("ldrsb w0, [x1, 1]", "20 04 c0 39"),
        ("ldrsh x0, [x1, 2]", "20 04 80 79"),
        ("ldr q0, [x1, 16]", "20 04 c0 3d"),
        ("str q0, [x1]", "20 00 80 3d"),
        ("ldr d0, [x1, 8]", "20 04 40 fd"),
        ("ldr b0, [x1]", "20 00 40 3d"),
        ("ldr h0, [x1, 2]", "20 04 40 7d"),
        ("prfm pldl1keep, [x0]", "00 00 80 f9"),
        ("ldur x0, [x1, -8]", "20 80 5f f8"),
        ("stur w0, [x1, 255]", "20 f0 0f b8"),
        ("ldursw x0, [x1, -4]", "20 c0 9f b8"),
        ("ldr x0, [x1, 8]!", "20 8c 40 f8"),
        ("ldr x0, [x1], 8", "20 84 40 f8"),
        ("str x0, [sp, -16]!", "e0 0f 1f f8"),
        ("ldr x0, [sp], 16", "e0 07 41 f8"),
        ("ldr x0, [x1, x2]", "20 68 62 f8"),
        ("ldr x0, [x1, x2, lsl 3]", "20 78 62 f8"),
        ("ldr w0, [x1, w2, uxtw 2]", "20 58 62 b8"),
        ("ldr w0, [x1, w2, sxtw]", "20 c8 62 b8"),
        ("ldr x0, [x1, x2, sxtx 3]", "20 f8 62 f8"),
        ("strb w0, [x1, x2]", "20 68 22 38"),
        ("ldrsw x0, [x1, x2, lsl 2]", "20 78 a2 b8"),
    ]);
}

#[test]
fn load_and_store_pairs() {
    check(&[
        ("ldp x0, x1, [x2]", "40 04 40 a9"),
        ("ldp x0, x1, [x2, 16]", "40 04 41 a9"),
        ("ldp x0, x1, [x2, -512]", "40 04 60 a9"),
        ("stp x29, x30, [sp, -16]!", "fd 7b bf a9"),
        ("ldp x29, x30, [sp], 16", "fd 7b c1 a8"),
        ("stp w0, w1, [x2, 8]", "40 04 01 29"),
        ("ldpsw x0, x1, [x2, 8]", "40 04 41 69"),
        ("stp d0, d1, [sp, -16]!", "e0 07 bf 6d"),
        ("ldp q0, q1, [x0, 32]", "00 04 41 ad"),
        ("stnp x0, x1, [x2, 16]", "40 04 01 a8"),
    ]);
}

#[test]
fn system() {
    check(&[
        ("nop", "1f 20 03 d5"),
        ("yield", "3f 20 03 d5"),
        ("wfe", "5f 20 03 d5"),
        ("wfi", "7f 20 03 d5"),
        ("sev", "9f 20 03 d5"),
        ("sevl", "bf 20 03 d5"),
        ("svc 0", "01 00 00 d4"),
        ("brk 1000", "00 7d 20 d4"),
        ("hlt 1", "20 00 40 d4"),
        ("isb", "df 3f 03 d5"),
        ("dmb sy", "bf 3f 03 d5"),
        ("dmb ish", "bf 3b 03 d5"),
        ("dmb ishst", "bf 3a 03 d5"),
        ("dsb nsh", "9f 37 03 d5"),
        ("clrex", "5f 3f 03 d5"),
        ("mrs x0, nzcv", "00 42 3b d5"),
        ("mrs x0, tpidr_el0", "40 d0 3b d5"),
        ("mrs x0, s3_3_c13_c0_3", "60 d0 3b d5"),
        ("msr nzcv, x0", "00 42 1b d5"),
        ("msr daifset, 2", "df 42 03 d5"),
        ("msr spsel, 0", "bf 40 00 d5"),
    ]);
}

#[test]
fn simd() {
    check(&[
        ("add v0.4s, v1.4s, v2.4s", "20 84 a2 4e"),
        ("add v0.8b, v1.8b, v2.8b", "20 84 22 0e"),
        ("sub v0.16b, v1.16b, v2.16b", "20 84 22 6e"),
        ("and v0.8b, v1.8b, v2.8b", "20 1c 22 0e"),
        ("orr v0.16b, v1.16b, v2.16b", "20 1c a2 4e"),
        ("eor v0.16b, v1.16b, v2.16b", "20 1c 22 6e"),
        ("mov v0.16b, v1.16b", "20 1c a1 4e"),
        ("dup v0.4s, w1", "20 0c 04 4e"),
        ("dup v0.2d, x1", "20 0c 08 4e"),
        ("dup v0.4s, v1.s[2]", "20 04 14 4e"),
        ("fmov d0, x1", "20 00 67 9e"),
        ("fmov x0, d1", "20 00 66 9e"),
        ("fmov s0, w1", "20 00 27 1e"),
        ("fmov d0, d1", "20 40 60 1e"),
    ]);
}

/// The stack pointer and the zero register share number 31, and which one a
/// field means depends on the instruction. These are the cases where picking
/// the wrong form silently changes the program.
#[test]
fn stack_pointer_forms() {
    check(&[
        ("add w0, wsp, w1", "e0 43 21 0b"),
        ("add wsp, w1, w2, lsl 2", "3f 48 22 0b"),
        ("add sp, x1, x2, lsl 3", "3f 6c 22 8b"),
        ("sub sp, sp, x1, lsl 2", "ff 6b 21 cb"),
        ("adds x0, sp, x1", "e0 63 21 ab"),
        ("cmp sp, x1", "ff 63 21 eb"),
        ("cmn sp, 4", "ff 13 00 b1"),
        ("mov wsp, w1", "3f 00 00 11"),
        ("mov w0, wsp", "e0 03 00 11"),
        ("and sp, x1, 3", "3f 04 40 92"),
        ("str xzr, [sp]", "ff 03 00 f9"),
        ("mov w0, wzr", "e0 03 1f 2a"),
        ("adds xzr, x1, 1", "3f 04 00 b1"),
        ("ands xzr, x1, 1", "3f 00 40 f2"),
        ("add x0, x1, xzr", "20 00 1f 8b"),
    ]);
}

#[test]
fn index_shift_selects_the_s_bit() {
    check(&[
        // A byte access has only one shift amount, but writing it still sets S.
        ("ldrb w0, [x1, x2, lsl 0]", "20 78 62 38"),
        ("ldrh w0, [x1, x2, lsl 0]", "20 68 62 78"),
        ("ldr x0, [x1, x2, lsl 0]", "20 68 62 f8"),
        ("ldr w0, [x1, w2, uxtw 0]", "20 48 62 b8"),
        // An offset that does not scale falls back to the unscaled form.
        ("ldr x0, [x1, 3]", "20 30 40 f8"),
        ("ldr w0, [x1, -4]", "20 c0 5f b8"),
    ]);
}

#[test]
fn add_immediates_choose_their_own_shift() {
    check(&[
        ("add x0, x1, 4096", "20 04 40 91"),
        ("add x0, x1, 0x7ff000", "20 fc 5f 91"),
        ("add x0, x1, 0xfff000", "20 fc 7f 91"),
        ("sub w0, w1, -4096", "20 04 40 11"),
        ("cmp x0, 8192", "1f 08 40 f1"),
    ]);
}

#[test]
fn mov_immediate_picks_the_same_form_as_llvm() {
    check(&[
        ("mov x0, 0", "00 00 80 d2"),
        ("mov w0, -1", "00 00 80 12"),
        ("mov w0, 0xffff0000", "e0 ff bf 52"),
        ("mov x0, 0xffffffff", "e0 7f 40 b2"),
        ("mov x0, 0x0000ffffffffffff", "e0 ff ff 92"),
        ("mov x0, 0xffffffffffff0000", "e0 ff 9f 92"),
    ]);
}

// ---- programs: labels, branches, fixups --------------------------------------

#[test]
fn unconditional_branches_resolve_both_directions() {
    program(
        "back:\n b back\n b fwd\n bl back\n bl fwd\nfwd:\n ret\n",
        "00 00 00 14 03 00 00 14 fe ff ff 97 01 00 00 94 c0 03 5f d6",
    );
}

#[test]
fn conditional_branches() {
    program(
        "top:\n b.eq top\n b.ne bot\n bne bot\n b.al top\nbot:\n nop\n",
        "00 00 00 54 61 00 00 54 41 00 00 54 ae ff ff 54 1f 20 03 d5",
    );
}

#[test]
fn compare_and_test_branches() {
    // `tbnz x2, 32` puts the top bit of the bit number in the width bit.
    program(
        "l:\n cbz x0, l\n cbnz w1, o\n tbz x0, 0, l\n tbnz x2, 32, o\no:\n ret\n",
        "00 00 00 b4 61 00 00 35 c0 ff 07 36 22 00 00 b7 c0 03 5f d6",
    );
}

#[test]
fn adr_is_byte_granular() {
    program(
        "h:\n adr x0, h\n adr x1, t\n adr x2, h - 4\nt:\n nop\n",
        "00 00 00 10 41 00 00 10 a2 ff ff 10 1f 20 03 d5",
    );
}

#[test]
fn literal_loads() {
    program(
        "p:\n .quad 0x0011223344556677\nc:\n ldr x0, p\n ldr w1, p\n ldr q2, p\n",
        "77 66 55 44 33 22 11 00 c0 ff ff 58 a1 ff ff 18 82 ff ff 9c",
    );
}

#[test]
fn a_function_prologue_and_epilogue() {
    program(
        "add3:\n stp x29, x30, [sp, -32]!\n mov x29, sp\n add w0, w0, w1\n \
         add w0, w0, w2\n ldp x29, x30, [sp], 32\n ret\n",
        "fd 7b be a9 fd 03 00 91 00 00 01 0b 00 00 02 0b fd 7b c2 a8 c0 03 5f d6",
    );
}

#[test]
fn alignment_padding_is_real_nops() {
    program(
        " nop\n .p2align 4\n ret\n",
        "1f 20 03 d5 1f 20 03 d5 1f 20 03 d5 1f 20 03 d5 c0 03 5f d6",
    );
}

#[test]
fn symbolic_constants_fold_into_immediates() {
    program(
        " .set slot, 3\n add x0, x1, slot * 8\n ldr x2, [x3, slot * 8]\n \
         movz x4, slot\n lsl x5, x6, slot\n",
        "20 60 00 91 62 0c 40 f9 64 00 80 d2 c5 f0 7d d3",
    );
}

/// First and last instruction word of a snippet, for range tests whose
/// padding would drown the comparison.
fn ends(src: &str) -> (String, String) {
    let b = text_for(ARCH, src);
    (hex(&b[..4]), hex(&b[b.len() - 4..]))
}

#[test]
fn branches_reach_exactly_their_documented_range() {
    // Largest forward and backward displacement of each field. A 19-bit field
    // of words reaches -1MB..+1MB-4; 14 bits reach -32KB..+32KB-4; `adr`
    // counts bytes, so it reaches one byte further forward.
    let cases = [
        (
            "b.eq far\n.space 0xffff8\nfar: nop",
            "e0 ff 7f 54",
            "1f 20 03 d5",
        ),
        (
            "far: nop\n.space 0xffffc\nb.ne far",
            "1f 20 03 d5",
            "01 00 80 54",
        ),
        (
            "cbz x1, far\n.space 0xffff8\nfar: nop",
            "e1 ff 7f b4",
            "1f 20 03 d5",
        ),
        (
            "tbz x0, 1, far\n.space 0x7ff8\nfar: nop",
            "e0 ff 0b 36",
            "1f 20 03 d5",
        ),
        (
            "far: nop\n.space 0x7ffc\ntbnz w3, 7, far",
            "1f 20 03 d5",
            "03 00 3c 37",
        ),
        (
            "adr x0, far\n.space 0xffffb\nfar: nop",
            "e0 ff 7f 70",
            "1f 20 03 d5",
        ),
        (
            "far: nop\n.space 0xffffc\nadr x0, far",
            "1f 20 03 d5",
            "00 00 80 10",
        ),
        (
            "ldr x0, far\n.space 0xffff8\nfar: nop",
            "e0 ff 7f 58",
            "1f 20 03 d5",
        ),
    ];
    for (src, first, last) in cases {
        let (f, l) = ends(src);
        assert_eq!((f.as_str(), l.as_str()), (first, last), "{src}");
    }
}

#[test]
fn b_and_bl_reach_128mb() {
    let (f, _) = ends("b far\n.space 0x7fffff8\nfar: nop");
    assert_eq!(f, "ff ff ff 15");
    let (_, l) = ends("far: nop\n.space 0x7fffffc\nb far");
    assert_eq!(l, "00 00 00 16");
}

#[test]
fn out_of_range_branches_are_diagnosed() {
    for src in [
        "b far\n.space 0x7fffffc\nfar: nop",
        "b.eq far\n.space 0xffffc\nfar: nop",
        "cbnz x0, far\n.space 0xffffc\nfar: nop",
        "tbz x0, 1, far\n.space 0x7ffc\nfar: nop",
        "far: nop\n.space 0x8000\ntbz x0, 1, far",
        "adr x0, far\n.space 0xffffc\nfar: nop",
    ] {
        rejects(src, &["out of range"]);
    }
}

#[test]
fn misaligned_branch_targets_are_diagnosed() {
    // A branch counts whole instructions; a target two bytes away cannot be
    // rounded to one.
    rejects("b odd\n.byte 0\nodd: nop", &["not a multiple of 4"]);
    rejects("cbz x0, odd\n.2byte 0\nodd: nop", &["not a multiple of 4"]);
}

// ---- relocations -------------------------------------------------------------

/// Relocation types, in order, for an assembled source.
fn relocs(src: &str) -> Vec<u32> {
    let asm = assemble_for(ARCH, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm.relocs.iter().map(|r| r.kind).collect()
}

#[test]
fn external_references_use_the_aarch64_relocation_numbers() {
    let kinds = relocs(
        " adrp x0, ext\n adr x1, ext\n b ext\n bl ext\n b.eq ext\n cbz x0, ext\n \
         tbz x0, 3, ext\n ldr x2, ext\n .quad ext\n .4byte ext\n",
    );
    assert_eq!(
        kinds,
        [
            275, // ADR_PREL_PG_HI21
            274, // ADR_PREL_LO21
            282, // JUMP26
            283, // CALL26
            280, // CONDBR19
            280, // CONDBR19
            279, // TSTBR14
            273, // LD_PREL_LO19
            257, // ABS64
            258, // ABS32
        ]
    );
}

#[test]
fn adrp_relocates_even_within_its_own_section() {
    // The page of the target depends on where the linker puts the section, so
    // the field is left zero and relocated, exactly as GNU as and llvm-mc do.
    let src = "base:\n adrp x0, base\n adrp x1, there\nthere:\n nop\n";
    assert_eq!(
        hex(&text_for(ARCH, src)),
        "00 00 00 90 01 00 00 90 1f 20 03 d5"
    );
    assert_eq!(relocs(src), [275, 275]);
}

#[test]
fn lo12_and_got_operators() {
    let src = "f:\n adrp x0, var\n add x0, x0, :lo12:var\n ldr x2, [x0, :lo12:var]\n \
               ldr w3, [x0, :lo12:var]\n ldrb w4, [x0, :lo12:var]\n \
               ldrh w5, [x0, :lo12:var]\n ldr q6, [x0, :lo12:var]\n \
               adrp x8, :got:var\n ldr x9, [x8, :got_lo12:var]\n";
    assert_eq!(
        hex(&text_for(ARCH, src)),
        "00 00 00 90 00 00 00 91 02 00 40 f9 03 00 40 b9 04 00 40 39 \
         05 00 40 79 06 00 c0 3d 08 00 00 90 09 01 40 f9"
    );
    assert_eq!(
        relocs(src),
        [
            275, // ADR_PREL_PG_HI21
            277, // ADD_ABS_LO12_NC
            286, // LDST64_ABS_LO12_NC
            285, // LDST32_ABS_LO12_NC
            278, // LDST8_ABS_LO12_NC
            284, // LDST16_ABS_LO12_NC
            299, // LDST128_ABS_LO12_NC
            311, // ADR_GOT_PAGE
            312, // LD64_GOT_LO12_NC
        ]
    );
}

#[test]
fn elf_machine_and_data_relocations() {
    let a = rsasm::arch::lookup(ARCH).expect("backend present");
    assert_eq!(a.elf_machine(), 183);
    assert_eq!(a.data_reloc(8, false), Some(257));
    assert_eq!(a.data_reloc(4, false), Some(258));
    assert_eq!(a.data_reloc(8, true), Some(260));
    assert_eq!(a.data_reloc(4, true), Some(261));
    assert_eq!(a.data_reloc(1, false), None);
    for alias in ["arm64", "armv8"] {
        assert_eq!(rsasm::arch::lookup(alias).map(|a| a.name()), Some(ARCH));
    }
}

#[test]
fn nop_fill_is_executable() {
    let a = rsasm::arch::lookup(ARCH).expect("backend present");
    let st = a.initial_state();
    assert_eq!(
        a.nop_fill(&st, 8),
        [0x1f, 0x20, 0x03, 0xd5, 0x1f, 0x20, 0x03, 0xd5]
    );
    // Padding that does not land on an instruction boundary cannot be
    // instructions; it must at least have the right length.
    assert_eq!(a.nop_fill(&st, 6).len(), 6);
    assert!(a.nop_fill(&st, 0).is_empty());
}

// ---- diagnostics -------------------------------------------------------------

#[test]
fn immediate_range_violations_name_the_limit() {
    rejects("add x0, x1, 4097", &["0..=4095", "multiple of 4096"]);
    rejects("add x0, x1, 0x1000000", &["0xfff000"]);
    rejects("add x0, x1, 1, lsl 8", &["0 or 12"]);
    rejects("add x0, x1, x2, lsl 64", &["0..=63"]);
    rejects("add w0, w1, w2, lsl 32", &["0..=31"]);
    rejects("movz x0, 0x10000", &["65535"]);
    rejects("movz x0, 1, lsl 17", &["0, 16, 32 or 48"]);
    rejects("movz w0, 1, lsl 32", &["0 or 16"]);
    rejects("ldr x0, [x1, 32768]", &["-256..=255", "32760"]);
    rejects("ldr x0, [x1, 256]!", &["-256..=255"]);
    rejects("ldp x0, x1, [x2, 512]", &["-512..=504"]);
    rejects("ldp x0, x1, [x2, 3]", &["multiple of 8"]);
    rejects("ldr w0, [x1, x2, lsl 3]", &["by 2"]);
    rejects("tbz w0, 32, 0", &["0..=31"]);
    rejects("svc 65536", &["0..=65535"]);
    rejects("ccmp x0, x1, 16, eq", &["0..=15"]);
    rejects("lsl x0, x1, 64", &["0..=63"]);
    rejects("ubfx x0, x1, 60, 8", &["1..=4"]);
}

#[test]
fn invalid_logical_immediates_are_explained() {
    for v in ["0", "5", "-1", "0xffffffff"] {
        rejects(
            &format!("and w0, w1, {v}"),
            &["not a valid logical immediate"],
        );
    }
    rejects("and x0, x1, 0", &["repeating run of ones"]);
    rejects("orr x0, x1, 0x123", &["not a valid logical immediate"]);
    // `bic` takes an immediate whose complement is encodable, as both
    // references do: this is `and x0, x1, 0xfffffffffffffffe`.
    check(&[("bic x0, x1, 1", "20 f8 7f 92")]);
}

#[test]
fn operand_shape_errors() {
    rejects("add w0, x1, x2", &["cannot mix 32-bit and 64-bit"]);
    rejects("ldp x0, w1, [x2]", &["same width"]);
    rejects("ldr x0, [w1]", &["64-bit register"]);
    rejects("ldr x0, [x1, w2]", &["`uxtw` or `sxtw`"]);
    rejects("ldr x0, [x1, sp]", &["stack pointer"]);
    rejects("add x0, x1, sym", &["constant"]);
    rejects("mov sp, 1", &["stack pointer"]);
    rejects("adds sp, x1, 1", &["stack pointer"]);
    rejects("tst sp, x1", &["stack pointer"]);
    rejects("mov x0, wzr", &["cannot mix"]);
    rejects("mov x0, 0x123456789", &["one instruction"]);
    rejects("cset x0, al", &["cannot be inverted"]);
    rejects("mrs x0, bogus", &["unknown system register"]);
    rejects("frobnicate x0", &["unknown instruction"]);
    rejects("add x0, x1", &["3 or 4 operand"]);
    rejects("ldr x0, [x1", &["unterminated"]);
    rejects("ldr x0, [x1]]", &["after `]`"]);
    rejects("sub x0, x0, :lo12:var", &["plain `add`"]);
    rejects("ldp x0, x1, [x2, :lo12:var]", &[":lo12:"]);
    rejects("b :lo12:var", &[":lo12:"]);
    rejects("smull x0, x1, x2", &["two `w` sources"]);
}

/// Where a field reads register 31 as the stack pointer, `xzr` must be refused
/// rather than silently becoming `sp` — llvm-mc rejects every one of these.
#[test]
fn zero_register_is_refused_where_31_means_sp() {
    for src in [
        "add xzr, x1, 1",
        "add x0, xzr, 1",
        "cmp xzr, 1",
        "mov sp, xzr",
        "mov xzr, sp",
        "add sp, xzr, x1",
        "add x0, xzr, w1, uxtw",
        "and xzr, x1, 1",
        "orr xzr, xzr, 1",
    ] {
        rejects(src, &["zero register"]);
    }
}

// ---- robustness --------------------------------------------------------------

/// Nonsense in every operand position the parser has. The only requirement
/// is that assembly reports something and returns.
const MALFORMED: &[&str] = &[
    "add",
    "add ,",
    "add x0,",
    "add , x0",
    "add x0, x1, ,",
    "add x0 x1 x2",
    "add x0, x1, lsl",
    "add x0, x1, x2, lsl",
    "add x0, x1, x2, lsl ,",
    "add x0, x1, x2, uxtb 99999999999999999999",
    "add x0, x1, x2, lsl 3, lsl 3",
    "add x0, x1, 1, lsl 12, 3",
    "add x0, x1, x2, sxtw",
    "add v0.4s, v1.4s",
    "add v0.4s, v1.8b, v2.4s",
    "add v0.1d, v1.1d, v2.1d",
    "mov",
    "mov x0",
    "mov x0, [x1]",
    "mov v0.4s, v1.4s",
    "mov x0, eq",
    "movz x0, 1, lsl",
    "movk x0, 1, asr 16",
    "and x0, x1, 1, lsl 3",
    "and x0, x1, 99999999999999999999999",
    "ldr",
    "ldr x0",
    "ldr x0, [",
    "ldr x0, []",
    "ldr x0, [,]",
    "ldr x0, [x1,",
    "ldr x0, [x1,]",
    "ldr x0, [x1, x2, x3]",
    "ldr x0, [x1, x2, lsl]",
    "ldr x0, [x1, x2, asr 3]",
    "ldr x0, [x1, x2, lsl 3]!",
    "ldr x0, [x1]!",
    "ldr x0, [x1], ",
    "ldr x0, [x1], x2",
    "ldr x0, [x1] 8",
    "ldr x0, [[x1]]",
    "ldr x0, [x1, :lo12:",
    "ldr x0, [x1, :lo12]",
    "ldr x0, [x1, :nope:sym]",
    "ldr x0, [x1, :lo12:sym]!",
    "ldur x0, [x1, :lo12:sym]",
    "ldr sp, [x1]",
    "ldr v0.4s, [x1]",
    "ldr x0, [v0.4s]",
    "ldr x0, [1]",
    "ldr x0, ]",
    "ldp x0, [x1]",
    "ldp x0, x1, [x2, x3]",
    "ldp b0, b1, [x2]",
    "ldpsw w0, w1, [x2]",
    "prfm nothing, [x0]",
    "prfm pld, [x0]",
    "prfm 32, [x0]",
    "b",
    "b ,",
    "b x0",
    "b [x0]",
    "b.eq",
    "b.foo 0",
    "cbz 0",
    "cbz sp, 0",
    "tbz x0, 0",
    "tbz x0, 64, 0",
    "tbz x0, sym, 0",
    "br",
    "br w0",
    "br 0",
    "ret x0, x1",
    "adr 0, 0",
    "adrp w0, sym",
    "adrp x0, :lo12:sym",
    "csel x0, x1, x2",
    "csel x0, x1, x2, x3",
    "cset x0",
    "cinc x0, x1, nv",
    "ccmp x0, x1, 3",
    "ccmp x0, x1, eq, 3",
    "ccmp x0, sp, 3, eq",
    "sbfx x0, x1, 0, 0",
    "sbfx x0, x1, 64, 1",
    "ubfiz w0, w1, 31, 2",
    "bfi x0, x1, -1, 1",
    "lsl x0, x1",
    "lsl x0, x1, -1",
    "sxtw w0, w1",
    "sxtb x0, x1",
    "uxtb x0, w1",
    "rev32 w0, w1",
    "smulh w0, w1, w2",
    "madd x0, w1, x2, x3",
    "mrs",
    "mrs x0",
    "mrs w0, nzcv",
    "mrs x0, s3_3_c13_c0",
    "mrs x0, s3_3_c13_c0_99",
    "mrs x0, s1_0_c0_c0_0",
    "msr daifset, x0",
    "msr daifset, 16",
    "msr nzcv, 1",
    "dmb nothing",
    "dmb 16",
    "isb sy, sy",
    "hint 128",
    "nop x0",
    "svc",
    "svc -1",
    "dup v0.4s, v1.s[4]",
    "dup v0.4s, v1.d[0]",
    "dup v0.4s, x1",
    "dup v0.s, w1",
    "dup v0.4s, v1.s[99999999999999999999]",
    "fmov x0, s1",
    "fmov d0, s1",
    "fmov q0, q1",
    "add {v0.16b, v1.16b}, x0, x1",
    "add {v0.16b,",
    "add {x0}, x1, x2",
    "add {v0.16b]",
    "add x0, x1, #",
    "add x0, x1, :",
    "add x0, x1, :lo12:",
    "add x0, x1, :lo12:sym, lsl 12",
    "x0",
    "w31",
    "b.",
    "b.eq.ne 0",
];

/// Random operand soup after every mnemonic the backend knows.
///
/// Two properties: nothing panics, and nothing is dropped silently. A64 is
/// fixed-width, so a line that assembles without a diagnostic must produce
/// exactly one four-byte word — anything else means an encoder returned
/// without reporting why.
#[test]
fn random_operands_never_panic_or_vanish() {
    const MNEMONICS: &[&str] = &[
        "add", "adds", "sub", "subs", "cmp", "cmn", "neg", "negs", "adc", "sbcs", "ngc", "and",
        "ands", "orr", "eor", "bic", "orn", "eon", "tst", "mvn", "mov", "movz", "movn", "movk",
        "sbfm", "ubfm", "bfm", "sbfx", "ubfx", "bfxil", "sbfiz", "ubfiz", "bfi", "sxtb", "sxth",
        "sxtw", "uxtb", "uxth", "lsl", "lsr", "asr", "ror", "lslv", "extr", "mul", "madd", "msub",
        "mneg", "smull", "umull", "smaddl", "umsubl", "smulh", "umulh", "sdiv", "udiv", "rbit",
        "rev", "rev16", "rev32", "clz", "cls", "csel", "csinc", "csinv", "csneg", "cset", "csetm",
        "cinc", "cinv", "cneg", "ccmp", "ccmn", "b", "bl", "b.eq", "bne", "cbz", "cbnz", "tbz",
        "tbnz", "br", "blr", "ret", "eret", "adr", "adrp", "ldr", "str", "ldrb", "strb", "ldrh",
        "strh", "ldrsb", "ldrsh", "ldrsw", "ldur", "stur", "prfm", "ldp", "stp", "ldpsw", "stnp",
        "nop", "wfi", "hint", "dmb", "isb", "clrex", "svc", "brk", "hlt", "mrs", "msr", "dup",
        "fmov",
    ];
    const PIECES: &[&str] = &[
        "x0",
        "w1",
        "sp",
        "wsp",
        "xzr",
        "wzr",
        "x30",
        "q0",
        "d1",
        "s2",
        "h3",
        "b4",
        "v0.4s",
        "v1.16b",
        "v2.2d",
        "v3.s[1]",
        "v4.b[99]",
        "0",
        "1",
        "-1",
        "4095",
        "4096",
        "0xff",
        "0xffffffff",
        "-0x8000000000000000",
        "99999999999999999999",
        "sym",
        "sym+4",
        ".",
        "(1",
        "1/0",
        "lsl 3",
        "lsl 12",
        "asr 64",
        "ror",
        "uxtw 2",
        "sxtx",
        "uxtb 9",
        "eq",
        "al",
        "nv",
        "hs",
        "nzcv",
        "daifset",
        "tpidr_el0",
        "s3_3_c13_c0_2",
        "pldl1keep",
        "ish",
        "[x0]",
        "[x0, 8]",
        "[x0, -8]!",
        "[x0], 8",
        "[sp, x1, lsl 3]",
        "[x0, w1, sxtw]",
        "[x0,",
        "[x0, :lo12:sym]",
        "[w0]",
        "[]",
        "]",
        "[x0]!",
        ":lo12:sym",
        ":got:sym",
        ":bad:sym",
        "{v0.16b}",
        "{",
        "}",
        "!",
        "",
        "p",
        "pld",
        "plil9keep",
        "spsel",
        "daifclr",
        "sy",
        "fp",
        "lr",
    ];
    // A fixed-seed xorshift keeps the test deterministic without a dependency.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    for _ in 0..4000 {
        let mut src = MNEMONICS[next(MNEMONICS.len())].to_string();
        let count = next(6);
        for k in 0..count {
            src.push_str(if k == 0 { " " } else { ", " });
            src.push_str(PIECES[next(PIECES.len())]);
        }
        if let Ok(bytes) = try_text_for(ARCH, &src) {
            assert_eq!(
                bytes.len(),
                4,
                "`{src}` assembled without a diagnostic but produced {} bytes",
                bytes.len()
            );
        }
    }
}

#[test]
fn malformed_input_never_panics() {
    for src in MALFORMED {
        // The result is irrelevant; not unwinding is the test.
        let _ = try_text_for(ARCH, src);
    }
}

#[test]
fn malformed_input_is_reported_rather_than_ignored() {
    // Everything in the list above is an error, not just a non-panic.
    for src in MALFORMED {
        assert!(
            try_text_for(ARCH, src).is_err(),
            "`{src}` should have produced a diagnostic"
        );
    }
}

// ---- GNU-style AArch64 spelling -----------------------------------------
//
// Expected bytes are from `llvm-mc -triple=aarch64`. Before comments became a
// per-target setting none of these could be written: `#` started a comment,
// so `add x0, x1, #1` reached the backend as `add x0, x1,`.

#[test]
fn hash_immediates_are_not_comments() {
    assert_eq!(hex(&text_for("aarch64", "add x0, x1, #1\n")), "20 04 00 91");
    assert_eq!(
        hex(&text_for("aarch64", "movz x0, #0x1234, lsl #16\n")),
        "80 46 a2 d2"
    );
    assert_eq!(
        hex(&text_for("aarch64", "ldr x0, [x1, #8]!\n")),
        "20 8c 40 f8"
    );
    assert_eq!(
        hex(&text_for("aarch64", "and x0, x1, #0xff\n")),
        "20 1c 40 92"
    );
}

#[test]
fn slash_comments_and_first_column_hash_comments() {
    assert_eq!(
        hex(&text_for("aarch64", "mov x0, #42 // the answer\n")),
        "40 05 80 d2"
    );
    assert_eq!(
        hex(&text_for("aarch64", "# 1 \"f.c\"\nret\n")),
        "c0 03 5f d6"
    );
}

#[test]
fn word_is_four_bytes_on_aarch64() {
    assert_eq!(
        hex(&text_for("aarch64", ".word 0x11223344\n")),
        "44 33 22 11"
    );
}
/// AdvSIMD data processing: same-width, widening and narrowing
/// arithmetic, indexed elements, reductions and comparisons, scalar forms
/// included.
#[test]
fn advsimd_data_processing() {
    check(&[
        ("add v0.4s, v1.4s, v2.4s", "20 84 a2 4e"),
        ("sqadd v31.16b, v30.16b, v29.16b", "df 0f 3d 4e"),
        ("uaddl v0.8h, v1.8b, v2.8b", "20 00 22 2e"),
        ("uaddl2 v0.2d, v1.4s, v2.4s", "20 00 a2 6e"),
        ("saddw v3.4s, v4.4s, v5.4h", "83 10 65 0e"),
        ("pmull v0.8h, v1.8b, v2.8b", "20 e0 22 0e"),
        ("pmull2 v0.1q, v1.2d, v2.2d", "20 e0 e2 4e"),
        ("sqdmull v0.4s, v1.4h, v2.h[3]", "20 b0 72 0f"),
        ("mla v0.4s, v1.4s, v2.s[1]", "20 00 a2 6f"),
        ("fmla v0.2d, v1.2d, v2.d[1]", "20 18 c2 4f"),
        ("addp d0, v1.2d", "20 b8 f1 5e"),
        ("addv b0, v1.16b", "20 b8 31 4e"),
        ("fmaxnmp h0, v1.2h", "20 c8 30 5e"),
        ("saddlv h0, v1.16b", "20 38 30 4e"),
        ("cnt v0.8b, v1.8b", "20 58 20 0e"),
        ("rev64 v0.4s, v1.4s", "20 08 a0 4e"),
        ("not v0.16b, v1.16b", "20 58 20 6e"),
        ("mvn v0.8b, v1.8b", "20 58 20 2e"),
        ("bsl v0.16b, v1.16b, v2.16b", "20 1c 62 6e"),
        ("cmeq v0.2d, v1.2d, #0", "20 98 e0 4e"),
        ("cmle v0.8b, v1.8b, v2.8b", "40 3c 21 0e"),
        ("fcmlt v0.4s, v1.4s, #0.0", "20 e8 a0 4e"),
        ("abs d0, d1", "20 b8 e0 5e"),
        ("sqabs b0, b1", "20 78 20 5e"),
        ("urecpe v0.4s, v1.4s", "20 c8 a1 4e"),
        ("xtn v0.8b, v1.8h", "20 28 21 0e"),
        ("uqxtn2 v0.4s, v1.2d", "20 48 a1 6e"),
    ]);
}

/// Single lanes (`ins`, `umov`, `smov`, `dup`), table lookups, and the
/// structure loads and stores, with both register-list spellings.
#[test]
fn vector_elements_lists_and_structures() {
    check(&[
        ("ins v0.b[15], w1", "20 1c 1f 4e"),
        ("mov v0.d[1], x2", "40 1c 18 4e"),
        ("ins v0.s[1], v1.s[3]", "20 64 0c 6e"),
        ("umov w0, v1.b[7]", "20 3c 0f 0e"),
        ("umov x0, v1.d[1]", "20 3c 18 4e"),
        ("mov w0, v1.s[2]", "20 3c 14 0e"),
        ("smov x0, v1.h[3]", "20 2c 0e 4e"),
        ("dup v0.16b, v1.b[4]", "20 04 09 4e"),
        ("dup v0.8h, w3", "60 0c 02 4e"),
        ("mov b0, v1.b[3]", "20 04 07 5e"),
        ("ext v0.16b, v1.16b, v2.16b, #15", "20 78 02 6e"),
        ("tbl v0.8b, {v1.16b}, v2.8b", "20 00 02 0e"),
        (
            "tbx v0.16b, {v1.16b, v2.16b, v3.16b, v4.16b}, v5.16b",
            "20 70 05 4e",
        ),
        ("tbl v0.16b, {v31.16b, v0.16b}, v1.16b", "e0 23 01 4e"),
        ("zip1 v0.8b, v1.8b, v2.8b", "20 38 02 0e"),
        ("uzp2 v0.2d, v1.2d, v2.2d", "20 58 c2 4e"),
        ("trn1 v0.4s, v1.4s, v2.4s", "20 28 82 4e"),
        ("ld1 {v0.16b-v3.16b}, [x0], #64", "00 20 df 4c"),
        ("ld1 {v0.16b, v1.16b}, [x0], x2", "00 a0 c2 4c"),
        ("st4 {v1.b, v2.b, v3.b, v4.b}[5], [sp], x8", "e1 37 a8 0d"),
        ("ld2 {v0.4s, v1.4s}, [x1]", "20 88 40 4c"),
        ("ld1r {v3.8h}, [x4], #2", "83 c4 df 4d"),
        ("st1 {v7.2d}, [x0], #16", "07 7c 9f 4c"),
    ]);
}

/// Shifts by immediate, whose field holds a count up from or down from the
/// element width, and the fixed-point conversions that share them.
#[test]
fn shifts_count_from_the_element_width() {
    check(&[
        ("sshr v0.8b, v1.8b, #8", "20 04 08 0f"),
        ("ushr d0, d1, #64", "20 04 40 7f"),
        ("shl v0.4s, v1.4s, #31", "20 54 3f 4f"),
        ("sqshlu v0.2d, v1.2d, #0", "20 64 40 6f"),
        ("rshrn v0.8b, v1.8h, #1", "20 8c 0f 0f"),
        ("uqrshrn2 v0.4s, v1.2d, #32", "20 9c 20 6f"),
        ("sshll v0.2d, v1.2s, #31", "20 a4 3f 0f"),
        ("sxtl v0.8h, v1.8b", "20 a4 08 0f"),
        ("uxtl2 v0.2d, v1.4s", "20 a4 20 6f"),
        ("sri v0.16b, v1.16b, #8", "20 44 08 6f"),
        ("fcvtzs v0.4s, v1.4s, #32", "20 fc 20 4f"),
        ("ucvtf d0, d1, #64", "20 e4 40 7f"),
        ("scvtf s0, w1, #1", "20 fc 02 1e"),
        ("fcvtzu x0, d1, #64", "20 00 59 9e"),
    ]);
}

/// `movi` and friends with every shift and `msl` form, the byte-mask
/// immediate, and floating-point vector immediates.
#[test]
fn simd_immediates() {
    check(&[
        ("movi v0.16b, #0xff", "e0 e7 07 4f"),
        ("movi v0.4s, #0x12, lsl #24", "40 66 00 4f"),
        ("movi v0.8h, #0x12, lsl #8", "40 a6 00 4f"),
        ("movi v0.4s, #0xff, msl #16", "e0 d7 07 4f"),
        ("mvni v0.2s, #0x80, msl #8", "00 c4 04 2f"),
        ("orr v0.4h, #0xf0, lsl #8", "00 b6 07 0f"),
        ("bic v0.4s, #0x1", "20 14 00 6f"),
        ("movi v0.2d, #0xff00ff00ff00ff00", "40 e5 05 6f"),
        ("movi d0, #0xffffffffffffffff", "e0 e7 07 2f"),
        ("fmov v0.4s, #-2.5", "80 f4 04 4f"),
        ("fmov v0.2d, #0.125", "00 f4 02 6f"),
        ("fmov v0.4h, #31.0", "e0 ff 01 0f"),
    ]);
}

/// Scalar floating point: the 8-bit immediate, comparisons, conditional
/// select, rounding and conversions.
#[test]
fn scalar_floating_point() {
    check(&[
        ("fmov s0, #1.0", "00 10 2e 1e"),
        ("fmov d31, #-0.1875", "1f 10 79 1e"),
        ("fmov h0, #16", "00 10 e6 1e"),
        ("fmov d0, x1", "20 00 67 9e"),
        ("fmov x0, v1.d[1]", "20 00 ae 9e"),
        ("fmov v0.d[1], x1", "20 00 af 9e"),
        ("fmov s0, s1", "20 40 20 1e"),
        ("fcmp s0, #0.0", "08 20 20 1e"),
        ("fcmpe d0, d1", "10 20 61 1e"),
        ("fccmp h0, h1, #15, ne", "0f 14 e1 1e"),
        ("fccmpe s0, s1, #0, al", "10 e4 21 1e"),
        ("fcsel d0, d1, d2, lt", "20 bc 62 1e"),
        ("fabs h0, h1", "20 c0 e0 1e"),
        ("fsqrt d0, d1", "20 c0 61 1e"),
        ("frinta s0, s1", "20 40 26 1e"),
        ("frint32x d0, d1", "20 c0 68 1e"),
        ("frint64z s0, s1", "20 40 29 1e"),
        ("fcvt d0, s1", "20 c0 22 1e"),
        ("fcvt h0, d1", "20 c0 63 1e"),
        ("fmadd d0, d1, d2, d3", "20 0c 42 1f"),
        ("fnmul s0, s1, s2", "20 88 22 1e"),
        ("fjcvtzs w0, d1", "20 00 7e 1e"),
        ("fcvtns x0, h1", "20 00 e0 9e"),
        ("ucvtf h0, x1, #16", "20 c0 c3 9e"),
        ("fmax d0, d1, d2", "20 48 62 1e"),
    ]);
}

/// AES, SHA-1, SHA-2, SHA-3 and SM3/SM4.
#[test]
fn cryptographic_extensions() {
    check(&[
        ("aese v0.16b, v1.16b", "20 48 28 4e"),
        ("aesimc v0.16b, v1.16b", "20 78 28 4e"),
        ("sha1c q0, s1, v2.4s", "20 00 02 5e"),
        ("sha1p q2, s3, v4.4s", "62 10 04 5e"),
        ("sha1h s0, s1", "20 08 28 5e"),
        ("sha1su0 v0.4s, v1.4s, v2.4s", "20 30 02 5e"),
        ("sha256h2 q0, q1, v2.4s", "20 50 02 5e"),
        ("sha256su1 v0.4s, v1.4s, v2.4s", "20 60 02 5e"),
        ("sha512h q0, q1, v2.2d", "20 80 62 ce"),
        ("sha512su0 v0.2d, v1.2d", "20 80 c0 ce"),
        ("eor3 v0.16b, v1.16b, v2.16b, v3.16b", "20 0c 02 ce"),
        ("bcax v0.16b, v1.16b, v2.16b, v3.16b", "20 0c 22 ce"),
        ("rax1 v0.2d, v1.2d, v2.2d", "20 8c 62 ce"),
        ("xar v0.2d, v1.2d, v2.2d, #63", "20 fc 82 ce"),
        ("sm3ss1 v0.4s, v1.4s, v2.4s, v3.4s", "20 0c 42 ce"),
        ("sm3tt2b v0.4s, v1.4s, v2.s[3]", "20 bc 42 ce"),
        ("sm4e v0.4s, v1.4s", "20 84 c0 ce"),
        ("sm4ekey v0.4s, v1.4s, v2.4s", "20 c8 62 ce"),
    ]);
}

/// The dot-product, i8mm, FP16 multiply-long and BFloat16 extensions.
#[test]
fn dot_product_and_matrix_multiply() {
    check(&[
        ("sdot v0.4s, v1.16b, v2.16b", "20 94 82 4e"),
        ("udot v0.2s, v1.8b, v2.8b", "20 94 82 2e"),
        ("usdot v0.4s, v1.16b, v2.16b", "20 9c 82 4e"),
        ("sudot v0.4s, v1.16b, v2.4b[3]", "20 f8 22 4f"),
        ("smmla v0.4s, v1.16b, v2.16b", "20 a4 82 4e"),
        ("usmmla v0.4s, v1.16b, v2.16b", "20 ac 82 4e"),
        ("fmlal v0.4s, v1.4h, v2.4h", "20 ec 22 4e"),
        ("fmlsl2 v0.2s, v1.2h, v2.h[3]", "20 c0 b2 2f"),
        ("bfdot v0.4s, v1.8h, v2.8h", "20 fc 42 6e"),
        ("bfmmla v0.4s, v1.8h, v2.8h", "20 ec 42 6e"),
    ]);
}

/// SVE and SVE2 data processing: predicated and destructive forms, the
/// `dup`/`dupm` choice behind `mov`, element-width wrapping, patterns and
/// multipliers, compares and SVE2's cryptography.
#[test]
fn sve_data_processing() {
    check(&[
        ("add z0.b, p0/m, z0.b, z1.b", "20 00 00 04"),
        ("add z0.d, z1.d, z2.d", "20 00 e2 04"),
        ("add z3.h, z3.h, #255", "e3 df 60 25"),
        ("add z3.h, z3.h, #256", "23 e0 60 25"),
        ("sub z0.s, z0.s, #1, lsl #8", "20 e0 a1 25"),
        ("mov z0.d, z1.d", "20 30 61 04"),
        ("mov z0.s, p1/m, z2.s", "40 c4 a0 05"),
        ("mov z0.h, #-128", "00 d0 78 25"),
        ("mov z0.h, #0xfff0", "00 de 78 25"),
        ("mov z0.h, #0xff00", "e0 ff 78 25"),
        ("mov z0.h, #0x7ff0", "40 65 c0 05"),
        ("mov z31.d, #0xffffffff00000000", "ff 03 c3 05"),
        ("and z0.b, z0.b, #0xf0", "60 26 80 05"),
        ("eor z1.s, z1.s, #-2", "c1 fb 40 05"),
        ("dupm z0.h, #0x3f00", "a0 44 c0 05"),
        ("index z0.b, #-16, #15", "00 42 2f 04"),
        ("index z0.d, x1, #1", "20 44 e1 04"),
        ("ptrue p0.b", "e0 e3 18 25"),
        ("ptrue p1.d, vl8", "01 e1 d8 25"),
        ("ptrues p2.s, mul3", "c2 e3 99 25"),
        ("whilelo p0.s, x0, x1", "00 1c a1 25"),
        ("whilelt {p0.b, p1.b}, x0, x1", "10 54 21 25"),
        ("cntd x0", "e0 e3 e0 04"),
        ("cntb x1, pow2, mul #16", "01 e0 2f 04"),
        ("incd x0, all, mul #4", "e0 e3 f3 04"),
        ("sqdecw x0, w0, vl256", "a0 f9 a0 04"),
        ("uqincp x0, p1.b", "20 8c 29 25"),
        ("incd z0.d", "e0 c3 f0 04"),
        ("dech z1.h, vl3, mul #2", "61 c4 71 04"),
        ("fcmgt p0.h, p1/z, z2.h, z3.h", "50 44 43 65"),
        ("fcmle p0.d, p1/z, z2.d, #0.0", "50 24 d1 65"),
        ("fmul z0.d, p0/m, z0.d, #2.0", "20 80 da 65"),
        ("fcpy z0.h, p0/m, #1.5", "00 cf 50 05"),
        ("fmov z0.d, #0.125", "00 c8 f9 25"),
        ("asr z0.d, p0/m, z0.d, #64", "00 80 80 04"),
        ("lsr z0.b, z1.b, #1", "20 94 2f 04"),
        ("cmpeq p0.b, p1/z, z2.b, #-16", "40 84 10 25"),
        ("cmphi p0.d, p1/z, z2.d, #127", "50 c4 ff 24"),
        ("bdep z0.d, z1.d, z2.d", "20 b4 c2 45"),
        ("sm4e z0.s, z0.s, z1.s", "20 e0 23 45"),
        ("rax1 z0.d, z1.d, z2.d", "20 f4 22 45"),
        ("aesmc z0.b, z0.b", "00 e0 20 45"),
        ("ext z0.b, z0.b, z1.b, #255", "20 1c 3f 05"),
        ("ext z0.b, {z1.b, z2.b}, #0", "20 00 60 05"),
        ("fadda d0, p0, d0, z1.d", "20 20 d8 65"),
        ("clasta w0, p1, w0, z2.s", "40 a4 b0 05"),
        ("rev z0.h, z1.h", "20 38 78 05"),
        ("movs p0.b, p1.b", "20 44 c1 25"),
        ("not p0.b, p1/z, p2.b", "40 46 01 25"),
        ("brka p0.b, p1/z, p2.b", "40 44 10 25"),
        ("cdot z0.s, z1.b, z2.b, #270", "20 1c 82 44"),
        ("cmla z0.b, z1.b, z2.b, #90", "20 24 02 44"),
    ]);
}

/// SVE loads, stores, gathers and scatters in every addressing form,
/// with a lone register for a one-register list.
#[test]
fn sve_loads_stores_gathers_and_scatters() {
    check(&[
        ("ld1b z0.b, p0/z, [x0]", "00 a0 00 a4"),
        ("ld1d {z0.d}, p0/z, [x0, #7, mul vl]", "00 a0 e7 a5"),
        ("ld1d {z0.d}, p0/z, [x0, #-8, mul vl]", "00 a0 e8 a5"),
        ("ld1w {z1.s}, p2/z, [x3, x4, lsl #2]", "61 48 44 a5"),
        ("ldff1d {z0.d}, p0/z, [x1, z2.d, lsl #3]", "20 e0 e2 c5"),
        ("ld1sw {z0.d}, p0/z, [x1, z2.d, sxtw #2]", "20 00 62 c5"),
        ("ldff1b {z0.d}, p0/z, [z1.d, #31]", "20 e0 3f c4"),
        ("st1h {z0.s}, p0, [x1, z2.s, uxtw #1]", "20 80 e2 e4"),
        ("st1d {z0.d}, p0, [sp]", "e0 e3 e0 e5"),
        ("ld2b {z0.b, z1.b}, p0/z, [x0, #-8, mul vl]", "00 e0 2c a4"),
        (
            "st4w {z31.s, z0.s, z1.s, z2.s}, p1, [x0, x1, lsl #2]",
            "1f 64 61 e5",
        ),
        (
            "ld3h {z1.h - z3.h}, p4/z, [sp, #-24, mul vl]",
            "e1 f3 c8 a4",
        ),
        ("ldr z0, [x0, #-256, mul vl]", "00 40 a0 85"),
        ("str p15, [sp, #255, mul vl]", "ef 1f 9f e5"),
        ("prfb pldl1keep, p0, [x0, #-32, mul vl]", "00 00 e0 85"),
        ("prfd pstl3strm, p1, [x2, z3.d, sxtw #3]", "4d 64 63 c4"),
        ("adr z0.d, [z1.d, z2.d, lsl #3]", "20 ac e2 04"),
        ("adr z0.s, [z1.s, z2.s]", "20 a0 a2 04"),
    ]);
}

/// The SME instructions this backend has: its mode switches, and
/// clearing ZA.
#[test]
fn sme_mode_switches_and_zeroing() {
    check(&[
        ("smstart", "7f 47 03 d5"),
        ("smstart za", "7f 45 03 d5"),
        ("smstop sm", "7f 42 03 d5"),
        ("zero {za}", "ff 00 08 c0"),
    ]);
}

/// Both references read a PC-relative operand written as a number as the
/// offset from the instruction, not as an address.
#[test]
fn a_pc_relative_number_is_an_offset() {
    program(
        " nop\n nop\n b #16\n bl #-8\n b.eq #12\n cbz x0, #8\n tbz x1, #3, #20\n \
         ldr x0, #16\n ldr s10, #196\n adr x0, #20\n",
        "1f 20 03 d5 1f 20 03 d5 04 00 00 14 fe ff ff 97 60 00 00 54 40 00 00 b4 \
         a1 00 18 36 80 00 00 58 2a 06 00 1c a0 00 00 10",
    );
}

/// What the table refuses, it refuses with the operand and the limit, or
/// with the forms the mnemonic does take.
#[test]
fn simd_diagnostics_name_the_operand_and_its_limit() {
    rejects("sshr v0.8b, v1.8b, #9", &["operand 3", "1..=8", "9"]);
    rejects("ins v0.s[4], w0", &["operand 1 index", "0..=3"]);
    rejects("movi v0.4s, #0x12, lsl #7", &["one of 0, 8, 16, 24"]);
    rejects("ld1d {z0.d}, p0/z, [x0, #8, mul vl]", &["-8..=7"]);
    rejects(
        "fcmla v0.4s, v1.4s, v2.4s, #45",
        &["one of 0, 90, 180, 270"],
    );
    rejects("fmov d0, #0.3", &["not an 8-bit floating-point immediate"]);
    rejects("and z0.b, z0.b, #0x5a", &["not a valid logical immediate"]);
    rejects(
        "add z0.b, p0/m, z1.b, z2.b",
        &["operand 3 has to be the same register as operand 1"],
    );
    rejects(
        "sqadd v0.8b, v1.16b, v2.8b",
        &[
            "does not take `v0.8b, v1.16b, v2.8b`",
            "vN.16b, vN.16b, vN.16b",
        ],
    );
    rejects("tbl v0.16b, {v1.16b, v3.16b}, v2.16b", &["consecutive"]);
    rejects("ld1 {v0.16b, v1.8b}, [x0]", &["same kind"]);
    rejects("frobnicate v0.8b", &["unknown instruction `frobnicate`"]);
}

/// `dc`, `ic`, `at`, `tlbi` and the rest are names for a `sys` word; the
/// name decides whether an address register follows it. Every name comes
/// from GNU as's tables through `tools/tables/aarch64-sys.py`.
#[test]
fn system_instructions() {
    check(&[
        ("dc civac, x0", "20 7e 0b d5"),
        ("dc cvau, x3", "23 7b 0b d5"),
        ("dc zva, x30", "3e 74 0b d5"),
        ("dc isw, x1", "41 76 08 d5"),
        ("ic ialluis", "1f 71 08 d5"),
        ("ic iallu", "1f 75 08 d5"),
        ("ic ivau, x2", "22 75 0b d5"),
        ("at s1e0r, x0", "40 78 08 d5"),
        ("at s1e1w, x1", "21 78 08 d5"),
        ("at s12e1r, x4", "84 78 0c d5"),
        ("at s1e3a, x5", "45 79 0e d5"),
        // A TLB maintenance name that addresses nothing takes no register,
        // one that addresses a page needs one, and the names that maintain a
        // whole regime take one or not.
        ("tlbi alle3", "1f 87 0e d5"),
        ("tlbi vae1, x3", "23 87 08 d5"),
        ("tlbi ipas2e1is, x7", "27 80 0c d5"),
        ("tlbi vmalle1is", "1f 83 08 d5"),
        ("tlbi vmalle1is, x0", "00 83 08 d5"),
        // The nXS variants, which do not wait for the extended-set
        // translations, are separate names.
        ("tlbi rvae1nxs, x9", "29 96 08 d5"),
        ("tlbi vmalle1isnxs", "1f 93 08 d5"),
        ("cfp rctx, x0", "80 73 0b d5"),
        ("dvp rctx, x1", "a1 73 0b d5"),
        ("cpp rctx, x2", "e2 73 0b d5"),
        ("cosp rctx, x3", "c3 73 0b d5"),
        // The instruction all of those are aliases of. Its register is
        // `xzr` when it is left out.
        ("sys #0, c7, c8, #0, x5", "05 78 08 d5"),
        ("sys #3, c7, c4, #1", "3f 74 0b d5"),
        ("sys #7, c15, c15, #7, x30", "fe ff 0f d5"),
        ("sysl x7, #3, c7, c4, #1", "27 74 2b d5"),
        ("sysl x0, #0, c0, c0, #0", "00 00 28 d5"),
    ]);
}

/// `mrs` and `msr` know every register name GNU as has, reach the rest
/// through the generic spelling, and write a PSTATE field with an immediate
/// of the width that field has.
#[test]
fn system_registers_and_pstate_fields() {
    check(&[
        ("mrs x0, nzcv", "00 42 3b d5"),
        ("mrs x1, ctr_el0", "21 00 3b d5"),
        ("mrs x2, midr_el1", "02 00 38 d5"),
        ("mrs x3, sctlr_el1", "03 10 38 d5"),
        ("mrs x5, zcr_el1", "05 12 38 d5"),
        ("mrs x4, s3_3_c13_c0_3", "64 d0 3b d5"),
        ("msr nzcv, x0", "00 42 1b d5"),
        ("msr sctlr_el1, x1", "01 10 18 d5"),
        ("msr s3_0_c15_c1_0, x2", "02 f1 18 d5"),
        ("msr daifset, #3", "df 43 03 d5"),
        ("msr daifclr, #15", "ff 4f 03 d5"),
        ("msr spsel, #1", "bf 41 00 d5"),
        ("msr pan, #0", "9f 40 00 d5"),
        ("msr dit, #1", "5f 41 03 d5"),
        ("msr allint, #1", "1f 41 01 d5"),
        // SME's mode switches are PSTATE fields too: this is `smstart sm`.
        ("msr svcrsm, #1", "7f 43 03 d5"),
        ("msr svcrsmza, #1", "7f 47 03 d5"),
    ]);
}

/// The barriers and the aliases of `hint`, with a name or with the number
/// the name stands for.
#[test]
fn barriers_and_hints() {
    check(&[
        ("nop", "1f 20 03 d5"),
        ("yield", "3f 20 03 d5"),
        ("wfe", "5f 20 03 d5"),
        ("sevl", "bf 20 03 d5"),
        ("hint #7", "ff 20 03 d5"),
        ("xpaclri", "ff 20 03 d5"),
        ("hint #127", "ff 2f 03 d5"),
        ("esb", "1f 22 03 d5"),
        ("psb csync", "3f 22 03 d5"),
        ("tsb csync", "5f 22 03 d5"),
        ("gcsb dsync", "7f 22 03 d5"),
        ("csdb", "9f 22 03 d5"),
        ("clrbhb", "df 22 03 d5"),
        ("dgh", "df 20 03 d5"),
        ("sb", "ff 30 03 d5"),
        ("paciaz", "1f 23 03 d5"),
        ("paciasp", "3f 23 03 d5"),
        ("pacia1716", "1f 21 03 d5"),
        ("autibsp", "ff 23 03 d5"),
        ("bti", "1f 24 03 d5"),
        ("bti c", "5f 24 03 d5"),
        ("bti jc", "df 24 03 d5"),
        ("chkfeat x16", "1f 25 03 d5"),
        ("stshh keep", "1f 26 03 d5"),
        ("stshh strm", "3f 26 03 d5"),
        ("ssbb", "9f 30 03 d5"),
        ("pssbb", "9f 34 03 d5"),
        ("dsb sy", "9f 3f 03 d5"),
        ("dsb ishst", "9f 3a 03 d5"),
        ("dsb oshld", "9f 31 03 d5"),
        ("dsb #11", "9f 3b 03 d5"),
        // `dsb` reaches the nXS barriers through a fifth bit, which only
        // four values of set.
        ("dsb nshnxs", "3f 36 03 d5"),
        ("dsb synxs", "3f 3e 03 d5"),
        ("dsb #24", "3f 3a 03 d5"),
        ("dmb ish", "bf 3b 03 d5"),
        ("dmb ld", "bf 3d 03 d5"),
        ("dmb #0", "bf 30 03 d5"),
        ("isb", "df 3f 03 d5"),
        ("isb sy", "df 3f 03 d5"),
        ("isb #0", "df 30 03 d5"),
        ("clrex", "5f 3f 03 d5"),
        ("clrex #3", "5f 33 03 d5"),
    ]);
}

/// A literal pool is laid out across a whole section, so these are whole
/// programs. GNU as groups the entries by width, aligns each run, shares an
/// entry between loads of the same value and width, and writes what is left
/// at the end of the section. Unlike its ARM port — and unlike llvm-mc — it
/// never turns a load of a small constant into a `mov`.
#[test]
fn literal_pools() {
    program(
        " ldr x0, =0x123456789\n ldr w1, =0x12345678\n ldr x2, =0x123456789\n \
         ldr s3, =0x3fc00000\n ldrsw x4, =-1\n ret\n",
        "40 01 00 58 a1 00 00 18 02 01 00 58 83 00 00 1c 84 00 00 98 c0 03 5f d6 \
         78 56 34 12 00 00 c0 3f ff ff ff ff 00 00 00 00 89 67 45 23 01 00 00 00",
    );
    program(
        " ldr x0, =0x1111222233334444\n ldr w1, =0x55556666\n ret\n .ltorg\n \
         ldr x2, =0x7777888899990000\n ret\n",
        "80 00 00 58 41 00 00 18 c0 03 5f d6 66 66 55 55 44 44 33 33 22 22 11 11 \
         42 00 00 58 c0 03 5f d6 00 00 99 99 88 88 77 77",
    );
}

/// The system instructions refuse an unknown name, and the register the
/// name does not allow.
#[test]
fn system_diagnostics_name_what_was_expected() {
    rejects("dc nosuchop, x0", &["`nosuchop` is not an operand of `dc`"]);
    rejects("dc civac", &["`dc civac` needs an address register"]);
    rejects("ic ialluis, x0", &["`ic ialluis` takes no register"]);
    rejects("dc civac, w0", &["64-bit register"]);
    rejects("sys #8, c0, c0, #0", &["`op1`", "0..=7"]);
    rejects("sys #0, c16, c0, #0", &["`c0` through `c15`"]);
    rejects("sys #0, x0, c0, #0", &["`c0` through `c15`"]);
    rejects("mrs x0, nosuchreg", &["unknown system register"]);
    rejects("msr daifset, #16", &["a PSTATE value", "0..=15"]);
    rejects("msr spsel, #2", &["a PSTATE value", "0..=1"]);
    rejects("psb dsync", &["is not an operand of `psb`", "csync"]);
    rejects("esb x0", &["`esb` takes no operand"]);
    rejects("dsb #18", &["16, 20, 24 and 28"]);
    rejects(
        "ldr x0, =1\n .space 1024 * 1024\n .ltorg\n",
        &["literal pool"],
    );
    rejects("str x0, =1", &["only `ldr` and `ldrsw`"]);
    rejects("add x0, x1, =4", &["only `ldr` loads from"]);
}

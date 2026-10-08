//! V850 and RH850 encoding tests.
//!
//! Every expected byte string here was produced by GNU as 2.47 —
//! `v850-elf-as` for `v850`, and `v850-elf-as -mv850e3v5` for `rh850` — and
//! the same statements are in `tools/xas-diff/{v850,rh850}{,-programs}.txt`,
//! where `tools/xas-diff/run.sh v850 rh850` compares the two assemblers again.
//! Relocation types, offsets and addends were read from `v850-elf-readelf -r`
//! on the reference's objects. Nothing in this file is an encoding rsasm
//! invented for itself.

#![cfg(feature = "v850")]

mod common;
use common::*;

/// Asserts that each statement assembles on its own to the given bytes.
#[track_caller]
fn check(arch: &str, cases: &[(&str, &str)]) {
    for (src, want) in cases {
        let got = hex(&text_for(arch, src));
        assert_eq!(
            &got, want,
            "\n  arch: {arch}\nsource: {src}\n  want: {want}\n   got: {got}"
        );
    }
}

#[track_caller]
fn enc(arch: &str, src: &str, want: &str) {
    let got = hex(&text_for(arch, src));
    assert_eq!(
        got, want,
        "\n  arch: {arch}\nsource: {src}\n  want: {want}\n   got: {got}"
    );
}

/// Asserts the length of a program's code, and its first and last bytes.
/// For programs padded with `.space` to push branches out of range.
#[track_caller]
fn ends(arch: &str, src: &str, len: usize, head: &str, tail: &str) {
    let bytes = text_for(arch, src);
    assert_eq!(bytes.len(), len, "length of\n{src}");
    let h = head.split(' ').count();
    let t = tail.split(' ').count();
    assert_eq!(hex(&bytes[..h]), head, "head of\n{src}");
    assert_eq!(hex(&bytes[len - t..]), tail, "tail of\n{src}");
}

/// Asserts that `src` is refused and that the diagnostic says `what`.
#[track_caller]
fn refused(arch: &str, src: &str, what: &str) {
    let e = errors_for(arch, src);
    assert!(
        e.contains(what),
        "\n  arch: {arch}\nsource: {src}\nexpected a diagnostic containing {what:?}, got:\n{e}"
    );
}

/// The relocations of `src` as (offset, type, addend), in emission order.
fn relocs(arch: &str, src: &str) -> Vec<(u64, u32, i64)> {
    let asm = assemble_for(arch, src);
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

const R_V810_BYTE: u32 = 0x31;
const R_V810_HWORD: u32 = 0x32;
const R_V810_WORD: u32 = 0x33;
const R_V810_WLO: u32 = 0x34;
const R_V810_WHI: u32 = 0x35;
const R_V810_WHI1: u32 = 0x36;
const R_V850_PCR22: u32 = 0x46;
const R_V850_BLO: u32 = 0x47;
const R_V810_WLO_1: u32 = 0x4c;
const R_V850_PC17: u32 = 0x60;
const R_V850_WLO23: u32 = 0x71;

// ---- programs -----------------------------------------------------------------

#[test]
fn short_branches_both_ways() {
    let src = "start:\n bz start\n bnz 1f\n br 1f\n bsa 1f\n nop\n1: bge start\n blt 1b\n";
    let want = "82 05 ca 05 b5 05 ad 05 00 00 be fd f6 fd";
    enc("v850", src, want);
    enc("rh850", src, want);
}

#[test]
fn jumps_to_labels_use_22_bits() {
    enc(
        "v850",
        "top: jr top\n jarl top, lp\n jarl next, r10\n nop\nnext: jr top\n",
        "80 07 00 00 bf ff fc ff 80 57 06 00 00 00 bf 07 f2 ff",
    );
}

#[test]
fn v850_branches_relax_to_an_inverted_branch_over_jr() {
    ends(
        "v850",
        " bnz far\n bz far\n bgt far\n .space 0x200\nfar: nop\n",
        532,
        "b2 05 80 07 10 02 ba 05 80 07 0a 02 b7 05 80 07 04 02 00 00",
        "00 00",
    );
}

#[test]
fn v850_br_relaxes_to_jr_and_bsa_to_eight_bytes() {
    ends(
        "v850",
        " br far\n bsa far\n jbr far\n .space 0x200\nfar: nop\n",
        530,
        "80 07 10 02 ad 05 b5 05 80 07 08 02 80 07 04 02 00 00",
        "00 00",
    );
    ends(
        "v850",
        "back: nop\n .space 0x100\n bz back\n br back\n bsa back\n",
        276,
        "00 00",
        "ba 05 bf 07 fc fe bf 07 f8 fe ad 05 b5 05 bf 07 f0 fe",
    );
}

#[test]
fn the_short_branch_reaches_exactly_its_nine_bits() {
    // +0xfe forward and -0x100 back still fit.
    ends(
        "v850",
        " bz fwd\n .space 0xfa\nfwd: nop\nback: .space 0xfe\n bz back\n",
        510,
        "e2 7d 00 00",
        "00 00 92 85",
    );
    // Two more bytes each way do not.
    ends(
        "v850",
        " bz fwd\n .space 0xfc\nfwd: nop\nback: .space 0x100\n bz back\n",
        514,
        "f2 7d 00 00",
        "00 00 82 85",
    );
}

#[test]
fn growing_one_branch_can_push_another_out_of_range() {
    // The second branch reached `b` only while the first was two bytes.
    ends(
        "v850",
        " bz a\n bz b\n .space 0xf8\na: nop\nb: nop\n",
        256,
        "e2 7d e2 7d 00 00",
        "00 00",
    );
}

#[test]
fn rh850_branches_relax_to_17_bits_first() {
    ends(
        "rh850",
        " bnz far\n bsa far\n br far\n .space 0x200\nfar: nop\n",
        526,
        "ea 07 0d 02 ed 07 09 02 80 07 04 02 00 00",
        "00 00",
    );
    ends(
        "rh850",
        " bz vfar\n bsa vfar\n br vfar\n .space 0x10000\nvfar: nop\n",
        65556,
        "ba 05 81 07 10 00 ad 05 b5 05 81 07 08 00 81 07 04 00 00 00",
        "00 00",
    );
    ends(
        "rh850",
        "back: nop\n .space 0x100\n bz back\n bsa back\n .space 0x10000\n bz back\n bsa back\n br back\n",
        65820,
        "00 00",
        "ba 05 be 07 f4 fe ad 05 b5 05 be 07 ec fe be 07 e8 fe",
    );
}

#[test]
fn the_17_bit_branch_reaches_exactly_its_range() {
    ends(
        "rh850",
        " bz fwd\n .space 0xfffa\nfwd: nop\nback: .space 0x10000\n bz back\n",
        131076,
        "e2 07 ff ff 00 00",
        "00 00 f2 07 01 00",
    );
    ends(
        "rh850",
        " bz fwd\n .space 0xfffc\nfwd: nop\nback: .space 0x10002\n bz back\n",
        131084,
        "ba 05 81 07 00 00",
        "00 00 ba 05 be 07 fc ff",
    );
}

#[test]
fn branches_to_undefined_symbols_take_the_longest_form() {
    // Far enough from the section start that GNU as, which sizes these as if
    // the symbol were at address 0, agrees.
    ends(
        "v850",
        " .space 0x10000\n bz ext\n br ext\n bsa ext\n jr ext\n jarl ext, lp\n",
        65562,
        "00 00",
        "ba 05 80 07 00 00 80 07 00 00 ad 05 b5 05 80 07 00 00 80 07 00 00 80 ff 00 00",
    );
    ends(
        "rh850",
        " .space 0x20004\n bz ext\n bsa ext\n br ext\n jr ext\n jarl ext, lp\n loop r2, ext\n",
        131108,
        "00 00",
        "ba 05 80 07 00 00 ad 05 b5 05 80 07 00 00 80 07 00 00 80 07 00 00 80 ff 00 00 5f 12 ea 07 01 00",
    );
}

#[test]
fn loop_back_and_forward() {
    enc(
        "rh850",
        "top: add 1, r6\n loop r7, top\n mov 5, r8\n1: nop\n loop r8, 1b\n",
        "41 32 e7 06 03 00 05 42 00 00 e8 06 03 00",
    );
    // Forward is `add -1, r3` and a 17-bit `bne`.
    enc(
        "rh850",
        " loop r3, fwd\n nop\nfwd: nop\n",
        "5f 1a ea 07 07 00 00 00 00 00",
    );
    ends(
        "rh850",
        "top: nop\n .space 0xfffc\n loop r1, top\n",
        65538,
        "00 00",
        "00 00 e1 06 ff ff",
    );
}

#[test]
fn loop_out_of_reach_backwards_is_refused() {
    // GNU as emits a 17-bit `bne` whose displacement has silently wrapped.
    refused(
        "rh850",
        "top2: nop\n .space 0xfffe\n loop r1, top2\n",
        "out of range",
    );
}

#[test]
fn hi_carries_bit_15_of_the_low_half() {
    enc(
        "v850",
        " .set addr, 0x12348000\n movhi hi(addr), r0, r1\n movea lo(addr), r1, r1\n\
          .set addr2, 0x12347fff\n movhi hi(addr2), r0, r1\n movea lo(addr2), r1, r1\n\
          .set addr3, 0xffff8000\n movhi hi(addr3), r0, r1\n movea lo(addr3), r1, r1\n",
        "40 0e 35 12 21 0e 00 80 40 0e 34 12 21 0e ff 7f 40 0e 00 00 21 0e 00 80",
    );
}

#[test]
fn label_differences_resolve_into_fields() {
    enc(
        "v850",
        "start: movea end - start, r0, r1\n addi end - start, r1, r1\n ld.w (end - start)[r1], r2\n nop\nend:\n",
        "20 0e 0e 00 01 0e 0e 00 21 17 0f 00 00 00",
    );
    enc(
        "rh850",
        "start: movea end - start, r0, r1\n mov hilo(end - start), r2\n ld.bu (end - start)[r1], r2\n ld.w (end - start)[r1], r2\n nop\nend:\n",
        "20 0e 14 00 22 06 14 00 00 00 81 17 15 00 21 17 15 00 00 00",
    );
}

#[test]
fn data_widths_and_comments() {
    enc(
        "v850",
        " .byte 1, 2\n .short 0x1234\n .word 0x12345678\n .long 0x9abcdef0\n",
        "01 02 34 12 78 56 34 12 f0 de bc 9a",
    );
    enc(
        "v850",
        " mov r1, r2    # a comment\n mov r3, r4; mov r5, r6\n# a whole-line comment\n nop\n",
        "01 10 03 20 05 30 00 00",
    );
}

#[test]
fn alignment_in_code_pads_with_nop() {
    enc(
        "v850",
        " mov r1, r2\n halt\n .p2align 3\n nop\n movea 1, r0, r1\n nop\n",
        "01 10 e0 07 20 01 00 00 00 00 20 0e 01 00 00 00",
    );
}

#[test]
fn small_programs() {
    enc(
        "v850",
        "handler:\n st.w r1, -4[sp]\n add -4, sp\n ldsr r1, eipc\n shl 2, r1\n ld.w 0[sp], r1\n add 4, sp\n reti\n",
        "63 0f fd ff 5c 1a e1 07 20 00 c2 0a 23 0f 01 00 44 1a e0 07 40 01",
    );
    enc(
        "v850",
        " movhi hi(0x12340000), r0, r6\n movea lo(0x12340000), r6, r6\n movea 16, r0, r7\n\
         1: sld.b 0[ep], r8\n st.b r8, 0[r6]\n add 1, r6\n add -1, r7\n bnz 1b\n jmp [lp]\n",
        "40 36 34 12 26 36 00 00 20 3e 10 00 00 43 46 47 00 00 41 32 5f 3a ba fd 7f 00",
    );
    enc(
        "rh850",
        "func:\n prepare {r20-r22, r29, lp}, 2\n mov r6, r20\n movea 0x100, r0, r21\n jarl callee, lp\n\
          mov r10, r22\n add r20, r10\n dispose 2, {r20-r22, r29, lp}, [lp]\n\
         callee:\n prepare {lp}, 0, sp\n dispose 0, {lp}\n jmp [lp]\n",
        "84 07 61 0e 06 a0 20 ae 00 01 80 ff 0c 00 0a b0 d4 51 44 06 7f 0e 80 07 23 00 40 06 20 00 7f 00",
    );
    enc(
        "rh850",
        " mov 0, r10\n bins r6, 8, 8, r10\n rotl 4, r10, r11\n satadd r11, r10, r12\n cmov lt, r12, r10, r13\n sch1l r13, r14\n jmp [lp]\n",
        "00 52 e6 57 d0 f8 e4 57 c4 58 eb 57 ba 63 ec 57 2c 6b e0 6f 66 73 7f 00",
    );
    enc(
        "rh850",
        " cmpf.s olt, r6, r7\n trfsr\n cmovf.s r6, r7, r10\n addf.d r6, r8, r10\n cvtf.ds r10, r11\n jmp [lp]\n",
        "e7 37 20 24 e0 07 00 04 e6 3f 00 54 e6 47 70 54 e3 57 52 5c 7f 00",
    );
}

#[test]
fn directives_switch_the_instruction_set() {
    enc(
        "v850",
        " mov r1, r2\n .v850e3v5\n callt 3\n mov 16, r1\n .v850\n mov 5, r1\n",
        "01 10 03 02 21 06 10 00 00 00 05 0a",
    );
    refused("v850", " .v850e3v5\n .v850\n callt 3\n", "RH850");
    refused("v850", " .v850e2v3\n", "not supported");
}

// ---- relocations ------------------------------------------------------------------

#[test]
fn relocations_match_gnu_as() {
    let src = " movhi hi(foo), r0, r1\n movea lo(foo), r1, r1\n movhi hi0(foo+4), r0, r1\n\
                movhi hi(foo+0x8000), r0, r1\n mov hilo(foo), r2\n ld.bu lo(foo)[r1], r2\n\
                ld.w lo(foo)[r1], r2\n ld.b lo23(foo)[r1], r2\n st.dw r2, foo[r1]\n\
                prepare {}, 0, hilo(foo)\n prepare {}, 0, hi(foo)\n prepare {}, 0, lo(foo)\n\
                jarl foo, lp\n jr foo+6\n";
    assert_eq!(
        relocs("rh850", src),
        vec![
            (0x02, R_V810_WHI1, 0),
            (0x06, R_V810_WLO, 0),
            (0x0a, R_V810_WHI, 4),
            (0x0e, R_V810_WHI1, 0x8000),
            (0x12, R_V810_WORD, 0),
            (0x16, R_V850_BLO, 0),
            (0x1c, R_V810_WLO_1, 0),
            (0x20, R_V850_WLO23, 0),
            (0x26, R_V850_WLO23, 0),
            (0x2e, R_V810_WORD, 0),
            (0x36, R_V810_WHI1, 0),
            (0x3c, R_V810_WLO, 0),
            (0x3e, R_V850_PCR22, 0),
            (0x42, R_V850_PCR22, 6),
        ]
    );
}

#[test]
fn pc_relative_relocations_have_no_bias_in_the_addend() {
    // The RH850 ABI measures branches from the start of the instruction, and
    // GNU as writes addend 0 for all of these.
    let src = " .space 0x20000\n bz ext\n br ext\n bsa ext\n loop r1, ext\n jarl ext, lp\n";
    assert_eq!(
        relocs("rh850", src),
        vec![
            (0x20002, R_V850_PCR22, 0),
            (0x20006, R_V850_PCR22, 0),
            (0x2000e, R_V850_PCR22, 0),
            (0x20014, R_V850_PC17, 0),
            (0x20018, R_V850_PCR22, 0),
        ]
    );
}

#[test]
fn data_relocations() {
    let asm = assemble_for(
        "rh850",
        " .data\nd: .long foo\n .short foo\n .byte foo\n .word d\n",
    );
    let got: Vec<(u64, u32, i64)> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.addend))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, R_V810_WORD, 0),
            (4, R_V810_HWORD, 0),
            (6, R_V810_BYTE, 0),
            (7, R_V810_WORD, 0),
        ]
    );
    // GNU as silently drops the `- .` here; rsasm refuses.
    refused("rh850", " .short foo - .\n", "cannot be relocated");
}

#[test]
fn a_local_label_is_relocated_against_its_section() {
    let src =
        " movhi hi(data), r0, r1\n movea lo(data), r1, r1\n .data\n .space 8\ndata: .long 1\n";
    assert_eq!(
        relocs("v850", src),
        vec![(2, R_V810_WHI1, 8), (6, R_V810_WLO, 8)]
    );
}

#[test]
fn objects_are_em_v800_little_endian_rela() {
    for arch in ["v850", "rh850"] {
        let a = rsasm::arch::lookup(arch).expect("backend");
        // GNU as marks both as EM_V800, "Renesas V850 (using RH850 ABI)".
        assert_eq!(a.elf_machine(), 36);
        assert!(rsasm::output::elf::uses_rela(a.elf_machine(), false));
        let asm = assemble_for(arch, " movhi hi(foo), r0, r1\n");
        let elf = rsasm::output::elf::build(&asm).expect("ELF output");
        assert_eq!(u16::from_le_bytes([elf[18], elf[19]]), 36);
        assert_eq!(elf[5], 1, "little-endian");
    }
}

// ---- diagnostics --------------------------------------------------------------------

#[test]
fn v850_refuses_rh850_instructions_by_name() {
    for src in [
        "callt 3",
        "prepare {r20}, 0",
        "loop r1, 2",
        "bsh r1, r2",
        "pushsp r20-r25",
        "addf.s r1, r2, r3",
    ] {
        refused("v850", src, "RH850");
    }
    // And forms of shared instructions that only RH850 has.
    for src in [
        "mov 16, r1",
        "shl r1, r2, r3",
        "ld.b 0x8000[r1], r2",
        "jr 0x200000",
        "ldsr r1, ctbp",
        "set1 r1, [r2]",
    ] {
        refused("v850", src, "needs the RH850 instruction set");
    }
}

#[test]
fn range_errors_name_their_limits() {
    refused("v850", "add 32, r1", "(-16 to 31)");
    refused("v850", "movea 0x10000, r0, r1", "(-32768 to 65535)");
    refused("v850", "bz 0x100", "(-256 to 254)");
    refused("v850", "br 0x100", "(-256 to 254)");
    refused("v850", "ld.b 0x8000[r1], r2", "RH850");
    refused("rh850", "loop r1, 0x10000", "(0 to 65534)");
    refused("rh850", "st.b r1, 0x400000[r2]", "(-4194304 to 4194303)");
    refused("rh850", "mul 256, r1, r2", "(-256 to 255)");
    refused("rh850", "prepare {}, 32", "(0 to 31)");
    refused("rh850", "bins r1, 20, 13, r2", "past bit 31");
    refused("rh850", "fetrap 0", "(1 to 15)");
    refused("rh850", "mov 0x100000000, r1", "4294967295");
}

#[test]
fn alignment_errors_name_the_multiple() {
    refused("v850", "sld.h 7[ep], r1", "not a multiple of 2");
    refused("v850", "sld.w 6[ep], r1", "not a multiple of 4");
    refused("v850", "bz 3", "not a multiple of 2");
    refused("v850", "ld.w 9[r1], r2", "not a multiple of 2");
    refused("rh850", "ld.dw 7[r1], r2", "not a multiple of 2");
    refused("rh850", "jmp 3[r1]", "not a multiple of 2");
}

#[test]
fn operand_mistakes_are_explained() {
    refused("v850", "mov r1, r0", "r0 cannot be used here");
    refused("v850", "sld.b 5[r1], r1", "relative to ep");
    refused("v850", "setf foo, r1", "unknown condition `foo`");
    refused("v850", "movea r1+4, r0, r1", "after register");
    refused("v850", "mov r1, r2, r3", "takes 2");
    refused("v850", "mov r1", "needs 2 operand");
    refused("v850", "bogus r1", "unknown instruction `bogus`");
    refused("rh850", "mov foo, r1", "hilo(sym)");
    refused("rh850", "movea hilo(foo), r0, r1", "32-bit field");
    refused("rh850", "movea sdaoff(foo), gp, r1", "RH850 ABI");
    refused("rh850", "callt ctoff(foo)", "callt");
    refused("rh850", "adf sa, r1, r2, r3", "`sa`");
    refused("rh850", "addf.d r1, r2, r3", "even register");
    refused("rh850", "prepare {r1}, 0", "only r20-r31");
    refused("rh850", "prepare {r22-r20}, 0", "backwards");
    refused("rh850", "cache bogus, [r1]", "unknown cache operation");
    refused("rh850", "ldtc.vr r1, r5", "vr0");
}

#[test]
fn malformed_input_never_panics() {
    const BAD: &[&str] = &[
        "",
        ",",
        "mov",
        "mov ,",
        "mov , ,",
        "mov r1,",
        "mov ,r1",
        "mov r1 r2",
        "mov [",
        "mov [r1",
        "mov [r1]]",
        "mov []",
        "mov [5]",
        "mov 5[",
        "mov 5[r1",
        "mov 5[r1]]",
        "mov {",
        "mov {}",
        "mov }",
        "mov r1-r2",
        "mov -r1, r2",
        "mov (r1), r2",
        "mov hi(, r1",
        "mov hi(), r1",
        "mov hi(r1), r2",
        "movea lo(",
        "movea lo(foo",
        "movea lo(foo))",
        "movea lo lo, r1, r2",
        "prepare",
        "prepare {",
        "prepare {r20",
        "prepare {r20-",
        "prepare {r20-}",
        "prepare {-r20}",
        "prepare {r20,,r21}, 0",
        "prepare {,}, 0",
        "prepare {r20}}, 0",
        "prepare {r20} x, 0",
        "prepare {5}, 0",
        "prepare 0x1000, 0",
        "prepare -1, 0",
        "prepare {}, 0, sp, 1",
        "dispose",
        "dispose 0",
        "dispose 0, {}, [r0]",
        "pushsp",
        "pushsp r1-",
        "pushsp -r1",
        "pushsp r1-r2-r3",
        "bins",
        "bins r1, 40, 1, r2",
        "bins r1, 1, -1, r2",
        "ldsr r1, psw, 99",
        "ldsr",
        "stsr ,",
        "setf , r1",
        "cmov , , , ",
        "bz",
        "bz ,",
        "bz r1",
        "bz [r1]",
        "bz lo(x)",
        "bz 1, 2",
        "loop",
        "loop r1",
        "loop 5, r1",
        "loop r1, lo(x)",
        "jmp",
        "jmp 5",
        "jmp [r1]x",
        "jarl",
        "jarl 1",
        "jr r1",
        "ld.b",
        "ld.b [r1], r2",
        "ld.b 5[sp]",
        "st.w r1",
        "st.w r1, r2",
        "sld.b 5, r1",
        "set1 99, 0[r1]",
        "cmpf.s",
        "cmpf.s lt",
        "trfsr 8",
        "rie 99, 99",
        "mov 99999999999999999999, r1",
        "mov 1/0, r1",
        "movea 1%0, r0, r1",
        "mov 0x, r1",
        "movea lo(0x8000000000000000), r0, r1",
        "movhi hi(-9223372036854775808), r0, r1",
        "mov -9223372036854775808, r1",
        "jr -9223372036854775808",
        "bz -9223372036854775808",
        "loop r1, -9223372036854775808",
        "ld.w -9223372036854775808[r1], r2",
        "prepare {}, 0, -9223372036854775808",
        "jmp -9223372036854775808[r1]",
        "sld.w -9223372036854775808[ep], r1",
        "mul -9223372036854775808, r1, r2",
        "ldsr r1, 9223372036854775807",
        "bins r1, 9223372036854775807, 2, r2",
        ".v850e",
        "r1: mov r1, r2",
        "mov r1, r2 # ok\n bz",
        "\u{0}",
        "mov\u{0}r1",
    ];
    for arch in ["v850", "rh850"] {
        for src in BAD {
            // Diagnostics or success are both fine; a panic is not.
            let _ = try_text_for(arch, src);
        }
    }
}

// ---- single statements, one table per corpus section -------------------------

/// Registers and their ABI names.
#[test]
fn v850_registers_and_their_abi_names() {
    check(
        "v850",
        &[
            ("mov r1, r2", "01 10"),
            ("mov R1, R2", "01 10"),
            ("mov r31, r30", "1f f0"),
            ("mov zero, hp", "00 10"),
            ("mov sp, gp", "03 20"),
            ("mov tp, ep", "05 f0"),
            ("mov lp, r31", "1f f8"),
            ("mov r0, r1", "00 08"),
        ],
    );
}

/// Format I: register to register.
#[test]
fn v850_format_i_register_to_register() {
    check(
        "v850",
        &[
            ("add r1, r2", "c1 11"),
            ("sub r1, r2", "a1 11"),
            ("cmp r1, r2", "e1 11"),
            ("and r3, r4", "43 21"),
            ("or r3, r4", "03 21"),
            ("xor r3, r4", "23 21"),
            ("not r3, r4", "23 20"),
            ("tst r3, r4", "63 21"),
            ("subr r3, r4", "83 21"),
            ("mulh r1, r2", "e1 10"),
            ("divh r1, r2", "41 10"),
            ("satadd r1, r2", "c1 10"),
            ("satsub r1, r2", "a1 10"),
            ("satsubr r1, r2", "81 10"),
            ("jmp [r1]", "61 00"),
            ("jmp r1", "61 00"),
            ("jmp [lp]", "7f 00"),
            ("nop", "00 00"),
            ("breakpoint", "01 00"),
        ],
    );
}

/// Format II: 5-bit immediates, and the range GNU as accepts for them.
#[test]
fn v850_format_ii_5_bit_immediates_and_the_range_gnu_as_accepts_for_() {
    check(
        "v850",
        &[
            ("mov 5, r2", "05 12"),
            ("mov -16, r10", "10 52"),
            ("mov 15, r10", "0f 52"),
            ("add 3, r2", "43 12"),
            ("add -16, r2", "50 12"),
            ("add 16, r2", "50 12"),
            ("add 31, r2", "5f 12"),
            ("cmp -1, r2", "7f 12"),
            ("cmp 15, r31", "6f fa"),
            ("satadd 5, r2", "25 12"),
            ("satadd -16, r2", "30 12"),
            ("mulh 5, r2", "e5 12"),
            ("mulh -1, r2", "ff 12"),
            ("shl 5, r6", "c5 32"),
            ("shr 31, r6", "9f 32"),
            ("sar 0, r6", "a0 32"),
            ("sar 17, r29", "b1 ea"),
        ],
    );
}

/// Format IV: short loads and stores relative to ep.
#[test]
fn v850_format_iv_short_loads_and_stores_relative_to_ep() {
    check(
        "v850",
        &[
            ("sld.b 5[ep], r1", "05 0b"),
            ("sld.b 127[ep], r1", "7f 0b"),
            ("sld.b -1[ep], r1", "7f 0b"),
            ("sld.b 5[r30], r1", "05 0b"),
            ("sld.h 6[ep], r2", "03 14"),
            ("sld.h 254[ep], r2", "7f 14"),
            ("sld.w 8[ep], r2", "04 15"),
            ("sld.w 252[ep], r2", "7e 15"),
            ("sst.b r1, 5[ep]", "85 0b"),
            ("sst.h r1, 6[ep]", "83 0c"),
            ("sst.w r1, 8[ep]", "05 0d"),
            ("sst.w r31, 0[ep]", "01 fd"),
        ],
    );
}

/// Format VI: 16-bit immediates.
#[test]
fn v850_format_vi_16_bit_immediates() {
    check(
        "v850",
        &[
            ("movea 5, r1, r2", "21 16 05 00"),
            ("movea -1, r1, r2", "21 16 ff ff"),
            ("movea 0xffff, r1, r2", "21 16 ff ff"),
            ("movea 0x8000, r1, r2", "21 16 00 80"),
            ("movea -32768, r1, r2", "21 16 00 80"),
            ("movhi 0x1234, r0, r1", "40 0e 34 12"),
            ("addi 100, r1, r2", "01 16 64 00"),
            ("addi -100, r0, r31", "00 fe 9c ff"),
            ("andi 0xffff, r1, r2", "c1 16 ff ff"),
            ("andi -1, r1, r2", "c1 16 ff ff"),
            ("ori 0x8000, r1, r2", "81 16 00 80"),
            ("xori 1, r1, r2", "a1 16 01 00"),
            ("mulhi 3, r1, r2", "e1 16 03 00"),
            ("satsubi 5, r1, r2", "61 16 05 00"),
        ],
    );
}

/// Hi(), lo() and hi0() on constants; hi() carries bit 15 of the low half.
#[test]
fn v850_hi_lo_and_hi0_on_constants_hi_carries_bit_15_of_the_low_half() {
    check(
        "v850",
        &[
            ("movhi hi(0x12345678), r0, r1", "40 0e 34 12"),
            ("movhi hi(0x12348000), r0, r2", "40 16 35 12"),
            ("movhi hi(0x12347fff), r0, r2", "40 16 34 12"),
            ("movhi hi(0x1234ffff), r0, r2", "40 16 35 12"),
            ("movhi hi0(0x12348000), r0, r2", "40 16 34 12"),
            ("movhi hi(-1), r0, r2", "40 16 00 00"),
            ("movhi hi(0x7fff8000), r0, r2", "40 16 00 80"),
            ("movea lo(0x12348000), r1, r2", "21 16 00 80"),
            ("movea lo(0x12347fff), r1, r2", "21 16 ff 7f"),
            ("movea lo(-1), r1, r2", "21 16 ff ff"),
            ("movea hi(0x12348000), r1, r2", "21 16 35 12"),
            ("movea zdaoff(0xfffff006), r0, r1", "20 0e 06 f0"),
        ],
    );
}

/// Format VII: 16-bit displacements.
#[test]
fn v850_format_vii_16_bit_displacements() {
    check(
        "v850",
        &[
            ("ld.b 5[r1], r2", "01 17 05 00"),
            ("ld.b -5[r1], r2", "01 17 fb ff"),
            ("ld.b 0x7fff[r1], r2", "01 17 ff 7f"),
            ("ld.b -0x8000[r1], r2", "01 17 00 80"),
            ("ld.h 6[r1], r2", "21 17 06 00"),
            ("ld.w 8[r1], r2", "21 17 09 00"),
            ("ld.w -8[sp], lp", "23 ff f9 ff"),
            ("st.b r2, 5[r1]", "41 17 05 00"),
            ("st.h r2, 6[r1]", "61 17 06 00"),
            ("st.w r2, 8[r1]", "61 17 09 00"),
            ("st.w lp, 0[sp]", "63 ff 01 00"),
            ("ld.b 5 [ r1 ] , r2", "01 17 05 00"),
            ("ld.b lo(0x12348000)[r1], r2", "01 17 00 80"),
            ("ld.w lo(0x12348002)[r1], r2", "21 17 03 80"),
        ],
    );
}

/// Format V: jumps with a numeric displacement.
#[test]
fn v850_format_v_jumps_with_a_numeric_displacement() {
    check(
        "v850",
        &[
            ("jarl 0, r31", "80 ff 00 00"),
            ("jarl 2, lp", "80 ff 02 00"),
            ("jarl -2, r1", "bf 0f fe ff"),
            ("jarl 0x1ffffe, r1", "9f 0f fe ff"),
            ("jarl -0x200000, r1", "a0 0f 00 00"),
            ("jr 100", "80 07 64 00"),
            ("jr -100", "bf 07 9c ff"),
        ],
    );
}

/// Format III: conditional branches with a numeric displacement.
#[test]
fn v850_format_iii_conditional_branches_with_a_numeric_displacement() {
    check(
        "v850",
        &[
            ("bz 8", "c2 05"),
            ("bz -2", "f2 fd"),
            ("bz 0xfe", "f2 7d"),
            ("bz -0x100", "82 85"),
            ("br 6", "b5 05"),
            ("bsa 4", "ad 05"),
            ("bgt 6", "bf 05"),
            ("bge 6", "be 05"),
            ("blt 6", "b6 05"),
            ("ble 6", "b7 05"),
            ("bh 6", "bb 05"),
            ("bnh 6", "b3 05"),
            ("bl 6", "b1 05"),
            ("bnl 6", "b9 05"),
            ("be 6", "b2 05"),
            ("bne 6", "ba 05"),
            ("bv 6", "b0 05"),
            ("bnv 6", "b8 05"),
            ("bn 6", "b4 05"),
            ("bp 6", "bc 05"),
            ("bc 6", "b1 05"),
            ("bnc 6", "b9 05"),
            ("bz 6", "b2 05"),
            ("bnz 6", "ba 05"),
            ("bt 6", "b2 05"),
            ("bf 6", "ba 05"),
            ("bsa 6", "bd 05"),
            ("jbr 6", "b5 05"),
            ("jgt 6", "bf 05"),
            ("jge 6", "be 05"),
            ("jlt 6", "b6 05"),
            ("jle 6", "b7 05"),
            ("jh 6", "bb 05"),
            ("jnh 6", "b3 05"),
            ("jl 6", "b1 05"),
            ("jnl 6", "b9 05"),
            ("je 6", "b2 05"),
            ("jne 6", "ba 05"),
            ("jv 6", "b0 05"),
            ("jnv 6", "b8 05"),
            ("jn 6", "b4 05"),
            ("jp 6", "bc 05"),
            ("jc 6", "b1 05"),
            ("jnc 6", "b9 05"),
            ("jz 6", "b2 05"),
            ("jnz 6", "ba 05"),
        ],
    );
}

/// Format VIII: bit manipulation.
#[test]
fn v850_format_viii_bit_manipulation() {
    check(
        "v850",
        &[
            ("set1 3, 5[r1]", "c1 1f 05 00"),
            ("clr1 3, 5[r1]", "c1 9f 05 00"),
            ("not1 3, 5[r1]", "c1 5f 05 00"),
            ("tst1 7, -1[r1]", "c1 ff ff ff"),
            ("set1 7, 0xffff[r1]", "c1 3f ff ff"),
            ("set1 0, -32768[r31]", "df 07 00 80"),
        ],
    );
}

/// Format IX/X: shifts by register, conditions, system registers, control.
#[test]
fn v850_format_ix_x_shifts_by_register_conditions_system_registers_c() {
    check(
        "v850",
        &[
            ("shl r1, r2", "e1 17 c0 00"),
            ("shr r1, r2", "e1 17 80 00"),
            ("sar r1, r2", "e1 17 a0 00"),
            ("setf z, r1", "e2 0f 00 00"),
            ("setf nz, r1", "ea 0f 00 00"),
            ("setf sa, r1", "ed 0f 00 00"),
            ("setf Z, r1", "e2 0f 00 00"),
            ("setf 5, r1", "e5 0f 00 00"),
            ("setf ge, r1", "ee 0f 00 00"),
            ("setf v, r31", "e0 ff 00 00"),
            ("setf nh, r2", "e3 17 00 00"),
            ("ldsr r1, psw", "e1 2f 20 00"),
            ("ldsr r1, 5", "e1 2f 20 00"),
            ("ldsr r1, eipc", "e1 07 20 00"),
            ("ldsr r1, sr31", "e1 ff 20 00"),
            ("stsr psw, r1", "e5 0f 40 00"),
            ("stsr eipc, r1", "e0 0f 40 00"),
            ("stsr 5, r1", "e5 0f 40 00"),
            ("stsr fepsw, r31", "e3 ff 40 00"),
            ("trap 0", "e0 07 00 01"),
            ("trap 31", "ff 07 00 01"),
            ("reti", "e0 07 40 01"),
            ("halt", "e0 07 20 01"),
            ("di", "e0 07 60 01"),
            ("ei", "e0 87 60 01"),
        ],
    );
}

/// The base set, spot-checked under -mv850e3v5.
#[test]
fn rh850_the_base_set_spot_checked_under_mv850e3v5() {
    check(
        "rh850",
        &[
            ("mov r1, r2", "01 10"),
            ("add -16, r2", "50 12"),
            ("movhi hi(0x12348000), r0, r2", "40 16 35 12"),
            ("ld.w 8[r1], r2", "21 17 09 00"),
            ("jarl 2, lp", "80 ff 02 00"),
            ("ldsr r1, psw", "e1 2f 20 00"),
            ("stsr psw, r1", "e5 0f 40 00"),
            ("bz -2", "f2 fd"),
        ],
    );
}

/// 48-bit mov, chosen when the value misses the 5-bit form.
#[test]
fn rh850_48_bit_mov_chosen_when_the_value_misses_the_5_bit_form() {
    check(
        "rh850",
        &[
            ("mov 16, r10", "2a 06 10 00 00 00"),
            ("mov -17, r10", "2a 06 ef ff ff ff"),
            ("mov 0x12345678, r10", "2a 06 78 56 34 12"),
            ("mov -0x80000000, r1", "21 06 00 00 00 80"),
            ("mov 0xffffffff, r1", "21 06 ff ff ff ff"),
            ("mov 5, r0", "20 06 05 00 00 00"),
            ("mov hilo(0x12345678), r1", "21 06 78 56 34 12"),
            ("mov hilo(5), r1", "21 06 05 00 00 00"),
        ],
    );
}

/// Three-operand shifts and rotates, saturating arithmetic.
#[test]
fn rh850_three_operand_shifts_and_rotates_saturating_arithmetic() {
    check(
        "rh850",
        &[
            ("shl r1, r2, r3", "e1 17 c2 18"),
            ("shr r1, r2, r3", "e1 17 82 18"),
            ("sar r1, r2, r3", "e1 17 a2 18"),
            ("rotl r1, r2, r3", "e1 17 c6 18"),
            ("rotl 5, r2, r3", "e5 17 c4 18"),
            ("rotl 31, r31, r1", "ff ff c4 08"),
            ("satadd r1, r2, r3", "e1 17 ba 1b"),
            ("satsub r1, r2, r3", "e1 17 9a 1b"),
            ("adf z, r1, r2, r3", "e1 17 a4 1b"),
            ("sbf nz, r1, r2, r3", "e1 17 94 1b"),
        ],
    );
}

/// Multiply and divide.
#[test]
fn rh850_multiply_and_divide() {
    check(
        "rh850",
        &[
            ("mul r1, r2, r3", "e1 17 20 1a"),
            ("mul 5, r2, r3", "e5 17 40 1a"),
            ("mul -256, r2, r3", "e0 17 60 1a"),
            ("mul 255, r2, r3", "ff 17 5c 1a"),
            ("mulu r1, r2, r3", "e1 17 22 1a"),
            ("mulu 511, r2, r3", "ff 17 7e 1a"),
            ("div r1, r2, r3", "e1 17 c0 1a"),
            ("divu r1, r2, r3", "e1 17 c2 1a"),
            ("divh r1, r2, r3", "e1 17 80 1a"),
            ("divhu r1, r2, r3", "e1 17 82 1a"),
            ("divq r1, r2, r3", "e1 17 fc 1a"),
            ("divqu r1, r2, r3", "e1 17 fe 1a"),
            ("mac r1, r2, r4, r6", "e1 17 c6 23"),
            ("macu r1, r2, r4, r6", "e1 17 e6 23"),
        ],
    );
}

/// Byte swaps and bit searches.
#[test]
fn rh850_byte_swaps_and_bit_searches() {
    check(
        "rh850",
        &[
            ("bsh r1, r2", "e0 0f 42 13"),
            ("bsw r1, r2", "e0 0f 40 13"),
            ("hsh r1, r2", "e0 0f 46 13"),
            ("hsw r1, r2", "e0 0f 44 13"),
            ("sch0l r1, r2", "e0 0f 64 13"),
            ("sch0r r1, r2", "e0 0f 60 13"),
            ("sch1l r1, r2", "e0 0f 66 13"),
            ("sch1r r1, r2", "e0 0f 62 13"),
            ("bsw r31, r0", "e0 ff 40 03"),
        ],
    );
}

/// Bins: the three encodings for a field above, across and below bit 16.
#[test]
fn rh850_bins_the_three_encodings_for_a_field_above_across_and_below_() {
    check(
        "rh850",
        &[
            ("bins r1, 0, 8, r2", "e1 17 d0 70"),
            ("bins r1, 8, 16, r2", "e1 17 b0 78"),
            ("bins r1, 20, 4, r2", "e1 17 98 70"),
            ("bins r1, 0, 32, r2", "e1 17 b0 f0"),
            ("bins r1, 15, 2, r2", "e1 17 be 08"),
            ("bins r1, 31, 1, r2", "e1 17 9e f8"),
            ("bins r1, 16, 16, r2", "e1 17 90 f0"),
            ("bins r1, 15, 1, r2", "e1 17 de f8"),
            ("bins r1, 0, 16, r2", "e1 17 d0 f0"),
            ("bins r1, 16, 1, r2", "e1 17 90 00"),
            ("bins r1, 0, 1, r2", "e1 17 d0 00"),
            ("bins r1, 20, 12, r2", "e1 17 98 f0"),
        ],
    );
}

/// Sign and zero extension, conditional move and set.
#[test]
fn rh850_sign_and_zero_extension_conditional_move_and_set() {
    check(
        "rh850",
        &[
            ("sxb r1", "a1 00"),
            ("sxh r1", "e1 00"),
            ("zxb r1", "81 00"),
            ("zxh r1", "c1 00"),
            ("cmov z, r1, r2, r3", "e1 17 24 1b"),
            ("cmov nz, 5, r2, r3", "e5 17 14 1b"),
            ("cmov sa, r1, r2, r3", "e1 17 3a 1b"),
            ("sasf c, r1", "e1 0f 00 02"),
            ("sasf sa, r1", "ed 0f 00 02"),
        ],
    );
}

/// Short loads without sign extension.
#[test]
fn rh850_short_loads_without_sign_extension() {
    check(
        "rh850",
        &[
            ("sld.bu 5[ep], r2", "65 10"),
            ("sld.bu 15[ep], r2", "6f 10"),
            ("sld.bu -1[ep], r2", "6f 10"),
            ("sld.hu 6[ep], r2", "73 10"),
            ("sld.hu 30[ep], r2", "7f 10"),
            ("ld.bu 5[r1], r2", "a1 17 05 00"),
            ("ld.bu -1[r1], r2", "a1 17 ff ff"),
            ("ld.bu 0x7fff[r1], r2", "a1 17 ff 7f"),
            ("ld.hu 6[r1], r2", "e1 17 07 00"),
            ("ld.bu lo(0x12348001)[r1], r2", "a1 17 01 80"),
        ],
    );
}

/// 48-bit loads and stores, chosen when the displacement misses 16 bits.
#[test]
fn rh850_48_bit_loads_and_stores_chosen_when_the_displacement_misses_() {
    check(
        "rh850",
        &[
            ("ld.b 0x8000[r1], r2", "81 07 05 10 00 01"),
            ("ld.b 0x3fffff[r1], r2", "81 07 f5 17 ff 7f"),
            ("ld.bu 0x8000[r1], r2", "a1 07 05 10 00 01"),
            ("ld.bu -32769[r1], r2", "a1 07 f5 17 ff fe"),
            ("ld.bu -0x8001[r1], r2", "a1 07 f5 17 ff fe"),
            ("ld.hu 0x8000[r1], r2", "a1 07 07 10 00 01"),
            ("ld.h 0x8000[r1], r2", "81 07 07 10 00 01"),
            ("ld.w 0x10000[r1], r2", "81 07 09 10 00 02"),
            ("ld.w -0x400000[r1], r2", "81 07 09 10 00 80"),
            ("ld.w 0x3ffffe[r1], r2", "81 07 e9 17 ff 7f"),
            ("st.w r2, 0x10000[r1]", "81 07 0f 10 00 02"),
            ("st.h r2, -0x8002[r1]", "a1 07 ed 17 ff fe"),
            ("ld.dw 8[r1], r2", "a1 07 89 10 00 00"),
            ("st.dw r2, 8[r1]", "a1 07 8f 10 00 00"),
            ("st.dw r2, 0x10000[r1]", "a1 07 0f 10 00 02"),
            ("ldl.w [r1], r2", "e1 07 78 13"),
            ("stc.w r2, [r1]", "e1 07 7a 13"),
        ],
    );
}

/// Jumps: through a register, and the 48-bit forms for far constants.
#[test]
fn rh850_jumps_through_a_register_and_the_48_bit_forms_for_far_consta() {
    check(
        "rh850",
        &[
            ("jarl [r1], r2", "e1 c7 60 11"),
            ("jarl r1, lp", "e1 c7 60 f9"),
            ("jarl 0x200000, r1", "e1 02 00 00 20 00"),
            ("jr 0x200000", "e0 02 00 00 20 00"),
            ("jr -0x200002", "e0 02 fe ff df ff"),
            ("jmp 0x12345678[r1]", "e1 06 78 56 34 12"),
            ("jmp -2[r1]", "e1 06 fe ff ff ff"),
            ("jmp 0x80000000[r1]", "e1 06 00 00 00 80"),
            ("switch r1", "41 00"),
            ("callt 0", "00 02"),
            ("callt 63", "3f 02"),
            ("ctret", "e0 07 44 01"),
        ],
    );
}

/// Bit operations with the bit number in a register.
#[test]
fn rh850_bit_operations_with_the_bit_number_in_a_register() {
    check(
        "rh850",
        &[
            ("set1 r1, [r2]", "e2 0f e0 00"),
            ("clr1 r1, [r2]", "e2 0f e4 00"),
            ("tst1 r1, [r2]", "e2 0f e6 00"),
            ("not1 r1, [r2]", "e2 0f e2 00"),
        ],
    );
}

/// System registers with a group, and the RH850 names.
#[test]
fn rh850_system_registers_with_a_group_and_the_rh850_names() {
    check(
        "rh850",
        &[
            ("ldsr r1, psw, 1", "e1 2f 20 08"),
            ("ldsr r1, sr31, 31", "e1 ff 20 f8"),
            ("ldsr r1, ctbp", "e1 a7 20 00"),
            ("ldsr r1, 31", "e1 ff 20 00"),
            ("ldsr r1, 32", "e1 07 20 08"),
            ("ldsr r1, 1023", "e1 ff 20 f8"),
            ("stsr psw, r1, 1", "e5 0f 40 08"),
            ("stsr 5, r1", "e5 0f 40 00"),
            ("stsr ctpc, r1", "f0 0f 40 00"),
            ("stsr fpsr, r1", "e6 0f 40 00"),
            ("stsr 1023, r1", "ff 0f 40 f8"),
            ("ldtc.gr r1, r2", "e1 17 32 00"),
            ("ldtc.sr r1, 5", "e1 2f 30 00"),
            ("ldtc.vr r1, vr5", "e1 2f 32 08"),
            ("ldtc.pc r1", "e1 07 32 f8"),
            ("ldvc.sr r1, 5", "e1 2f 34 00"),
            ("sttc.gr r1, r2", "e1 17 52 00"),
            ("sttc.sr 5, r1", "e5 0f 50 00"),
            ("sttc.vr vr5, r1", "e5 0f 52 08"),
            ("sttc.pc r1", "e0 0f 52 f8"),
            ("stvc.sr 5, r1", "e5 0f 54 00"),
        ],
    );
}

/// Prepare and dispose.
#[test]
fn rh850_prepare_and_dispose() {
    check(
        "rh850",
        &[
            ("prepare {r20-r29, r31}, 4", "88 07 e1 ff"),
            ("prepare {r31}, 0", "80 07 21 00"),
            ("prepare {}, 0", "80 07 01 00"),
            ("prepare {r20}, 31", "be 07 01 08"),
            ("prepare {r30}, 0", "81 07 01 00"),
            ("prepare {ep, lp}, 1", "83 07 21 00"),
            ("prepare {r29, r28, r20}, 2", "84 07 c1 08"),
            ("prepare {r20 - r22}, 0", "80 07 01 0e"),
            (
                "prepare {r20,r21,r22,r23,r24,r25,r26,r27,r28,r29,r30,r31}, 0",
                "81 07 e1 ff",
            ),
            ("prepare 0xfff, 0", "81 07 e1 ff"),
            ("prepare {r20-r29, r31}, 4, sp", "88 07 e3 ff"),
            ("prepare {r20-r29, r31}, 4, 0x10", "88 07 eb ff 10 00"),
            ("prepare {r20-r29, r31}, 4, -1", "88 07 eb ff ff ff"),
            (
                "prepare {r20-r29, r31}, 4, 0x8000",
                "88 07 fb ff 00 80 00 00",
            ),
            ("prepare {r20-r29, r31}, 4, 0x10000", "88 07 f3 ff 01 00"),
            (
                "prepare {r20-r29, r31}, 4, 0x12345678",
                "88 07 fb ff 78 56 34 12",
            ),
            ("prepare {}, 0, 0", "80 07 0b 00 00 00"),
            ("prepare {}, 0, 0xffffffff", "80 07 0b 00 ff ff"),
            ("prepare {}, 0, 0xffff8000", "80 07 0b 00 00 80"),
            ("prepare {}, 0, -0x8001", "80 07 1b 00 ff 7f ff ff"),
            ("prepare {}, 0, 0x7fff", "80 07 0b 00 ff 7f"),
            ("prepare {}, 0, 0xffff", "80 07 1b 00 ff ff 00 00"),
            ("prepare {}, 0, hi(0x12348000)", "80 07 13 00 35 12"),
            ("prepare {}, 0, lo(0x12348000)", "80 07 0b 00 00 80"),
            ("prepare {}, 0, hilo(0x12348000)", "80 07 1b 00 00 80 34 12"),
            ("dispose 4, {r20-r29, r31}", "48 06 e0 ff"),
            ("dispose 4, {r20-r29, r31}, r31", "48 06 ff ff"),
            ("dispose 0, {r31}, [lp]", "40 06 3f 00"),
            ("dispose 0, {}", "40 06 00 00"),
            ("dispose 31, {r30}", "7f 06 00 00"),
            ("dispose 0, {}, [r31]", "40 06 1f 00"),
            ("dispose 0, {}, r1", "40 06 01 00"),
        ],
    );
}

/// Stack and debug.
#[test]
fn rh850_stack_and_debug() {
    check(
        "rh850",
        &[
            ("pushsp r20-r25", "f4 47 60 c9"),
            ("pushsp r20, r25", "f4 47 60 c9"),
            ("pushsp r20-r20", "f4 47 60 a1"),
            ("pushsp r25-r20", "f9 47 60 a1"),
            ("pushsp r0-r31", "e0 47 60 f9"),
            ("popsp r20-r25", "f4 67 60 c9"),
            ("popsp r1, r1", "e1 67 60 09"),
            ("dbpush r1-r3", "e1 5f 60 19"),
            ("dbtag 5", "e5 cf 60 01"),
            ("dbtag 1023", "ff cf 60 f9"),
            ("dbtrap", "40 f8"),
            ("dbret", "e0 07 46 01"),
            ("dbcp", "40 e8"),
            ("dbhvtrap", "40 e0"),
        ],
    );
}

/// Exceptions, traps and control.
#[test]
fn rh850_exceptions_traps_and_control() {
    check(
        "rh850",
        &[
            ("eiret", "e0 07 48 01"),
            ("feret", "e0 07 4a 01"),
            ("fetrap 1", "40 08"),
            ("fetrap 15", "40 78"),
            ("syscall 0", "e0 d7 60 01"),
            ("syscall 255", "ff d7 60 39"),
            ("hvcall 5", "e5 d7 60 41"),
            ("hvcall 255", "ff d7 60 79"),
            ("hvtrap 5", "e5 07 10 01"),
            ("hvtrap 31", "ff 07 10 01"),
            ("rie", "40 00"),
            ("rie 1, 2", "f2 0f 00 00"),
            ("rie 31, 15", "ff ff 00 00"),
            ("rmtrap", "40 f0"),
            ("synce", "1d 00"),
            ("synci", "1c 00"),
            ("syncm", "1e 00"),
            ("syncp", "1f 00"),
            ("snooze", "e0 0f 20 01"),
            ("tlbai", "e0 87 60 89"),
            ("tlbr", "e0 87 60 e9"),
            ("tlbs", "e0 87 60 c1"),
            ("tlbvi", "e0 87 60 81"),
            ("tlbw", "e0 87 60 e1"),
            ("dst", "e0 07 34 01"),
            ("est", "e0 07 32 01"),
            ("caxi [r1], r2, r3", "e1 17 ee 18"),
            ("caxi r1, r2, r3", "e1 17 ee 18"),
            ("cache chbii, [r1]", "e1 e7 60 01"),
            ("cache cistd, [r1]", "e1 ff 60 21"),
            ("cache 0x7f, [r1]", "e1 ff 60 f9"),
            ("pref prefi, [r1]", "e1 df 60 01"),
            ("pref prefd, [r1]", "e1 df 60 21"),
            ("pref 4, [r1]", "e1 df 60 21"),
        ],
    );
}

/// Loop with a numeric backward distance.
#[test]
fn rh850_loop_with_a_numeric_backward_distance() {
    check(
        "rh850",
        &[
            ("loop r1, 10", "e1 06 0b 00"),
            ("loop r1, 0xfffe", "e1 06 ff ff"),
        ],
    );
}

/// Floating point.
#[test]
fn rh850_floating_point() {
    check(
        "rh850",
        &[
            ("absf.d r2, r4", "e0 17 58 24"),
            ("absf.s r1, r2", "e0 0f 48 14"),
            ("addf.d r2, r4, r6", "e2 27 70 34"),
            ("addf.s r1, r2, r3", "e1 17 60 1c"),
            ("ceilf.dl r2, r4", "e2 17 54 24"),
            ("ceilf.dul r2, r4", "f2 17 54 24"),
            ("ceilf.duw r2, r3", "f2 17 50 1c"),
            ("ceilf.dw r2, r3", "e2 17 50 1c"),
            ("ceilf.sl r1, r4", "e2 0f 44 24"),
            ("ceilf.sul r1, r4", "f2 0f 44 24"),
            ("ceilf.suw r1, r3", "f2 0f 40 1c"),
            ("ceilf.sw r1, r3", "e2 0f 40 1c"),
            ("cmovf.d 1, r2, r4, r6", "e2 27 12 34"),
            ("cmovf.d r2, r4, r6", "e2 27 10 34"),
            ("cmovf.s 7, r1, r2, r3", "e1 17 0e 1c"),
            ("cmovf.s r1, r2, r3", "e1 17 00 1c"),
            ("cmpf.d eq, r2, r4", "e4 17 30 14"),
            ("cmpf.d eq, r2, r4, 3", "e4 17 36 14"),
            ("cmpf.s lt, r1, r2", "e2 0f 20 64"),
            ("cmpf.s ule, r1, r2, 7", "e2 0f 2e 3c"),
            ("cmpf.s 5, r1, r2", "e2 0f 20 2c"),
            ("cvtf.dl r2, r4", "e4 17 54 24"),
            ("cvtf.ds r2, r3", "e3 17 52 1c"),
            ("cvtf.dul r2, r4", "f4 17 54 24"),
            ("cvtf.duw r2, r3", "f4 17 50 1c"),
            ("cvtf.dw r2, r3", "e4 17 50 1c"),
            ("cvtf.hs r1, r2", "e2 0f 42 14"),
            ("cvtf.ld r2, r4", "e1 17 52 24"),
            ("cvtf.ls r2, r3", "e1 17 42 1c"),
            ("cvtf.sd r1, r4", "e2 0f 52 24"),
            ("cvtf.sl r1, r4", "e4 0f 44 24"),
            ("cvtf.sh r1, r2", "e3 0f 42 14"),
            ("cvtf.sul r1, r4", "f4 0f 44 24"),
            ("cvtf.suw r1, r3", "f4 0f 40 1c"),
            ("cvtf.sw r1, r3", "e4 0f 40 1c"),
            ("cvtf.uld r2, r4", "f1 17 52 24"),
            ("cvtf.uls r2, r3", "f1 17 42 1c"),
            ("cvtf.uwd r1, r4", "f0 0f 52 24"),
            ("cvtf.uws r1, r3", "f0 0f 42 1c"),
            ("cvtf.wd r1, r4", "e0 0f 52 24"),
            ("cvtf.ws r1, r3", "e0 0f 42 1c"),
            ("divf.d r2, r4, r6", "e2 27 7e 34"),
            ("divf.s r1, r2, r3", "e1 17 6e 1c"),
            ("floorf.dl r2, r4", "e3 17 54 24"),
            ("floorf.dul r2, r4", "f3 17 54 24"),
            ("floorf.duw r2, r3", "f3 17 50 1c"),
            ("floorf.dw r2, r3", "e3 17 50 1c"),
            ("floorf.sl r1, r4", "e3 0f 44 24"),
            ("floorf.sul r1, r4", "f3 0f 44 24"),
            ("floorf.suw r1, r3", "f3 0f 40 1c"),
            ("floorf.sw r1, r3", "e3 0f 40 1c"),
            ("fmaf.s r1, r2, r3", "e1 17 e0 1c"),
            ("fmsf.s r1, r2, r3", "e1 17 e2 1c"),
            ("fnmaf.s r1, r2, r3", "e1 17 e4 1c"),
            ("fnmsf.s r1, r2, r3", "e1 17 e6 1c"),
            ("maxf.d r2, r4, r6", "e2 27 78 34"),
            ("maxf.s r1, r2, r3", "e1 17 68 1c"),
            ("minf.d r2, r4, r6", "e2 27 7a 34"),
            ("minf.s r1, r2, r3", "e1 17 6a 1c"),
            ("mulf.d r2, r4, r6", "e2 27 74 34"),
            ("mulf.s r1, r2, r3", "e1 17 64 1c"),
            ("negf.d r2, r4", "e1 17 58 24"),
            ("negf.s r1, r2", "e1 0f 48 14"),
            ("recipf.d r2, r4", "e1 17 5e 24"),
            ("recipf.s r1, r2", "e1 0f 4e 14"),
            ("rsqrtf.d r2, r4", "e2 17 5e 24"),
            ("rsqrtf.s r1, r2", "e2 0f 4e 14"),
            ("sqrtf.d r2, r4", "e0 17 5e 24"),
            ("sqrtf.s r1, r2", "e0 0f 4e 14"),
            ("subf.d r2, r4, r6", "e2 27 72 34"),
            ("subf.s r1, r2, r3", "e1 17 62 1c"),
            ("trfsr", "e0 07 00 04"),
            ("trfsr 3", "e0 07 06 04"),
            ("trfsr 7", "e0 07 0e 04"),
            ("trncf.dl r2, r4", "e1 17 54 24"),
            ("trncf.dul r2, r4", "f1 17 54 24"),
            ("trncf.duw r2, r3", "f1 17 50 1c"),
            ("trncf.dw r2, r3", "e1 17 50 1c"),
            ("trncf.sl r1, r4", "e1 0f 44 24"),
            ("trncf.sul r1, r4", "f1 0f 44 24"),
            ("trncf.suw r1, r3", "f1 0f 40 1c"),
            ("trncf.sw r1, r3", "e1 0f 40 1c"),
        ],
    );
}

//! MIPS encoding tests.
//!
//! Every expected byte string here was taken from a run of
//! `tools/mc-diff/run.sh mips mipsel mips64`, which assembles the same source
//! with rsasm and with llvm-mc 22 and compares the `.text` bytes. Nothing in
//! this file is an encoding rsasm invented for itself.
//!
//! The comparison is made in `.set noreorder` mode: rsasm emits exactly the
//! instructions written and never fills a delay slot. llvm-mc defaults to
//! `.set reorder`, so the corpus works around the `nop` it inserts (see the
//! header of `tools/mc-diff/mips-programs.txt`).

#![cfg(feature = "mips")]

mod common;
use common::*;

/// Asserts that `src` assembles for big-endian MIPS32 to `want`.
#[track_caller]
fn enc(src: &str, want: &str) {
    let got = hex(&text_for("mips", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// Asserts that `src` assembles for big-endian MIPS64 to `want`.
#[track_caller]
fn enc64(src: &str, want: &str) {
    let got = hex(&text_for("mips64", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

#[test]
fn three_register_arithmetic_and_logic() {
    enc("add $1, $2, $3", "00 43 08 20");
    enc("addu $a0, $a1, $a2", "00 a6 20 21");
    enc("sub $s0, $s1, $s2", "02 32 80 22");
    enc("subu $v0, $v1, $a0", "00 64 10 23");
    enc("and $t3, $t4, $t5", "01 8d 58 24");
    enc("or $t6, $t7, $s0", "01 f0 70 25");
    enc("xor $s1, $s2, $s3", "02 53 88 26");
    enc("nor $s4, $s5, $s6", "02 b6 a0 27");
    enc("slt $s7, $t8, $t9", "03 19 b8 2a");
    enc("sltu $k0, $k1, $gp", "03 7c d0 2b");
    // `mul` is SPECIAL2, not SPECIAL, so its opcode field is not zero.
    enc("mul $4, $5, $6", "70 a6 20 02");
}

#[test]
fn registers_may_be_named_or_numbered() {
    // The same instruction three ways.
    let by_number = text_for("mips", "add $4, $5, $6");
    assert_eq!(by_number, text_for("mips", "add $a0, $a1, $a2"));
    // `$fp` and `$s8` are both register 30.
    assert_eq!(
        text_for("mips", "add $fp, $fp, $fp"),
        text_for("mips", "add $s8, $s8, $s8")
    );
    enc("add $zero, $at, $v0", "00 22 00 20");
    enc("add $sp, $fp, $ra", "03 df e8 20");
}

#[test]
fn shifts_put_the_shifted_value_in_rt() {
    enc("sll $1, $2, 3", "00 02 08 c0");
    enc("sll $1, $2, 31", "00 02 0f c0");
    enc("srl $3, $4, 1", "00 04 18 42");
    enc("sra $5, $6, 16", "00 06 2c 03");
    // Variable shifts take the count last, in `rs`.
    enc("sllv $7, $8, $9", "01 28 38 04");
    enc("srlv $10, $11, $12", "01 8b 50 06");
    enc("srav $13, $14, $15", "01 ee 68 07");
}

#[test]
fn multiply_divide_and_the_hi_lo_pair() {
    enc("mult $4, $5", "00 85 00 18");
    enc("multu $6, $7", "00 c7 00 19");
    enc("div $zero, $8, $9", "01 09 00 1a");
    enc("divu $zero, $10, $11", "01 4b 00 1b");
    enc("mfhi $16", "00 00 80 10");
    enc("mflo $17", "00 00 88 12");
    enc("mthi $18", "02 40 00 11");
    enc("mtlo $19", "02 60 00 13");
    // The two-operand spelling means the same instruction: the destination
    // field of a divide is architecturally ignored.
    assert_eq!(
        text_for("mips", "div $8, $9"),
        text_for("mips", "div $zero, $8, $9")
    );
}

#[test]
fn immediate_arithmetic() {
    enc("addi $4, $5, -1", "20 a4 ff ff");
    enc("addi $4, $5, 32767", "20 a4 7f ff");
    enc("addiu $a0, $a1, -100", "24 a4 ff 9c");
    enc("slti $4, $5, -1", "28 a4 ff ff");
    enc("sltiu $4, $5, 1000", "2c a4 03 e8");
    enc("andi $4, $5, 0xffff", "30 a4 ff ff");
    enc("ori $4, $5, 0x1234", "34 a4 12 34");
    enc("xori $4, $5, 0xabcd", "38 a4 ab cd");
    enc("lui $4, 0xffff", "3c 04 ff ff");
    // A 16-bit field takes either spelling of the same bit pattern.
    assert_eq!(
        text_for("mips", "addiu $4, $5, -1"),
        text_for("mips", "addiu $4, $5, 0xffff")
    );
}

#[test]
fn loads_and_stores() {
    enc("lb $4, -1($5)", "80 a4 ff ff");
    enc("lbu $4, 1($5)", "90 a4 00 01");
    enc("lh $4, 2($5)", "84 a4 00 02");
    enc("lhu $4, 4($5)", "94 a4 00 04");
    enc("lw $4, 8($5)", "8c a4 00 08");
    enc("lw $4, -32768($5)", "8c a4 80 00");
    enc("lwl $4, 3($5)", "88 a4 00 03");
    enc("lwr $4, 0($5)", "98 a4 00 00");
    enc("sb $4, 0($5)", "a0 a4 00 00");
    enc("sh $4, 2($5)", "a4 a4 00 02");
    enc("sw $4, 4($5)", "ac a4 00 04");
    enc("swl $4, 3($5)", "a8 a4 00 03");
    enc("swr $4, 0($5)", "b8 a4 00 00");
    enc("sw $ra, 28($sp)", "af bf 00 1c");
    // A missing displacement is zero.
    enc("lw $t0, ($sp)", "8f a8 00 00");
    enc("ll $4, 8($5)", "c0 a4 00 08");
    enc("sc $4, 8($5)", "e0 a4 00 08");
}

#[test]
fn register_aliases_are_real_instructions() {
    // MIPS `nop` is `sll $zero, $zero, 0`: the all-zero word.
    enc("nop", "00 00 00 00");
    enc("sll $zero, $zero, 0", "00 00 00 00");
    enc("move $4, $5", "00 a0 20 25");
    enc("not $4, $5", "00 a0 20 27");
    // Negation subtracts from zero, so the operand lands in `rt`.
    enc("neg $4, $5", "00 05 20 22");
    enc("negu $4, $5", "00 05 20 23");
}

#[test]
fn li_picks_the_shortest_sequence() {
    // Fits a sign-extended 16-bit field: one `addiu`.
    enc("li $4, 0", "24 04 00 00");
    enc("li $4, -1", "24 04 ff ff");
    enc("li $4, 0x7fff", "24 04 7f ff");
    // Fits a zero-extended one: one `ori`.
    enc("li $4, 0x8000", "34 04 80 00");
    enc("li $4, 0xffff", "34 04 ff ff");
    // No low half: one `lui`.
    enc("li $4, 0x10000", "3c 04 00 01");
    enc("li $4, -0x80000000", "3c 04 80 00");
    // Everything else takes two.
    enc("li $4, 0x12345", "3c 04 00 01 34 84 23 45");
    enc("li $4, -32769", "3c 04 ff ff 34 84 7f ff");
    enc("li $4, 0x7fffffff", "3c 04 7f ff 34 84 ff ff");
    // The value is read as a signed 32-bit word, so this is `addiu -1`, not a
    // two-instruction load.
    enc("li $4, 0xffffffff", "24 04 ff ff");
    enc("la $4, 0x12345", "3c 04 00 01 34 84 23 45");
}

#[test]
fn system_instructions() {
    enc("syscall", "00 00 00 0c");
    enc("syscall 3", "00 00 00 cc");
    enc("break", "00 00 00 0d");
    // A single `break` operand fills the upper of its two code fields.
    enc("break 7", "00 07 00 0d");
    enc("break 7, 8", "00 07 02 0d");
    enc("sync", "00 00 00 0f");
    enc("sync 1", "00 00 00 4f");
    enc("eret", "42 00 00 18");
    enc("teq $4, $5", "00 85 00 34");
    // A trap's optional code sits in bits 15..6.
    enc("teq $4, $5, 7", "00 85 01 f4");
    enc("tne $4, $5, 1023", "00 85 ff f6");
    enc("mfc0 $4, $12", "40 04 60 00");
    enc("mtc0 $4, $12", "40 84 60 00");
    enc("mfc0 $4, $12, 1", "40 04 60 01");
}

#[test]
fn floating_point() {
    enc("mfc1 $4, $f0", "44 04 00 00");
    enc("mtc1 $4, $f12", "44 84 60 00");
    enc("lwc1 $f0, 8($4)", "c4 80 00 08");
    enc("ldc1 $f2, 16($sp)", "d7 a2 00 10");
    enc("swc1 $f4, 0($5)", "e4 a4 00 00");
    enc("sdc1 $f6, 24($sp)", "f7 a6 00 18");
    enc("add.s $f0, $f2, $f4", "46 04 10 00");
    enc("add.d $f0, $f2, $f4", "46 24 10 00");
    enc("sub.d $f6, $f8, $f10", "46 2a 41 81");
    enc("mul.s $f0, $f2, $f4", "46 04 10 02");
    enc("div.s $f0, $f2, $f4", "46 04 10 03");
    enc("abs.s $f0, $f2", "46 00 10 05");
    enc("neg.d $f0, $f2", "46 20 10 07");
    enc("mov.s $f0, $f2", "46 00 10 06");
    enc("mov.d $f30, $f28", "46 20 e7 86");
    enc("sqrt.d $f0, $f2", "46 20 10 04");
    // In a conversion the *source* format is the field and the destination is
    // part of the function code.
    enc("cvt.s.d $f0, $f2", "46 20 10 20");
    enc("cvt.s.w $f0, $f2", "46 80 10 20");
    enc("cvt.d.w $f0, $f2", "46 80 10 21");
    enc("cvt.w.s $f0, $f2", "46 00 10 24");
    enc("trunc.w.s $f0, $f2", "46 00 10 0d");
    enc("c.eq.s $f0, $f2", "46 02 00 32");
    enc("c.lt.d $f0, $f2", "46 22 00 3c");
    enc("c.ule.s $f0, $f2", "46 02 00 37");
    enc("c.ngt.s $f0, $f2", "46 02 00 3f");
}

#[test]
fn branches_are_measured_from_the_delay_slot() {
    // A branch to its own address is -4 bytes from the delay slot: -1 word.
    enc("foo: beq $1, $2, foo", "10 22 ff ff");
    enc("foo: bne $a0, $a1, foo", "14 85 ff ff");
    enc("foo: blez $a0, foo", "18 80 ff ff");
    enc("foo: bgtz $a0, foo", "1c 80 ff ff");
    enc("foo: bltz $a0, foo", "04 80 ff ff");
    enc("foo: bgez $a0, foo", "04 81 ff ff");
    enc("foo: bltzal $a0, foo", "04 90 ff ff");
    enc("foo: bgezal $a0, foo", "04 91 ff ff");
    enc("foo: bc1f foo", "45 00 ff ff");
    enc("foo: bc1t foo", "45 01 ff ff");
    enc(
        "foo: addiu $4, $4, 1\naddiu $5, $5, -1\nbeq $4, $5, foo",
        "24 84 00 01 24 a5 ff ff 10 85 ff fd",
    );
    // Forward: the target is three words past the delay slot.
    enc(
        "beq $1, $2, end\nnop\nnop\nend: nop",
        "10 22 00 02 00 00 00 00 00 00 00 00 00 00 00 00",
    );
}

#[test]
fn branch_pseudo_instructions() {
    // `b` is `beq $zero, $zero`; `bal` is the always-true `bgezal $zero`.
    enc("foo: b foo", "10 00 ff ff");
    enc("foo: bal foo", "04 11 ff ff");
    enc("foo: beqz $v0, foo", "10 40 ff ff");
    enc("foo: bnez $v0, foo", "14 40 ff ff");
    // The ordered branches compute the predicate into $at first.
    enc("foo: bge $a0, $a1, foo", "00 85 08 2a 10 20 ff fe");
    enc("foo: bgt $a0, $a1, foo", "00 a4 08 2a 14 20 ff fe");
    enc("foo: ble $a0, $a1, foo", "00 a4 08 2a 10 20 ff fe");
    enc("foo: blt $a0, $a1, foo", "00 85 08 2a 14 20 ff fe");
    enc("foo: bgeu $a0, $a1, foo", "00 85 08 2b 10 20 ff fe");
    enc("foo: bgtu $a0, $a1, foo", "00 a4 08 2b 14 20 ff fe");
    enc("foo: bleu $a0, $a1, foo", "00 a4 08 2b 10 20 ff fe");
    enc("foo: bltu $a0, $a1, foo", "00 85 08 2b 14 20 ff fe");
    // Against $zero the signed orderings collapse to a sign test.
    enc("foo: bge $a0, $zero, foo", "04 81 ff ff");
    enc("foo: blt $a0, $zero, foo", "04 80 ff ff");
}

#[test]
fn register_jumps() {
    enc("jr $ra", "03 e0 00 08");
    // One operand links through $ra.
    enc("jalr $t9", "03 20 f8 09");
    enc("jalr $s0, $t9", "03 20 80 09");
}

#[test]
fn a_jump_target_is_a_word_index_not_a_displacement() {
    // `j` keeps the top four bits of the delay slot's address and replaces the
    // rest, so the encoded field is the target address shifted right by two —
    // unlike a branch, it is not relative to anything.
    enc("j 0x400000", "08 10 00 00");
    enc("j 0x0ffffffc", "0b ff ff ff");
    // The region bits are supplied by the PC, so a target above 256 MB is not
    // out of range: this is an ordinary call inside kseg0.
    enc("jal 0x80001000", "0c 00 04 00");
    // A label resolves the same way once the section has an address.
    let asm = assemble_flat_for("mips", "foo: nop\nj foo", 0x0040_0000);
    assert_eq!(
        hex(&asm.section_bytes(rsasm::section::SectionId(0))),
        "00 00 00 00 08 10 00 00"
    );
}

#[test]
fn a_misaligned_jump_target_is_an_error() {
    let e = errors_for("mips", "j 0x400001");
    // Misalignment is its own problem, not a range problem, and the message
    // says which.
    assert!(e.contains("not a multiple of 4"), "{e}");
}

#[test]
fn hi_and_lo_split_a_32_bit_address() {
    enc(
        "sym = 0x12345678\nlui $a0, %hi(sym)\naddiu $a0, $a0, %lo(sym)",
        "3c 04 12 34 24 84 56 78",
    );
    // %hi is biased so that adding the *sign-extended* %lo gets back to the
    // address: with a low half of 0xabcd the high half must round up.
    enc(
        "sym = 0x1234abcd\nlui $a0, %hi(sym)\naddiu $a0, $a0, %lo(sym)",
        "3c 04 12 35 24 84 ab cd",
    );
}

#[test]
fn a_real_loop() {
    enc(
        "loop: lw $t0, 0($a0)\naddiu $a0, $a0, 4\naddiu $a1, $a1, -1\nsw $t0, 0($a1)\nbnez $a1, loop",
        "8c 88 00 00 24 84 00 04 24 a5 ff ff ac a8 00 00 14 a0 ff fb",
    );
}

// ---- byte order -----------------------------------------------------------

#[test]
fn mipsel_emits_the_same_words_byte_reversed() {
    for src in [
        "add $1, $2, $3",
        "lw $4, 8($5)",
        "li $4, 0x12345",
        "foo: beq $1, $2, foo",
        "add.d $f0, $f2, $f4",
    ] {
        let be = text_for("mips", src);
        let le = text_for("mipsel", src);
        assert_eq!(be.len(), le.len(), "{src}");
        let swapped: Vec<u8> = be.chunks(4).flat_map(|w| w.iter().rev().copied()).collect();
        assert_eq!(hex(&le), hex(&swapped), "\nsource: {src}");
    }
    // And the 64-bit pair behaves the same way.
    let be = text_for("mips64", "ld $4, 8($5)");
    let le = text_for("mips64el", "ld $4, 8($5)");
    assert_eq!(hex(&be), "dc a4 00 08");
    assert_eq!(hex(&le), "08 00 a4 dc");
}

// ---- 64-bit ---------------------------------------------------------------

#[test]
fn doubleword_arithmetic_and_shifts() {
    enc64("dadd $4, $5, $6", "00 a6 20 2c");
    enc64("daddu $4, $5, $6", "00 a6 20 2d");
    enc64("dsub $4, $5, $6", "00 a6 20 2e");
    enc64("dsubu $4, $5, $6", "00 a6 20 2f");
    enc64("daddi $4, $5, 100", "60 a4 00 64");
    enc64("daddiu $sp, $sp, -32", "67 bd ff e0");
    enc64("dsll $4, $5, 3", "00 05 20 f8");
    enc64("dsrl $4, $5, 3", "00 05 20 fa");
    enc64("dsra $4, $5, 3", "00 05 20 fb");
    // The `32` forms add 32 to the written shift amount, which is how a 5-bit
    // field spans a 6-bit range.
    enc64("dsll32 $4, $5, 3", "00 05 20 fc");
    enc64("dsrl32 $4, $5, 3", "00 05 20 fe");
    enc64("dsra32 $4, $5, 3", "00 05 20 ff");
    enc64("dsllv $4, $5, $6", "00 c5 20 14");
    enc64("dsrlv $4, $5, $6", "00 c5 20 16");
    enc64("dsrav $4, $5, $6", "00 c5 20 17");
    enc64("dmult $4, $5", "00 85 00 1c");
    enc64("dmultu $4, $5", "00 85 00 1d");
    enc64("ddiv $zero, $4, $5", "00 85 00 1e");
    enc64("ddivu $zero, $4, $5", "00 85 00 1f");
}

#[test]
fn doubleword_memory_and_coprocessor() {
    enc64("ld $4, 8($5)", "dc a4 00 08");
    enc64("sd $ra, 24($sp)", "ff bf 00 18");
    enc64("lwu $4, 8($5)", "9c a4 00 08");
    enc64("ldl $4, 8($5)", "68 a4 00 08");
    enc64("ldr $4, 8($5)", "6c a4 00 08");
    enc64("sdl $4, 8($5)", "b0 a4 00 08");
    enc64("sdr $4, 8($5)", "b4 a4 00 08");
    enc64("dmfc1 $4, $f0", "44 24 00 00");
    enc64("dmtc1 $4, $f0", "44 a4 00 00");
    enc64("cvt.l.s $f0, $f2", "46 00 10 25");
    enc64("cvt.s.l $f0, $f2", "46 a0 10 20");
    enc64("trunc.l.s $f0, $f2", "46 00 10 09");
}

#[test]
fn la_widens_on_a_64_bit_target() {
    // An address fills the whole register, so the narrow form has to
    // sign-extend through all 64 bits.
    enc("la $4, 8", "24 04 00 08");
    enc64("la $4, 8", "64 04 00 08");
    // `li` still loads a word.
    enc64("li $4, 8", "24 04 00 08");
}

#[test]
fn doubleword_instructions_are_rejected_on_a_32_bit_target() {
    for src in ["ld $4, 0($5)", "dadd $4, $5, $6", "dsll $4, $5, 1"] {
        let e = errors_for("mips", src);
        assert!(e.contains("64-bit"), "{src}: {e}");
    }
}

// ---- padding and object properties ----------------------------------------

#[test]
fn alignment_padding_is_all_zero_because_that_is_what_nop_is() {
    let b = text_for("mips", "add $1, $2, $3\n.p2align 4\nadd $1, $2, $3");
    assert_eq!(b.len(), 20);
    assert_eq!(&b[4..16], &[0u8; 12]);
}

#[test]
fn object_level_properties() {
    for name in ["mips", "mipsel", "mips64", "mips64el"] {
        let a = rsasm::arch::lookup(name).expect("backend present");
        assert_eq!(a.elf_machine(), 8, "{name}");
        assert_eq!(a.data_reloc(4, false), Some(2), "{name}"); // R_MIPS_32
        assert_eq!(a.data_reloc(8, false), Some(18), "{name}"); // R_MIPS_64
        assert_eq!(a.data_reloc(4, true), Some(248), "{name}"); // R_MIPS_PC32
    }
    assert_eq!(
        rsasm::arch::lookup("mips")
            .expect("mips")
            .pointer_bytes(&rsasm::arch::lookup("mips").expect("mips").initial_state()),
        4
    );
    assert_eq!(
        rsasm::arch::lookup("mips64")
            .expect("mips64")
            .pointer_bytes(
                &rsasm::arch::lookup("mips64")
                    .expect("mips64")
                    .initial_state()
            ),
        8
    );
}

// ---- diagnostics ----------------------------------------------------------

#[test]
fn out_of_range_values_name_their_limit() {
    let e = errors_for("mips", "addiu $4, $5, 0x10000");
    assert!(e.contains("16-bit") && e.contains("65535"), "{e}");

    let e = errors_for("mips", "sll $1, $2, 32");
    assert!(e.contains("shift amount") && e.contains("0 to 31"), "{e}");

    let e = errors_for("mips", "li $4, 0x123456789");
    assert!(e.contains("32 bits"), "{e}");

    let e = errors_for("mips", "break 2000");
    assert!(e.contains("break code"), "{e}");
}

#[test]
fn an_unreachable_branch_is_an_error_not_a_truncation() {
    // A 16-bit field of words reaches +-128 KB.
    let e = errors_for("mips", "b far\n.space 0x40000\nfar: nop");
    assert!(e.contains("out of range"), "{e}");
}

#[test]
fn a_misaligned_branch_target_is_an_error() {
    let e = errors_for("mips", "b odd\n.space 1\nodd: nop");
    assert!(e.contains("not a multiple of 4"), "{e}");
}

#[test]
fn bad_operands_are_diagnosed() {
    assert!(errors_for("mips", "add $1, $2").contains("operand"));
    assert!(errors_for("mips", "add $1, $2, $3, $4").contains("operand"));
    assert!(errors_for("mips", "add $1, $2, 3").contains("integer register"));
    assert!(errors_for("mips", "lw $1, 8").contains("offset(base)"));
    assert!(errors_for("mips", "add $1, $2, $32").contains("$0-$31"));
    assert!(errors_for("mips", "add $1, $2, $nope").contains("unknown register"));
    assert!(errors_for("mips", "frobnicate $1").contains("unknown instruction"));
    assert!(errors_for("mips", "add.s $f0, $f2, $4").contains("floating-point register"));
    assert!(errors_for("mips", "add $f0, $f2, $f4").contains("integer register"));
    assert!(errors_for("mips", "lw $1, 8($f0)").contains("base register"));
    assert!(errors_for("mips", "lui $4, %got(sym)").contains("%hi or %lo"));
    assert!(errors_for("mips", "li $4, sym").contains("assembly time"));
    assert!(errors_for("mips", "div $4, $8, $9").contains("$zero"));
    assert!(errors_for("mips", "teq $4, $5, 1024").contains("0 to 1023"));
    assert!(errors_for("mips", "mult $zero, $4, $5").contains("operand"));
}

// ---- robustness -----------------------------------------------------------

/// Nonsense of various shapes. The only requirement is that the assembler
/// reports something and returns instead of panicking.
const BAD: &[&str] = &[
    "$",
    "$$",
    "$,",
    "$)",
    "add",
    "add ,",
    "add , ,",
    "add $1,",
    "add ,$1",
    "add $1, $2, $",
    "add $1 $2 $3",
    "add $, $, $",
    "add $-1, $2, $3",
    "add $999999999999, $2, $3",
    "add $f, $f, $f",
    "lw $1, (",
    "lw $1, ($",
    "lw $1, ($2",
    "lw $1, 8($2",
    "lw $1, 8)",
    "lw $1, ()",
    "lw $1, 8($2))",
    "lw $1, 8($2) $3",
    "li",
    "li $4",
    "li $4,",
    "li $4, ,",
    "la $4, %hi(sym)",
    "lui $4, %",
    "lui $4, %hi",
    "lui $4, %hi(",
    "lui $4, %hi()",
    "lui $4, %(sym)",
    "b",
    "b b",
    "b $1",
    "j",
    "jal",
    "jr",
    "jalr",
    "jalr $1, $2, $3",
    "beq",
    "beq $1",
    "beq $1, $2",
    "bge $1, $2",
    "sll $1, $2",
    "sll $1, $2, $3",
    "sll $1, $2, -1",
    "break -1",
    "sync 99",
    "syscall -1",
    "mfc0",
    "mfc0 $4",
    "mfc0 $4, $12, 99",
    "c.eq.q $f0, $f2",
    "cvt.q.s $f0, $f2",
    "add.",
    ".",
    "add.s",
    "add.s $f0",
    "nop $1",
    "eret $1",
    "0",
    "$0",
];

#[test]
fn malformed_input_never_panics() {
    for arch in ["mips", "mipsel", "mips64", "mips64el"] {
        for src in BAD {
            // Either outcome is fine; a panic or a hang is not.
            let _ = try_text_for(arch, src);
        }
    }
}

#[test]
fn set_noreorder_is_accepted_and_matches_the_oracle() {
    // With `.set noreorder`, llvm-mc stops inserting a `nop` after the branch,
    // so the `addiu` really is the delay slot and the bytes match exactly.
    // Expected bytes are from `llvm-mc -triple=mips`.
    let out = text_for(
        "mips",
        ".set noreorder\nbeq $1, $2, x\naddiu $3, $3, 1\nx:\n",
    );
    assert_eq!(hex(&out), "10 22 00 01 24 63 00 01");
}

#[test]
fn set_reorder_is_refused_because_it_would_change_the_program() {
    let e = errors_for("mips", ".set reorder\nnop\n");
    assert!(e.contains("never fills delay slots"), "{e}");
}

#[test]
fn unknown_set_options_are_diagnosed_but_assignments_still_work() {
    let e = errors_for("mips", ".set bogus\n");
    assert!(e.contains("not an option"), "{e}");
    // `.set name, value` is still an assignment.
    assert_eq!(text_for("mips", ".set n, 7\n.byte n\n"), vec![7]);
}

/// The contents of the section named `name` in a little-endian ELF64 object.
fn elf64le_section(b: &[u8], name: &str) -> Vec<u8> {
    let u16at = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as usize;
    let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    let u64at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) as usize;
    let (shoff, shnum, shstrndx) = (u64at(0x28), u16at(0x3c), u16at(0x3e));
    let names = u64at(shoff + shstrndx * 64 + 0x18);
    (0..shnum)
        .map(|i| shoff + i * 64)
        .find(|&sh| {
            let at = names + u32at(sh);
            b[at..].starts_with(name.as_bytes()) && b[at + name.len()] == 0
        })
        .map(|sh| b[u64at(sh + 0x18)..u64at(sh + 0x18) + u64at(sh + 0x20)].to_vec())
        .unwrap_or_else(|| panic!("no section `{name}`"))
}

#[test]
fn n64_objects_use_rela_with_the_mips64el_info_layout() {
    // llvm-mc for `.quad ext+8` on mips64el: a `.rela.data` entry whose info
    // is the symbol as a little-endian 32-bit word followed by the bytes
    // `00 00 00 12` (R_MIPS_64 last), with addend 8 and a zero field.
    let asm = assemble_for("mips64el", ".data\n.quad ext+8\n");
    let elf = rsasm::output::elf::build(&asm).expect("ELF output");
    assert_eq!(hex(&section(&asm, ".data")), "00 00 00 00 00 00 00 00");
    let rela = elf64le_section(&elf, ".rela.data");
    assert_eq!(rela.len(), 24);
    assert_eq!(&rela[0..8], &[0; 8], "r_offset");
    assert_ne!(&rela[8..12], &[0; 4], "r_sym");
    assert_eq!(
        &rela[12..16],
        &[0, 0, 0, 0x12],
        "r_ssym, r_type3, r_type2, r_type"
    );
    assert_eq!(&rela[16..24], &[8, 0, 0, 0, 0, 0, 0, 0], "r_addend");
}

//! PowerPC encoding tests.
//!
//! Every expected byte string here was produced by a run of
//! `tools/mc-diff/run.sh powerpc powerpc64 powerpc64le`, which compares rsasm
//! against `llvm-mc`. These tests are the hermetic record of that agreement —
//! they can tell you that rsasm still encodes what it encoded last time, but
//! only the differential run can tell you it is *right*, so new expectations
//! belong in the corpus first.

#![cfg(feature = "powerpc")]

mod common;
use common::*;

/// Assembles each line on its own and checks its word. One line at a time
/// matters for branches: a bare number is an address, so the encoding of
/// `b 0x100` depends on where the instruction sits.
fn each(arch: &str, cases: &[(&str, &str)]) {
    for (src, want) in cases {
        let got = hex(&text_for(arch, &format!("{src}\n"))).replace(' ', "");
        assert_eq!(&got, want, "encoding `{src}` for {arch}");
    }
}

#[test]
fn fixed_point_arithmetic() {
    each(
        "powerpc64",
        &[
            ("add 3, 4, 5", "7c642a14"),
            ("add. 3, 4, 5", "7c642a15"),
            ("addo. 3, 4, 5", "7c642e15"),
            ("addc 3, 4, 5", "7c642814"),
            ("adde 3, 4, 5", "7c642914"),
            ("subf 3, 4, 5", "7c642850"),
            ("subfc 3, 4, 5", "7c642810"),
            ("subfe 3, 4, 5", "7c642910"),
            ("neg 3, 4", "7c6400d0"),
            ("nego. 3, 4", "7c6404d1"),
            ("mullw 3, 4, 5", "7c6429d6"),
            ("mulld 3, 4, 5", "7c6429d2"),
            ("mulhw 3, 4, 5", "7c642896"),
            ("divw 3, 4, 5", "7c642bd6"),
            ("divd 3, 4, 5", "7c642bd2"),
            ("addi 3, 4, -100", "3864ff9c"),
            ("addis 3, 4, 65535", "3c64ffff"),
            ("addic. 3, 4, 7", "34640007"),
            ("li 3, -1", "3860ffff"),
            ("lis 3, 4660", "3c601234"),
            ("la 3, 8(4)", "38640008"),
            // `subi` is `addi` with the immediate negated, so it lands on the
            // same word as `addi 3, 4, -100`.
            ("subi 3, 4, 100", "3864ff9c"),
            // `sub rD, rA, rB` is `subf rD, rB, rA`, with the operands swapped.
            ("sub 3, 4, 5", "7c652050"),
            ("mr 3, 4", "7c832378"),
            ("not 3, 4", "7c8320f8"),
            ("nop", "60000000"),
        ],
    );
}

#[test]
fn logical_and_sign_extension() {
    each(
        "powerpc64",
        &[
            ("and 3, 4, 5", "7c832838"),
            ("or 3, 4, 5", "7c832b78"),
            ("xor 3, 4, 5", "7c832a78"),
            ("nand 3, 4, 5", "7c832bb8"),
            ("nor 3, 4, 5", "7c8328f8"),
            ("eqv 3, 4, 5", "7c832a38"),
            ("andc 3, 4, 5", "7c832878"),
            ("orc 3, 4, 5", "7c832b38"),
            ("andi. 3, 4, 255", "708300ff"),
            ("andis. 3, 4, 255", "748300ff"),
            ("ori 3, 4, 65535", "6083ffff"),
            ("oris 3, 4, 255", "648300ff"),
            ("xori 3, 4, 255", "688300ff"),
            ("xoris 3, 4, 255", "6c8300ff"),
            ("extsb 3, 4", "7c830774"),
            ("extsh 3, 4", "7c830734"),
            ("extsw 3, 4", "7c8307b4"),
            ("cntlzw 3, 4", "7c830034"),
            ("cntlzd 3, 4", "7c830074"),
        ],
    );
}

#[test]
fn rotate_and_mask() {
    each(
        "powerpc64",
        &[
            ("rlwinm 3, 4, 5, 6, 7", "5483298e"),
            ("rlwinm. 3, 4, 5, 6, 7", "5483298f"),
            ("rlwimi 3, 4, 5, 6, 7", "5083298e"),
            ("rlwnm 3, 4, 5, 6, 7", "5c83298e"),
            ("rldicl 3, 4, 5, 6", "78832980"),
            // The interesting case: SH 40 and MB 33 both need their sixth bit,
            // and the two fields put it in different places.
            ("rldicl 3, 4, 40, 33", "78834062"),
            ("rldicr 3, 4, 40, 33", "78834066"),
            ("rldic. 3, 4, 40, 33", "7883406b"),
            ("rldimi 3, 4, 40, 33", "7883406e"),
            ("rldcl 3, 4, 5, 33", "78832870"),
            ("rldcr. 3, 4, 5, 33", "78832873"),
        ],
    );
}

#[test]
fn extended_rotate_mnemonics() {
    each(
        "powerpc64",
        &[
            ("slwi 3, 4, 5", "54832834"),
            ("srwi 3, 4, 5", "5483d97e"),
            ("clrlwi 3, 4, 5", "5483017e"),
            ("clrrwi 3, 4, 5", "54830034"),
            ("rotlwi 3, 4, 5", "5483283e"),
            ("sldi 3, 4, 5", "78832ea4"),
            ("srdi 3, 4, 5", "7883d942"),
            ("clrldi 3, 4, 5", "78830140"),
            ("clrrdi 3, 4, 5", "788306a4"),
            ("rotldi 3, 4, 5", "78832800"),
            ("extlwi 3, 4, 5, 6", "54833008"),
            ("extrwi 3, 4, 5, 6", "54835efe"),
            ("inslwi 3, 4, 5, 6", "5083d194"),
            ("insrwi 3, 4, 5, 6", "5083a994"),
            ("extldi 3, 4, 5, 6", "78833104"),
            ("extrdi 3, 4, 5, 6", "78835ee0"),
            ("insrdi 3, 4, 5, 6", "7883a98e"),
        ],
    );
    // The extended spellings really are the base instructions underneath.
    assert_eq!(
        text_for("powerpc64", "slwi 3, 4, 5\n"),
        text_for("powerpc64", "rlwinm 3, 4, 5, 0, 26\n")
    );
    assert_eq!(
        text_for("powerpc64", "srdi 3, 4, 5\n"),
        text_for("powerpc64", "rldicl 3, 4, 59, 5\n")
    );
}

#[test]
fn loads_and_stores() {
    each(
        "powerpc64",
        &[
            ("lbz 3, 8(4)", "88640008"),
            ("lhz 3, 8(4)", "a0640008"),
            ("lha 3, -8(4)", "a864fff8"),
            ("lwz 3, 8(4)", "80640008"),
            ("lwzu 3, 8(4)", "84640008"),
            ("lwz 3, -32768(31)", "807f8000"),
            // A base of 0 means "no base register", not r0.
            ("lwz 3, 0(0)", "80600000"),
            // DS-form: the low two bits of the displacement field are opcode.
            ("ld 3, 16(4)", "e8640010"),
            ("ldu 3, 16(4)", "e8640011"),
            ("lwa 3, 8(4)", "e864000a"),
            ("stb 3, 8(4)", "98640008"),
            ("sth 3, 8(4)", "b0640008"),
            ("stw 3, 8(4)", "90640008"),
            ("stwu 3, 8(4)", "94640008"),
            ("std 3, 16(4)", "f8640010"),
            ("stdu 3, 16(4)", "f8640011"),
            ("lmw 3, 8(4)", "b8640008"),
            ("stmw 3, 8(4)", "bc640008"),
            ("lwzx 3, 4, 5", "7c64282e"),
            ("lwzux 3, 4, 5", "7c64286e"),
            ("ldx 3, 4, 5", "7c64282a"),
            ("stwx 3, 4, 5", "7c64292e"),
            ("stdx 3, 4, 5", "7c64292a"),
        ],
    );
}

#[test]
fn branches() {
    each(
        "powerpc64",
        &[
            ("b 0", "48000000"),
            ("bl 0", "48000001"),
            ("ba 0x1000", "48001002"),
            ("bla 0x1000", "48001003"),
            ("bc 12, 0, 0", "41800000"),
            ("bca 12, 0, 0x100", "41800102"),
            ("bcl 12, 0, 0", "41800001"),
            // BO 12 branches when the CR bit is set, BO 4 when it is clear;
            // BI picks the bit within CR field 0 (lt, gt, eq, so).
            ("beq 0", "41820000"),
            ("bne 0", "40820000"),
            ("blt 0", "41800000"),
            ("bgt 0", "41810000"),
            ("ble 0", "40810000"),
            ("bge 0", "40800000"),
            // A CR field operand adds four to BI per field.
            ("beq 7, 0", "419e0000"),
            ("bne 2, 0", "408a0000"),
            ("bdnz 0", "42000000"),
            ("bdz 0", "42400000"),
            ("bt 5, 0", "41850000"),
            ("bf 5, 0", "40850000"),
            ("blr", "4e800020"),
            ("blrl", "4e800021"),
            ("bctr", "4e800420"),
            ("bctrl", "4e800421"),
            ("bclr 20, 0", "4e800020"),
            ("bcctr 20, 0", "4e800420"),
            ("beqlr", "4d820020"),
            ("beqlr 7", "4d9e0020"),
            ("beqctr", "4d820420"),
            ("bdnzlr", "4e000020"),
        ],
    );
}

#[test]
fn comparison_and_condition_register_logic() {
    each(
        "powerpc64",
        &[
            ("cmp 7, 1, 3, 4", "7fa32000"),
            ("cmpi 0, 0, 3, 100", "2c030064"),
            ("cmpl 0, 1, 3, 4", "7c232040"),
            ("cmpli 0, 1, 3, 100", "28230064"),
            // The extended spellings bake in the L bit and make the CR field
            // optional.
            ("cmpw 3, 4", "7c032000"),
            ("cmpw 7, 3, 4", "7f832000"),
            ("cmpwi 3, -1", "2c03ffff"),
            ("cmpd 3, 4", "7c232000"),
            ("cmpdi 3, 100", "2c230064"),
            ("cmplw 3, 4", "7c032040"),
            ("cmplwi 3, 65535", "2803ffff"),
            ("crand 3, 4, 5", "4c642a02"),
            ("cror 3, 4, 5", "4c642b82"),
            ("crxor 3, 4, 5", "4c642982"),
            ("crnot 3, 4", "4c642042"),
            ("crclr 3", "4c631982"),
            ("crset 3", "4c631a42"),
            ("mcrf 3, 4", "4d900000"),
        ],
    );
}

#[test]
fn system_and_special_registers() {
    each(
        "powerpc64",
        &[
            ("mfspr 3, 8", "7c6802a6"),
            ("mtspr 9, 3", "7c6903a6"),
            ("mflr 3", "7c6802a6"),
            ("mtlr 3", "7c6803a6"),
            ("mfctr 3", "7c6902a6"),
            ("mtctr 3", "7c6903a6"),
            ("mfxer 3", "7c6102a6"),
            ("mtxer 3", "7c6103a6"),
            ("mfcr 3", "7c600026"),
            ("mtcrf 255, 3", "7c6ff120"),
            ("sync", "7c0004ac"),
            ("lwsync", "7c2004ac"),
            ("isync", "4c00012c"),
            ("eieio", "7c0006ac"),
            ("sc", "44000002"),
            ("trap", "7fe00008"),
            ("tw 4, 3, 4", "7c832008"),
            ("twi 4, 3, 100", "0c830064"),
        ],
    );
    // The named SPRs are the same instruction as the numbered form.
    assert_eq!(
        text_for("powerpc64", "mfspr 3, lr\n"),
        text_for("powerpc64", "mflr 3\n")
    );
    assert_eq!(
        text_for("powerpc64", "mtspr ctr, 3\n"),
        text_for("powerpc64", "mtctr 3\n")
    );
}

#[test]
fn floating_point() {
    each(
        "powerpc64",
        &[
            ("lfs 1, 8(4)", "c0240008"),
            ("lfd 1, 8(4)", "c8240008"),
            ("stfs 1, 8(4)", "d0240008"),
            ("stfd 1, 8(4)", "d8240008"),
            ("fadd 1, 2, 3", "fc22182a"),
            ("fadd. 1, 2, 3", "fc22182b"),
            ("fadds 1, 2, 3", "ec22182a"),
            ("fsub 1, 2, 3", "fc221828"),
            // `fmul` takes FRC, not FRB, so its third operand lands in a
            // different field from `fadd`'s.
            ("fmul 1, 2, 3", "fc2200f2"),
            ("fmuls 1, 2, 3", "ec2200f2"),
            ("fdiv 1, 2, 3", "fc221824"),
            // `fmadd frD, frA, frC, frB`: the written order is not field order.
            ("fmadd 1, 2, 3, 4", "fc2220fa"),
            ("fmadds 1, 2, 3, 4", "ec2220fa"),
            ("fmr 1, 2", "fc201090"),
            ("fneg 1, 2", "fc201050"),
            ("fabs 1, 2", "fc201210"),
            ("frsp 1, 2", "fc201018"),
            ("fctiw 1, 2", "fc20101c"),
            ("fcmpu 0, 1, 2", "fc011000"),
        ],
    );
}

#[test]
fn registers_may_be_named_or_numbered() {
    let bare = text_for("powerpc64", "add 3, 4, 5\n");
    assert_eq!(text_for("powerpc64", "add r3, r4, r5\n"), bare);
    assert_eq!(text_for("powerpc64", "add %r3, %r4, %r5\n"), bare);
    assert_eq!(text_for("powerpc64", "add R3, R4, R5\n"), bare);

    let mem = text_for("powerpc64", "lwz 3, 8(4)\n");
    assert_eq!(text_for("powerpc64", "lwz r3, 8(r4)\n"), mem);

    let f = text_for("powerpc64", "fadd 1, 2, 3\n");
    assert_eq!(text_for("powerpc64", "fadd f1, f2, f3\n"), f);
    assert_eq!(text_for("powerpc64", "fadd fr1, fr2, fr3\n"), f);

    assert_eq!(
        text_for("powerpc64", "beq cr7, 0\n"),
        text_for("powerpc64", "beq 7, 0\n")
    );
    assert_eq!(
        text_for("powerpc64", "cmpw cr3, r4, r5\n"),
        text_for("powerpc64", "cmpw 3, 4, 5\n")
    );
}

#[test]
fn the_same_instructions_are_byte_reversed_between_the_two_byte_orders() {
    // The whole point of the endian seam: identical instruction words, and the
    // only difference is the order the four bytes of each reach memory.
    let src = "\
        add 3, 4, 5\n\
        lwz 3, 8(4)\n\
        rldicl 3, 4, 40, 33\n\
        beq 7, 0\n\
        blr\n";
    let be = text_for("powerpc64", src);
    let le = text_for("powerpc64le", src);
    assert_eq!(be.len(), le.len());
    assert_eq!(be.len() % 4, 0);
    for (b, l) in be.chunks(4).zip(le.chunks(4)) {
        let reversed: Vec<u8> = b.iter().rev().copied().collect();
        assert_eq!(reversed, l, "word {} is not byte-reversed", hex(b));
    }
    // 32-bit PowerPC is big-endian and encodes the same words as powerpc64.
    assert_eq!(text_for("powerpc", "add 3, 4, 5\n"), be[..4]);
}

#[test]
fn branch_displacements_are_measured_from_the_instruction() {
    let src = "\
start:\n\
        li 3, 0\n\
loop:   addi 3, 3, 1\n\
        cmpwi 3, 10\n\
        bne loop\n\
        b done\n\
        nop\n\
done:   blr\n";
    assert_eq!(
        hex(&text_for("powerpc64", src)),
        // `bne loop` is -8, `b done` is +8.
        "38 60 00 00 38 63 00 01 2c 03 00 0a 40 82 ff f8 48 00 00 08 60 00 00 00 4e 80 00 20"
    );
}

#[test]
fn alignment_padding_is_no_ops() {
    // Whole words of `ori 0, 0, 0`, with any sub-word remainder placed first,
    // since it can only be the tail of the partial word before it.
    let src = "\
        blr\n\
        .p2align 4\n\
        blr\n\
        .byte 1\n\
        .p2align 2\n\
        blr\n";
    assert_eq!(
        hex(&text_for("powerpc64", src)),
        "4e 80 00 20 60 00 00 00 60 00 00 00 60 00 00 00 \
         4e 80 00 20 01 00 00 00 4e 80 00 20"
    );
    // Little-endian padding is the same no-op written the other way round.
    assert_eq!(
        hex(&text_for("powerpc64le", "blr\n.p2align 3\nblr\n")),
        "20 00 80 4e 00 00 00 60 20 00 80 4e"
    );
}

#[test]
fn symbolic_operands_become_relocations() {
    // Nothing here can be resolved at assembly time, so each leaves a hole and
    // a relocation rather than an error.
    let src = "\
        bl ext\n\
        beq ext\n\
        lis 3, ext@ha\n\
        addi 3, 3, ext@l\n\
        lwz 4, ext@l(3)\n\
        ld 5, ext@l(3)\n";
    let asm = assemble_for("powerpc64", src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
    // R_PPC64_REL24, REL14, ADDR16_HA, ADDR16_LO, ADDR16_LO, ADDR16_LO_DS.
    assert_eq!(kinds, vec![10, 11, 6, 4, 4, 57]);
    // ELF puts a halfword relocation on the halfword, which on a big-endian
    // target is two bytes into the instruction.
    let offsets: Vec<u64> = asm.relocs.iter().map(|r| r.offset).collect();
    assert_eq!(offsets, vec![0, 4, 10, 14, 18, 22]);

    let le = assemble_for("powerpc64le", src);
    let le_offsets: Vec<u64> = le.relocs.iter().map(|r| r.offset).collect();
    assert_eq!(le_offsets, vec![0, 4, 8, 12, 16, 20]);
}

#[test]
fn relocation_modifiers_on_constants_are_applied() {
    each(
        "powerpc64",
        &[
            // `@ha` rounds up when the low half will be sign-extended negative.
            ("lis 3, 0x8000@ha", "3c600001"),
            ("lis 3, 0x12348000@ha", "3c601235"),
            ("lis 3, 0x12348000@h", "3c601234"),
            ("addi 3, 3, 0x12348000@l", "38638000"),
            ("ori 3, 3, 0x12348000@l", "60638000"),
            ("lwz 3, 0x12348000@l(4)", "80648000"),
            ("ld 3, 0x12348004@l(4)", "e8648004"),
        ],
    );
    assert!(errors_for("powerpc64", "li 3, 1 + 2@l\n").contains("whole operand"));
    assert!(errors_for("powerpc64", "li 3, 2@got\n").contains("not supported"));
    assert!(errors_for("powerpc64", "bl foo@plt\n").contains("not supported"));
}

#[test]
fn out_of_range_and_misaligned_values_are_diagnosed() {
    let cases = [
        ("li 3, 70000\n", "out of range"),
        ("addi 3, 4, -40000\n", "out of range"),
        ("ori 3, 4, -1\n", "out of range"),
        ("add 3, 4, 32\n", "out of range"),
        ("beq 8, 0\n", "must be 0 to 7"),
        ("rlwinm 3, 4, 32, 0, 31\n", "must be 0 to 31"),
        ("rldicl 3, 4, 64, 0\n", "must be 0 to 63"),
        ("slwi 3, 4, 32\n", "must be 0 to 31"),
        ("sldi 3, 4, 64\n", "must be 0 to 63"),
        ("extlwi 3, 4, 8, 28\n", "runs past the end"),
        ("extlwi 3, 4, 0, 0\n", "must be 1 to 32"),
        ("ld 3, 6(4)\n", "multiple of 4"),
        ("std 3, 2(4)\n", "multiple of 4"),
        ("lwz 3, 40000(4)\n", "must be -32768 to 32767"),
        ("addi 3, 4, 40000\n", "must be -32768 to 32767"),
        ("cmplwi 3, -1\n", "must be 0 to 65535"),
        ("mfspr 3, 2000\n", "must be 0 to 1023"),
        ("mtcrf 256, 3\n", "must be 0 to 255"),
        ("beq+ 0\n", "hints"),
    ];
    for (src, needle) in cases {
        let msg = errors_for("powerpc64", src);
        assert!(
            msg.contains(needle),
            "assembling `{}` should mention `{needle}`, got:\n{msg}",
            src.trim()
        );
    }
}

#[test]
fn out_of_range_branches_are_diagnosed() {
    // The I-form reaches +-32MB and the B-form +-32KB.
    let far = "b target\n.space 0x2000000\ntarget: blr\n";
    assert!(errors_for("powerpc64", far).contains("out of range"));
    let far_cond = "beq target\n.space 0x8000\ntarget: blr\n";
    assert!(errors_for("powerpc64", far_cond).contains("out of range"));
    // A target that is not word-aligned cannot be encoded at all.
    let odd = "b target\n.byte 0\ntarget: blr\n";
    assert!(errors_for("powerpc64", odd).contains("not a multiple of 4"));
}

#[test]
fn sixty_four_bit_instructions_are_rejected_in_thirty_two_bit_code() {
    for src in [
        "ld 3, 8(4)\n",
        "sldi 3, 4, 5\n",
        "cmpdi 3, 1\n",
        "rldicl 3, 4, 5, 6\n",
    ] {
        let msg = errors_for("powerpc", src);
        assert!(
            msg.contains("64-bit instruction"),
            "`{}` should be rejected in 32-bit code, got:\n{msg}",
            src.trim()
        );
    }
    // The same lines assemble happily for the 64-bit targets.
    for arch in ["powerpc64", "powerpc64le"] {
        text_for(arch, "ld 3, 8(4)\nsldi 3, 4, 5\ncmpdi 3, 1\n");
    }
}

#[test]
fn every_architecture_name_and_alias_resolves() {
    for name in [
        "powerpc",
        "ppc",
        "ppc32",
        "powerpc32",
        "powerpc64",
        "ppc64",
        "powerpc64le",
        "ppc64le",
    ] {
        assert!(
            rsasm::arch::lookup(name).is_some(),
            "`{name}` should name the PowerPC backend"
        );
    }
    assert_eq!(
        rsasm::arch::lookup("ppc64le").map(|a| a.name()),
        Some("powerpc64le")
    );
    assert_eq!(
        rsasm::arch::lookup("ppc").map(|a| a.elf_machine()),
        Some(20) // EM_PPC
    );
    assert_eq!(
        rsasm::arch::lookup("ppc64").map(|a| a.elf_machine()),
        Some(21) // EM_PPC64
    );
}

/// Malformed input must produce diagnostics, never a panic. The point is only
/// that the process survives; what each line reports is not fixed here.
#[test]
fn malformed_input_never_panics() {
    let cases = [
        "",
        "add",
        "add 3",
        "add 3,",
        "add ,",
        "add 3, 4",
        "add 3, 4, 5, 6",
        "add 3, 4, 5,",
        "add (",
        "add )",
        "add 3, 4, (",
        "add r99, r4, r5",
        "add cr0, cr1, cr2",
        "add lr, 4, 5",
        "fadd 3, r4, 5",
        "lwz 3, 8(",
        "lwz 3, 8)",
        "lwz 3, (4",
        "lwz 3, 4",
        "lwz 3",
        "lwz (4), 3",
        "ld 3, 8(4)(5)",
        "b",
        "b 1, 2",
        "b )",
        "bc 12",
        "bc 12, 0",
        "beq cr9, 0",
        "beq lr, 0",
        "blr 1",
        "nop 1",
        "mfspr",
        "mfspr 3",
        "mtcrf 3",
        "rlwinm 3, 4, 5",
        "rlwinm 3, 4, 5, 6",
        "rlwinm 3, 4, 5, 6, 7, 8",
        "rldicl 3, 4, 5",
        "slwi 3, 4",
        "extlwi 3, 4, 5",
        "extlwi 3, 4, undefined, 6",
        "li 3, undefined_symbol",
        "li 3",
        "sc 999",
        "crclr",
        "crclr 3, 4",
        "notaninstruction 1, 2",
        "add.. 3, 4, 5",
        "addoo 3, 4, 5",
        ".",
        "add 3, 4, 5 6",
        "add 3 4 5",
        "lwz 3, 8(4",
        "la 3, 4",
        "mr 3, 4, 5",
        "twi 4, 3",
        "mtfsf 1",
        "fmadd 1, 2, 3",
    ];
    for src in cases {
        // Both byte orders and both widths take the same path, but running
        // them all is cheap insurance.
        for arch in ["powerpc", "powerpc64", "powerpc64le"] {
            let _ = try_text_for(arch, &format!("{src}\n"));
        }
    }
}

#[test]
fn a_small_function_assembles_end_to_end() {
    let src = "\
        .text\n\
        .globl f\n\
f:      mflr 0\n\
        std 0, 16(1)\n\
        stdu 1, -32(1)\n\
        mr 31, 3\n\
        addi 3, 31, 1\n\
        addi 1, 1, 32\n\
        ld 0, 16(1)\n\
        mtlr 0\n\
        blr\n";
    let bytes = text_for("powerpc64", src);
    assert_eq!(bytes.len(), 36);
    assert_eq!(hex(&bytes[..4]), "7c 08 02 a6"); // mflr 0
    assert_eq!(hex(&bytes[32..]), "4e 80 00 20"); // blr
}

// ---- AltiVec, VSX and the POWER8-10 additions --------------------------------
//
// The bytes below were taken from llvm-mc runs, as the vector section of the
// tools/mc-diff corpora checks them, except where a comment names GNU as.

#[test]
fn altivec_instructions() {
    each(
        "powerpc64",
        &[
            ("vaddubm 2, 3, 4", "10432000"),
            ("vaddcuw 31, 0, 15", "13e07980"),
            ("vsubsbs 2, 3, 4", "10432700"),
            ("vmulouw 2, 3, 4", "10432088"),
            ("vavgsh 2, 3, 4", "10432542"),
            ("vcmpequb 2, 3, 4", "10432006"),
            ("vcmpequb. 2, 3, 4", "10432406"),
            ("vcmpgtfp. 30, 31, 0", "13df06c6"),
            ("vcmpbfp. 2, 3, 4", "104327c6"),
            ("vand 2, 3, 4", "10432404"),
            ("vxor 2, 2, 2", "104214c4"),
            ("vnor 2, 3, 3", "10431d04"),
            ("vmr 2, 3", "10431c84"),
            ("vnot 2, 3", "10431d04"),
            ("vslb 2, 3, 4", "10432104"),
            ("vrlq 2, 3, 4", "10432005"),
            ("vsldoi 2, 3, 4, 15", "104323ec"),
            ("vspltisb 2, -16", "1050030c"),
            ("vspltisw 31, 15", "13ef038c"),
            ("vspltb 2, 3, 15", "104f1a0c"),
            ("vpermxor 2, 3, 4, 5", "1043216d"),
            ("vmrghb 2, 3, 4", "1043200c"),
            ("vpkpx 2, 3, 4", "1043230e"),
            ("vupkhsb 2, 3", "10401a0e"),
            ("vsel 2, 3, 4, 5", "1043216a"),
            ("vmhaddshs 2, 3, 4, 5", "10432160"),
            ("lvx 2, 3, 4", "7c4320ce"),
            ("stvx 2, 3, 4", "7c4321ce"),
            ("mfvscr 2", "10400604"),
            ("mtvscr 3", "10001e44"),
            ("vpmsumd 2, 3, 4", "104324c8"),
            ("vcipher 2, 3, 4", "10432508"),
            ("vncipherlast 2, 3, 4", "10432549"),
            ("vsbox 2, 3", "104305c8"),
            ("vshasigmad 2, 3, 1, 6", "1043b6c2"),
            ("vpopcntd 2, 3", "10401fc3"),
            ("vclzb 2, 3", "10401f02"),
            ("vctzd 2, 3", "105f1e02"),
            ("vclzlsbb 3, 4", "10602602"),
            ("vextractub 2, 3, 15", "104f1a0d"),
            ("vinsertd 2, 3, 8", "10481bcd"),
            ("vcmpneb. 2, 3, 4", "10432407"),
            ("vcmpnezw 2, 3, 4", "10432187"),
            ("vmul10uq 2, 3", "10430201"),
            ("bcdadd. 2, 3, 4, 1", "10432601"),
            ("bcdsub. 2, 3, 4, 0", "10432441"),
            ("vextractbm 3, 4", "10682642"),
            ("vstribl. 2, 3", "10401c0d"),
            ("vinsblx 2, 3, 4", "1043220f"),
            ("vextdubvlx 2, 3, 4, 5", "10432158"),
            ("vgnb 3, 4, 2", "106224cc"),
            ("vdivesq 2, 3, 4", "1043230b"),
            ("mtvsrbmi 2, 65535", "105fffd5"),
        ],
    );
}

#[test]
fn vsx_instructions_carry_the_sixth_register_bit_below_the_opcode() {
    each(
        "powerpc64",
        &[
            ("xsadddp 1, 2, 3", "f0221900"),
            ("xsadddp 33, 34, 35", "f0221907"),
            ("xxlor 0, 32, 63", "f000fc96"),
            ("xxlor 63, 0, 31", "f3e0fc91"),
            ("xxlxor 40, 40, 40", "f10844d7"),
            ("xxland 1, 2, 3", "f0221c10"),
            ("xxlorc 1, 2, 3", "f0221d50"),
            ("xxleqv 60, 61, 62", "f39df5d7"),
            ("xxpermdi 1, 2, 3, 2", "f0221a50"),
            ("xxsldwi 1, 2, 3, 3", "f0221b10"),
            ("xxmrghd 1, 2, 3", "f0221850"),
            ("xxswapd 1, 2", "f0221250"),
            ("xxspltd 1, 2, 1", "f0221350"),
            ("xxspltw 60, 61, 3", "f383ea93"),
            ("xxsel 1, 2, 3, 4", "f0221930"),
            ("xxsel 63, 62, 61, 60", "f3feef3f"),
            ("xxspltib 3, 255", "f067fad0"),
            ("xxspltib 35, 128", "f06402d1"),
            ("xxbrd 1, 2", "f037176c"),
            ("xxbrq 33, 34", "f03f176f"),
            ("xsmaddadp 1, 2, 3", "f0221908"),
            ("xsmsubmdp 33, 34, 35", "f02219cf"),
            ("xscmpudp 7, 1, 2", "f3811118"),
            ("xscmpexpdp 0, 63, 33", "f01f09de"),
            ("xvcmpeqdp 1, 2, 3", "f0221b18"),
            ("xvcmpeqdp. 1, 2, 3", "f0221f18"),
            ("xvcmpgesp. 61, 62, 63", "f3befe9f"),
            ("xstsqrtdp 3, 45", "f18069aa"),
            ("xststdcsp 1, 2, 127", "f0ff14a8"),
            ("xvtstdcdp 63, 63, 127", "f3ffffef"),
            ("xscvdpsxds 1, 2", "f0201560"),
            ("xscvdpuxws 33, 34", "f0201123"),
            ("xvcvspsxws 1, 2", "f0201260"),
            ("xscvsxddp 40, 2", "f10015e1"),
            ("xsrdpi 1, 2", "f0201124"),
            ("xsiexpdp 1, 3, 4", "f023272c"),
            ("xsxexpdp 3, 33", "f0600d6e"),
            ("xsaddqp 2, 3, 4", "fc432008"),
            ("xsmaddqpo 30, 31, 0", "ffdf0309"),
            ("xscvqpdp 2, 3", "fc541e88"),
            ("xscvdpqp 2, 3", "fc561e88"),
            ("xsrqpi 1, 2, 3, 3", "fc411e0a"),
            ("xsrqpxp 0, 31, 30, 0", "ffe0f04a"),
            ("xscmpuqp 7, 2, 3", "ff821d08"),
            ("lxv 1, 16(3)", "f4230011"),
            ("lxv 63, -32768(31)", "f7ff8009"),
            ("stxv 35, 32752(4)", "f4647ffd"),
            ("lxsd 2, 8(4)", "e444000a"),
            ("stxssp 31, -4(3)", "f7e3ffff"),
            ("lxvx 1, 3, 4", "7c232218"),
            ("stxvl 63, 3, 4", "7fe3231b"),
            ("lxvdsx 33, 3, 4", "7c232299"),
            ("lxvp 34, 32(4)", "18640020"),
            ("stxvp 62, -16(4)", "1be4fff1"),
            ("mfvsrd 3, 34", "7c430067"),
            ("mtvsrd 63, 4", "7fe40167"),
            ("mfvsrwz 3, 2", "7c4300e6"),
            ("mtvsrwa 1, 4", "7c2401a6"),
            ("mtvsrdd 33, 3, 4", "7c232367"),
            ("mfvsrld 3, 35", "7c630267"),
            ("mtfprd 1, 3", "7c230166"),
            ("xxgenpcvbm 33, 2, 3", "f0231729"),
            ("xxextractuw 1, 2, 12", "f02c1294"),
            ("xxinsertw 1, 2, 12", "f02c12d4"),
            ("lxvkq 63, 31", "f3fffad1"),
            ("xxeval 1, 2, 3, 4, 5", "0500000588221910"),
            ("xxpermx 33, 34, 35, 36, 7", "050000078822190f"),
            ("xxblendvw 1, 2, 3, 4", "0500000084221920"),
            ("xxspltiw 3, 0x12345678", "0500123480665678"),
            ("xxspltidp 35, -1", "0500ffff8065ffff"),
            ("xxsplti32dx 34, 1, 0xdeadbeef", "0500dead8043beef"),
        ],
    );
}

#[test]
fn power9_and_power10_scalar_instructions() {
    each(
        "powerpc64",
        &[
            ("maddld 3, 4, 5, 6", "106429b3"),
            ("maddhd 3, 4, 5, 6", "106429b0"),
            ("maddhdu 3, 4, 5, 6", "106429b1"),
            ("darn 3, 1", "7c6105e6"),
            ("addpcis 3, 100", "4c720044"),
            ("addpcis 3, -32768", "4c608004"),
            ("subpcis 3, 100", "4c6eff84"),
            ("lnia 3", "4c600004"),
            ("cmprb 0, 1, 3, 4", "7c232180"),
            ("cmpeqb 7, 3, 4", "7f8321c0"),
            ("setb 3, 7", "7c7c0100"),
            ("setbc 3, 5", "7c650300"),
            ("setnbcr 3, 31", "7c7f03c0"),
            ("modsd 3, 4, 5", "7c642e12"),
            ("modud 3, 4, 5", "7c642a12"),
            ("modsw 3, 4, 5", "7c642e16"),
            ("moduw 3, 4, 5", "7c642a16"),
            ("extswsli 3, 4, 63", "7c83fef6"),
            ("extswsli. 3, 4, 5", "7c832ef5"),
            ("cnttzw 3, 4", "7c830434"),
            ("cnttzd. 3, 4", "7c830475"),
            ("copy 3, 4", "7c23260c"),
            ("paste. 3, 4", "7c23270d"),
            ("paste. 3, 4, 0", "7c03270d"),
            ("brd 3, 4", "7c830176"),
            ("brw 3, 4", "7c830136"),
            ("brh 3, 4", "7c8301b6"),
            ("cfuged 3, 4, 5", "7c8329b8"),
            ("pdepd 3, 4, 5", "7c832938"),
            ("pextd 3, 4, 5", "7c832978"),
            ("cntlzdm 3, 4, 5", "7c832876"),
            ("cnttzdm 3, 4, 5", "7c832c76"),
            ("addex 3, 4, 5, 0", "7c642954"),
            ("mffsl 3", "fc78048e"),
            ("mffscrni 3, 3", "fc771c8e"),
            ("mffscdrn 3, 4", "fc74248e"),
            ("dst 3, 4, 1", "7c2322ac"),
            ("dss 3", "7c60066c"),
            ("dssall", "7e00066c"),
        ],
    );
}

#[test]
fn prefixed_instructions_are_two_words_each_in_the_target_byte_order() {
    each(
        "powerpc64",
        &[
            ("paddi 3, 4, 100, 0", "0600000038640064"),
            ("paddi 3, 0, 100, 1", "0610000038600064"),
            ("paddi 3, 4, -8589934592", "0602000038640000"),
            ("pli 3, 8589934591", "0601ffff3860ffff"),
            ("pla 3, 100(4)", "0600000038640064"),
            ("psubi 3, 4, 100", "0603ffff3864ff9c"),
            ("pld 3, 100(4), 0", "04000000e4640064"),
            ("pld 3, -100(0), 1", "0413ffffe460ff9c"),
            ("pstd 31, 8(3), 0", "04000000f7e30008"),
            ("plwa 3, 8(4), 0", "04000000a4640008"),
            ("plwz 3, 8(4), 0", "0600000080640008"),
            ("pstb 3, 8(4), 0", "0600000098640008"),
            ("plfd 1, 8(4), 0", "06000000c8240008"),
            ("pstfs 31, 8(4), 0", "06000000d3e40008"),
            ("plxsd 2, 8(4), 0", "04000000a8440008"),
            ("pstxssp 2, 8(4), 0", "04000000bc440008"),
            ("plxv 35, 8(4), 0", "04000000cc640008"),
            ("pstxv 63, 8(4), 0", "04000000dfe40008"),
            ("plxvp 34, 8(4), 0", "04000000e8640008"),
            ("pstxvp 2, 8(4), 0", "04000000f8440008"),
        ],
    );
    each(
        "powerpc64le",
        &[
            ("paddi 3, 4, 100, 0", "0000000664006438"),
            ("paddi 3, 0, 100, 1", "0000100664006038"),
            ("paddi 3, 4, -8589934592", "0000020600006438"),
            ("pli 3, 8589934591", "ffff0106ffff6038"),
            ("pla 3, 100(4)", "0000000664006438"),
            ("psubi 3, 4, 100", "ffff03069cff6438"),
            ("pld 3, 100(4), 0", "00000004640064e4"),
            ("pld 3, -100(0), 1", "ffff13049cff60e4"),
            ("pstd 31, 8(3), 0", "000000040800e3f7"),
            ("plwa 3, 8(4), 0", "00000004080064a4"),
            ("plwz 3, 8(4), 0", "0000000608006480"),
            ("pstb 3, 8(4), 0", "0000000608006498"),
            ("plfd 1, 8(4), 0", "00000006080024c8"),
            ("pstfs 31, 8(4), 0", "000000060800e4d3"),
            ("plxsd 2, 8(4), 0", "00000004080044a8"),
            ("pstxssp 2, 8(4), 0", "00000004080044bc"),
            ("plxv 35, 8(4), 0", "00000004080064cc"),
            ("pstxv 63, 8(4), 0", "000000040800e4df"),
            ("plxvp 34, 8(4), 0", "00000004080064e8"),
            ("pstxvp 2, 8(4), 0", "00000004080044f8"),
        ],
    );
}

#[test]
fn forms_only_gnu_as_accepts_are_accepted() {
    // Taken from GNU as 2.47 with -mfuture; llvm-mc refuses each of these.
    each(
        "powerpc64",
        &[
            ("xxmr 1, 63", "f03ffc96"),
            ("xxlnot 40, 2", "f1021511"),
            ("vcfpsxws 2, 3, 31", "105f1bca"),
            ("fmrgew 1, 2, 3", "fc221f8c"),
            ("pnop", "0700000000000000"),
            ("pla 3, -100(0), 1", "0613ffff3860ff9c"),
            ("xxspltib 3, -128", "f06402d0"),
            ("mtvsrbmi 2, -1", "105fffd5"),
            ("subpcis 3, 32768", "4c608004"),
        ],
    );
}

#[test]
fn vector_registers_may_be_named_or_numbered() {
    let v = text_for("powerpc64", "vaddubm 2, 3, 4\n");
    assert_eq!(text_for("powerpc64", "vaddubm v2, v3, v4\n"), v);
    assert_eq!(text_for("powerpc64", "vaddubm %v2, %v3, %v4\n"), v);
    let vs = text_for("powerpc64", "xxlor 1, 40, 63\n");
    assert_eq!(text_for("powerpc64", "xxlor vs1, vs40, vs63\n"), vs);
    assert_eq!(text_for("powerpc64", "xxlor %vs1, %VS40, %vs63\n"), vs);
    assert_eq!(
        text_for("powerpc64", "lxv vs33, 16(r3)\n"),
        text_for("powerpc64", "lxv 33, 16(3)\n")
    );
    // A name from another bank is refused, where both references read its
    // number: `v2` in a VSX slot is VSX register 34, not 2, and `vs40` has no
    // vector-register spelling at all.
    for (src, needle) in [
        ("xxlor v2, 3, 4\n", "expected a VSX register"),
        ("vaddubm vs2, 3, 4\n", "is not a vector register"),
        ("mfvsrd vs3, 4\n", "is not a general-purpose register"),
        ("xsadddp f1, 2, 3\n", "expected a VSX register"),
    ] {
        let msg = errors_for("powerpc64", src);
        assert!(msg.contains(needle), "`{}`: {msg}", src.trim());
    }
}

#[test]
fn vector_operands_are_range_checked() {
    let cases = [
        ("xxlor 64, 0, 0\n", "must be 0 to 63"),
        ("vaddubm 32, 0, 0\n", "must be 0 to 31"),
        ("vspltisb 2, 16\n", "must be -16 to 15"),
        ("xxspltib 2, 256\n", "must be -128 to 255"),
        ("vsldoi 2, 3, 4, 16\n", "must be 0 to 15"),
        ("xxpermdi 1, 2, 3, 4\n", "must be 0 to 3"),
        ("xxspltd 1, 2, 2\n", "must be 0 or 1"),
        ("lxv 3, 8(4)\n", "multiple of 16"),
        ("lxsd 3, 6(4)\n", "multiple of 4"),
        ("lxvp 3, 16(4)\n", "must be even-numbered"),
        ("addpcis 3, 65536\n", "must be -32768 to 65535"),
        ("pli 3, 8589934592\n", "out of range"),
        ("xxspltiw 3, 8589934592\n", "out of range"),
        ("vcmpequbo 2, 3, 4\n", "unknown instruction"),
        ("vaddubm. 2, 3, 4\n", "unknown instruction"),
    ];
    for (src, needle) in cases {
        let msg = errors_for("powerpc64", src);
        assert!(
            msg.contains(needle),
            "assembling `{}` should mention `{needle}`, got:\n{msg}",
            src.trim()
        );
    }
}

#[test]
fn the_r_operand_has_to_agree_with_the_displacement() {
    // R says the displacement is from the instruction, which leaves no room
    // for a base register; both references refuse the pair.
    assert!(errors_for("powerpc64", "paddi 3, 4, 100, 1\n").contains("base register is 0"));
    assert!(errors_for("powerpc64", "pld 3, 8(4), 1\n").contains("base register is 0"));
    // A PC-relative value in a field the instruction reads relative to `rA`
    // would link to nonsense. GNU as writes it; llvm-mc refuses it.
    assert!(errors_for("powerpc64", "pld 3, x@pcrel(0)\n").contains("R operand"));
    assert!(errors_for("powerpc64", "paddi 3, 0, x@pcrel, 0\n").contains("R operand"));
}

#[test]
fn a_prefixed_instruction_is_never_split_across_a_64_byte_boundary() {
    // A no-op goes in front of a prefixed instruction whose suffix would start
    // a new 64-byte block, and the section is aligned to 64. A label on a line
    // of its own stays on the padding; one on the instruction's line moves past
    // it, so `b lab` reaches back 12 bytes and `b lab3` 8. Bytes from llvm-mc,
    // and GNU as agrees.
    let src = "\
        .fill 15, 4, 0x60000000\n\
lab:\n\
        paddi 3, 4, 100, 0\n\
        b lab\n\
        .fill 11, 4, 0x60000000\n\
lab2:   pli 3, 1\n\
        b lab2\n\
        .fill 14, 4, 0x60000000\n\
lab3:   pli 3, 2\n\
        b lab3\n";
    let nops = |n: usize| vec!["60000000"; n].join("");
    let want = format!(
        "{}06000000386400644bfffff4{}06000000386000014bfffff8{}06000000386000024bfffff8",
        nops(16),
        nops(11),
        nops(15)
    );
    assert_eq!(hex(&text_for("powerpc64", src)).replace(' ', ""), want);
    let asm = assemble_for("powerpc64", src);
    assert_eq!(asm.sections[0].align, 64);
    // Without a prefixed instruction, `.text` keeps llvm-mc's 4.
    assert_eq!(assemble_for("powerpc64", "nop\n").sections[0].align, 4);
}

#[test]
fn prefixed_references_become_34_bit_relocations() {
    let src = "\
        pld 3, ext@pcrel(0), 1\n\
        paddi 3, 0, ext@pcrel+8, 1\n\
        pld 4, ext@got@pcrel(0), 1\n\
        paddi 3, 4, ext, 0\n\
        lxv 35, ext@l(4)\n\
        stxsd 2, ext(4)\n";
    for arch in ["powerpc64", "powerpc64le"] {
        let asm = assemble_for(arch, src);
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
        // R_PPC64_PCREL34 twice, GOT_PCREL34, D34 (from GNU as: llvm-mc cannot
        // write it), ADDR16_LO_DS and ADDR16_DS, the last two on the halfword.
        assert_eq!(kinds, vec![132, 132, 133, 128, 57, 56], "{arch}");
        let offsets: Vec<u64> = asm.relocs.iter().map(|r| r.offset).collect();
        let half = if arch == "powerpc64" { 2 } else { 0 };
        assert_eq!(offsets, vec![0, 8, 16, 24, 32 + half, 36 + half], "{arch}");
    }
    // llvm-mc leaves a PC-relative prefixed reference to a local label to the
    // linker too, since the linker may rewrite the instruction.
    let local = assemble_for("powerpc64", "paddi 3, 0, 1f@pcrel, 1\n1: blr\n");
    assert_eq!(local.relocs.len(), 1);
    // In 32-bit code the DS relocations do not exist: GNU as writes the plain
    // halfword ones, R_PPC_ADDR16 and R_PPC_ADDR16_LO, and a 34-bit field
    // cannot be relocated at all.
    let asm = assemble_for("powerpc", "lxsd 3, ext(4)\nlxv 35, ext@l(4)\n");
    let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, vec![3, 4]);
    assert!(errors_for("powerpc", "xxspltiw 3, 1\npstw 3, ext(4), 0\n").contains("64-bit object"));
}

#[test]
fn doubleword_vector_and_scalar_additions_are_64_bit_only() {
    for src in [
        "maddld 3, 4, 5, 6\n",
        "mfvsrd 3, 34\n",
        "pld 3, 8(4), 0\n",
        "extswsli 3, 4, 5\n",
    ] {
        let msg = errors_for("powerpc", src);
        assert!(
            msg.contains("64-bit instruction"),
            "`{}`: {msg}",
            src.trim()
        );
    }
    // The word-sized and vector ones are fine in 32-bit code.
    text_for(
        "powerpc",
        "modsw 3, 4, 5\nmfvsrwz 3, 34\nxxlor 1, 2, 3\nplwz 3, 8(4), 0\n",
    );
}

#[test]
fn malformed_vector_input_never_panics() {
    let cases = [
        "xxlor",
        "xxlor 1, 2",
        "xxlor 1, 2, 3, 4",
        "xxlor r1, v2, vs3",
        "xxlor 1, 2, undefined",
        "vaddubm 1, 2, (3)",
        "lxv 3, 8",
        "lxv 3, (4)",
        "lxv 3, sym@pcrel(4)",
        "lxvp 63, 0(4)",
        "pld 3, 8(4)(5), 0",
        "pld 3, 8, 1",
        "pld 3",
        "paddi 3, 4",
        "paddi 3, 4, sym@got, 1",
        "pli 3, sym@pcrel",
        "psubi 3, 4, sym",
        "xxspltiw 3",
        "xxspltiw 3, sym",
        "xxeval 1, 2, 3, 4",
        "xxeval 1, 2, 3, 4, 256",
        "vcmpequb.. 2, 3, 4",
        "bcdadd 2, 3, 4, 0",
        "pnop 1",
        "paste. 3",
        "paste. 3, 4, 2",
        "addpcis 3, sym",
        "xvtstdcsp 1, 2, 128",
        "vshasigmad 2, 3, 2, 0",
    ];
    for src in cases {
        for arch in ["powerpc", "powerpc64", "powerpc64le"] {
            let _ = try_text_for(arch, &format!("{src}\n"));
        }
    }
}

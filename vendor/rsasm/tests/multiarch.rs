//! Source that switches targets with `.arch`.
//!
//! No reference assembler reads such a file, so every expected byte string
//! here came out of `tools/multiarch-diff/run.sh -v`, which splits the
//! program at its `.arch` lines, assembles each part with that target's
//! reference (GNU as or llvm-mc) and concatenates the code. Where a test
//! moves a switch into a macro, a conditional or an included file, its bytes
//! are those of the same program with the switches at the top level, which
//! the corpus pairs it with.

#![cfg(all(feature = "x86", feature = "m68k"))]

mod common;
use common::*;

/// The code `src` assembles to, starting as `arch`.
#[track_caller]
fn code(arch: &str, src: &str) -> String {
    hex(&text_for(arch, src))
}

// ---- lexing -------------------------------------------------------------------

#[test]
fn the_rest_of_the_file_is_read_by_the_new_targets_rules() {
    // `#` is an x86 comment and an m68k immediate; `|` the reverse.
    let src = "        movq    $1, %rax        # an x86 comment
        ret
.arch m68k
        movew   #1, %d0         | an m68k comment
        moveq   #-1, %d1
        rts
";
    assert_eq!(
        code("x86-64", src),
        "48 c7 c0 01 00 00 00 c3 30 3c 00 01 72 ff 4e 75"
    );
    let src = "        movel   #0x12345678, %d0
.arch x86-64
        movl    $0x12345678, %eax       # the same number, little-endian
        movl    $1 | 2, %ebx
";
    assert_eq!(
        code("m68k", src),
        "20 3c 12 34 56 78 b8 78 56 34 12 bb 03 00 00 00"
    );
}

#[test]
fn the_switching_line_is_read_by_the_old_rules() {
    let src = "        nop\n.arch m68k # this comment is x86's\n        moveq   #1, %d0\n";
    assert_eq!(code("x86-64", src), "90 70 01");
}

#[cfg(feature = "superh")]
#[test]
fn superh_comments_between_x86_code() {
    let src = "        movl    $7, %eax        # x86
.arch sh
# a first-column comment
        mov     #7, r0          ! an SH comment
        rts
        nop
.arch x86-64
        ret
";
    assert_eq!(code("x86-64", src), "b8 07 00 00 00 e0 07 00 0b 00 09 c3");
}

#[cfg(all(feature = "arm", feature = "aarch64"))]
#[test]
fn arm_and_aarch64_comments() {
    let src = "        movl    $1, %eax        # x86
.arch arm
        mov     r0, #1          @ arm
        bx      lr
.arch aarch64
        mov     x0, #1          // aarch64
        ret
";
    assert_eq!(
        code("x86-64", src),
        "b8 01 00 00 00 01 00 a0 e3 1e ff 2f e1 20 00 80 d2 c0 03 5f d6"
    );
}

#[cfg(feature = "rl78")]
#[test]
fn rl78_numbers_only_where_rl78_is_the_target() {
    let src = "        movl    $0x10, %eax
.arch rl78
        mov     a, #10H
        movw    ax, #1234H
        ret
.arch x86-64
        movl    $0x10, %eax
";
    assert_eq!(
        code("x86-64", src),
        "b8 10 00 00 00 51 10 30 34 12 d7 b8 10 00 00 00"
    );
}

#[test]
fn names_that_are_not_one_token() {
    let src = "        nop\n.arch 68000\n        moveq   #1, %d0\n.arch x86-64\n        nop\n";
    assert_eq!(code("x86-64", src), "90 70 01 90");
    let asm = assemble_for("m68k", ".arch x86-64\n");
    assert_eq!(asm.arch.name(), "x86-64");
    #[cfg(feature = "k78")]
    {
        let asm = assemble_for("x86-64", ".arch 78k0\n");
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        assert_eq!(asm.arch.name(), "78k0");
    }
}

// ---- where the switch is ------------------------------------------------------

#[test]
fn a_macro_body_is_read_by_the_rules_where_it_is_expanded() {
    let src = ".macro load_one
        movew   #1, %d0         | m68k
.endm
        movq    $1, %rax        # x86
.arch m68k
        load_one
        rts
";
    assert_eq!(
        code("x86-64", src),
        "48 c7 c0 01 00 00 00 30 3c 00 01 4e 75"
    );
}

#[test]
fn a_switch_in_a_macro_applies_to_the_rest_of_the_file() {
    let src = ".macro to_m68k
        .arch   m68k
.endm
        nop                     # x86
        to_m68k
        moveq   #3, %d0         | m68k
        rts
";
    assert_eq!(code("x86-64", src), "90 70 03 4e 75");
}

#[test]
fn a_switch_in_a_macro_applies_to_the_rest_of_the_expansion() {
    let src = ".macro m68k_then_back n
        .arch   m68k
        moveq   #\\n, %d0        | m68k
        .arch   x86-64
.endm
        m68k_then_back 5
        movl    $5, %eax        # x86
";
    assert_eq!(code("x86-64", src), "70 05 b8 05 00 00 00");
}

#[test]
fn a_switch_in_a_false_conditional_does_not_happen() {
    let src = "        .if 0
        .arch   m68k
        .endif
        movl    $1, %eax        # still x86
        .if 1
        .arch   m68k
        .else
        .arch   sh
        .endif
        moveq   #1, %d0         | m68k
";
    assert_eq!(code("x86-64", src), "b8 01 00 00 00 70 01");
}

#[test]
fn a_switch_in_a_repeat_block_applies_from_where_it_is() {
    let src = "        .rept   2
        .arch   m68k
        moveq   #2, %d0         | m68k
        .arch   x86-64
        movl    $2, %eax        # x86
        .endr
";
    assert_eq!(
        code("x86-64", src),
        "70 02 b8 02 00 00 00 70 02 b8 02 00 00 00"
    );
}

#[test]
fn included_files_follow_the_switch_both_ways() {
    // The same program as the first test, with part of it in another file.
    let dir = std::env::temp_dir().join(format!("rsasm-multiarch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let body = dir.join("body.s");
    let switch = dir.join("switch.s");
    std::fs::write(
        &body,
        "        movew   #1, %d0         | an m68k comment\n        moveq   #-1, %d1\n",
    )
    .unwrap();
    std::fs::write(&switch, "# still x86 here\n.arch m68k\n").unwrap();
    // A quoted file name takes escapes, so a Windows path's backslashes are
    // doubled.
    let quoted = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let head = "        movq    $1, %rax        # an x86 comment\n        ret\n";
    let included = code(
        "x86-64",
        &format!(
            "{head}.arch m68k\n        .include \"{}\"\n        rts\n",
            quoted(&body)
        ),
    );
    let switched = code(
        "x86-64",
        &format!(
            "{head}        .include \"{}\"\n        movew   #1, %d0 | m68k\n        moveq   #-1, %d1\n        rts\n",
            quoted(&switch)
        ),
    );
    std::fs::remove_dir_all(&dir).unwrap();
    let want = "48 c7 c0 01 00 00 00 c3 30 3c 00 01 72 ff 4e 75";
    assert_eq!(included, want);
    assert_eq!(switched, want);
}

// ---- output ---------------------------------------------------------------------

#[cfg(feature = "mips")]
#[test]
fn data_takes_the_byte_order_of_its_target() {
    let src = "        .long   0x11223344
        .word   0x5566
.arch x86-64
        .long   0x11223344
        .word   0x5566
.arch mips
        .word   0x11223344
.arch mipsel
        .word   0x11223344
";
    assert_eq!(
        code("m68k", src),
        "11 22 33 44 55 66 44 33 22 11 66 55 11 22 33 44 44 33 22 11"
    );
}

#[cfg(feature = "superh")]
#[test]
fn fixups_resolved_at_the_end_take_the_byte_order_of_their_code() {
    // Resolved once the whole file is read, when the active target is SH.
    let src = "        call    1f
        nop
1:      .long   2f - 1b
2:      ret
        nop
.arch m68k
        bsr     3f
        nop
3:      .long   4f - 3b
4:      rts
.arch sh
        bsr     5f
        nop
        .align  2
5:      .long   6f - 5b
6:      rts
        nop
";
    assert_eq!(
        code("x86-64", src),
        "e8 01 00 00 00 90 04 00 00 00 c3 90 61 00 00 04 4e 71 00 00 00 04 4e 75 \
         b0 00 00 09 00 00 00 04 00 0b 00 09"
    );
    let src = "        mov     #1, r0
        rts
        nop
        .align  2
        .long   7f - .
7:
.arch shl
        mov     #1, r0
        rts
        nop
        .align  2
        .long   8f - .
8:
";
    assert_eq!(
        code("sh", src),
        "e0 01 00 0b 00 09 00 09 00 00 00 04 01 e0 0b 00 09 00 09 00 04 00 00 00"
    );
}

#[test]
fn i386_and_x86_64_code_in_one_file() {
    let src = "        movl    (%eax), %ebx
.arch i386
        movl    (%eax), %ebx
.arch x86-64
        movl    (%eax), %ebx
";
    assert_eq!(code("x86-64", src), "67 8b 18 8b 18 67 8b 18");
}

#[cfg(all(feature = "aarch64", feature = "superh", feature = "riscv"))]
#[test]
fn alignment_is_padded_with_the_no_ops_of_the_code_before_it() {
    let src = "        mov     x0, #1
        .p2align 3
.arch m68k
        moveq   #1, %d0
        .p2align 2
.arch sh
        mov     #1, r0
        .p2align 2
.arch riscv64
        addi    a0, a0, 1
        .p2align 3
";
    assert_eq!(
        code("aarch64", src),
        "20 00 80 d2 1f 20 03 d5 70 01 00 00 e0 01 00 09 05 05 01 00 13 00 00 00"
    );
}

#[test]
fn the_object_is_for_the_target_assembly_started_with() {
    // An x86-64 file that ends in m68k code is still an ELF64, little-endian
    // x86-64 object.
    let asm = assemble_for("x86-64", "nop\n.arch m68k\nmoveq #1,%d0\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let elf = rsasm::output::elf::build(&asm).expect("ELF output");
    assert_eq!(elf[4], 2, "ELFCLASS64");
    assert_eq!(elf[5], 1, "ELFDATA2LSB");
    assert_eq!(u16::from_le_bytes([elf[18], elf[19]]), 62, "EM_X86_64");
    assert_eq!(asm.target().name(), "x86-64");
}

#[test]
fn another_targets_code_cannot_be_relocated() {
    let e = errors_for("x86-64", "nop\n.arch m68k\n.long foo\n");
    assert!(
        e.contains(
            "in code for `m68k`, needs a relocation, which an object for `x86-64` cannot hold"
        ),
        "{e}"
    );
    // The same machine in the same byte order and class can.
    let asm = assemble_for("m68k", "nop\n.arch 68000\n.long foo\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(asm.relocs.len(), 1);
}

#[cfg(feature = "superh")]
#[test]
fn superh_e_flags_count_the_code_before_a_switch() {
    fn e_flags(src: &str) -> u32 {
        let asm = assemble_for("sh", src);
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        let elf = rsasm::output::elf::build(&asm).expect("ELF output");
        u32::from_be_bytes(elf[0x24..0x28].try_into().unwrap())
    }
    // `sh-elf-as` marks `ldtlb` with `movca.l` 0x10 (tests/superh.rs), which
    // neither is on its own. A switch to the same machine, directly or by way
    // of another, does not forget the first.
    assert_eq!(e_flags("ldtlb\n.arch sh\nmovca.l r0,@r1"), 0x10);
    assert_eq!(
        e_flags("ldtlb\n.arch x86-64\nnop\n.arch sh\nmovca.l r0,@r1"),
        0x10
    );
    // And a file that ends in another target's code is still described by
    // its SH code: `fipr fv8,fv4` alone is 0x9.
    assert_eq!(e_flags("fipr fv8,fv4\n.arch x86-64\nnop"), 0x9);
}

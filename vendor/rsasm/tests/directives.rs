//! Directive behaviour, including the error cases.

#![cfg(feature = "x86")]

mod common;
use common::*;

#[test]
fn data_directives_respect_width_and_endianness() {
    assert_eq!(text(".byte 1, 2, 0xff"), vec![1, 2, 0xff]);
    assert_eq!(text(".short 0x1234"), vec![0x34, 0x12]);
    assert_eq!(text(".long 0x11223344"), vec![0x44, 0x33, 0x22, 0x11]);
    assert_eq!(
        text(".quad 0x1122334455667788"),
        vec![0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
    );
    // `.word` is two bytes on x86, matching GNU as.
    assert_eq!(text(".word 1").len(), 2);
    // Negative values fill the field.
    assert_eq!(text(".byte -1"), vec![0xff]);
    assert_eq!(text(".long -1"), vec![0xff; 4]);
}

#[test]
fn string_directives() {
    assert_eq!(text(r#".ascii "hi""#), b"hi".to_vec());
    assert_eq!(text(r#".asciz "hi""#), b"hi\0".to_vec());
    assert_eq!(text(r#".string "a\nb""#), b"a\nb\0".to_vec());
    assert_eq!(text(r#".ascii "a", "b""#), b"ab".to_vec());
}

#[test]
fn leb128_encoding() {
    assert_eq!(text(".uleb128 0"), vec![0]);
    assert_eq!(text(".uleb128 624485"), vec![0xe5, 0x8e, 0x26]);
    assert_eq!(text(".sleb128 -2"), vec![0x7e]);
    assert_eq!(text(".sleb128 -127"), vec![0x81, 0x7f]);
}

#[test]
fn space_fill_and_zero() {
    assert_eq!(text(".space 4"), vec![0; 4]);
    assert_eq!(text(".space 3, 0xaa"), vec![0xaa; 3]);
    assert_eq!(text(".zero 2"), vec![0, 0]);
    // `.fill count, size, value` writes `count` items of `size` bytes.
    assert_eq!(
        text(".fill 3, 2, 0x4142"),
        vec![0x42, 0x41, 0x42, 0x41, 0x42, 0x41]
    );
    assert_eq!(text(".fill 4"), vec![0; 4]);
}

#[test]
fn alignment() {
    // An explicit fill byte is honoured.
    assert_eq!(
        text(".byte 1\n.p2align 3, 0xcc\n.byte 2"),
        vec![1, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 2]
    );
    // `.align` counts bytes on x86, not powers of two.
    assert_eq!(text(".byte 1\n.align 2, 0\n.byte 2"), vec![1, 0, 2]);
    // Already-aligned means no padding.
    assert_eq!(text(".short 1\n.balign 2, 0\n.byte 2"), vec![1, 0, 2]);
    // The maximum-skip form gives up rather than pad too far.
    assert_eq!(text(".byte 1\n.balign 8, 0, 2\n.byte 2"), vec![1, 2]);
    assert_eq!(
        text(".byte 1\n.balign 4, 0, 8\n.byte 2"),
        vec![1, 0, 0, 0, 2]
    );
    // A data section pads with zeroes by default.
    let asm = assemble(".data\n.byte 1\n.balign 4\n.byte 2");
    assert_eq!(section(&asm, ".data"), vec![1, 0, 0, 0, 2]);
}

#[test]
fn executable_sections_pad_with_no_ops() {
    // Padding in `.text` has to stay executable, so it is filled with real
    // no-ops rather than zeroes.
    let out = text("nop\n.balign 8\nret");
    assert_eq!(out.len(), 9);
    assert_eq!(out[0], 0x90);
    assert_eq!(out[8], 0xc3);
    // The 7-byte no-op is `0f 1f 80 00 00 00 00`; what matters is that the
    // padding starts an instruction rather than being a run of zero bytes,
    // which would decode as `add %al, (%rax)`.
    assert!(
        matches!(out[1], 0x90 | 0x66 | 0x0f),
        "padding should start a no-op: {out:02x?}"
    );
}

#[test]
fn org_moves_the_location_counter() {
    assert_eq!(text(".byte 1\n.org 4\n.byte 2"), vec![1, 0, 0, 0, 2]);
    assert_eq!(
        text(".byte 1\n.org 4, 0xff\n.byte 2"),
        vec![1, 0xff, 0xff, 0xff, 2]
    );
    assert_eq!(text(".byte 1\n. = . + 3\n.byte 2"), vec![1, 0, 0, 0, 2]);
    assert!(errors(".byte 1\n.byte 2\n.org 1").contains("cannot move backwards"));
}

#[test]
fn set_and_expressions() {
    assert_eq!(text(".set n, 4\n.byte n, n*n"), vec![4, 16]);
    assert_eq!(text(".equ n, 1+2*3\n.byte n"), vec![7]);
    assert_eq!(text("n = 5\n.byte n"), vec![5]);
    // `.set` may be redefined; a label may not.
    assert_eq!(text(".set n, 1\n.byte n\n.set n, 2\n.byte n"), vec![1, 2]);
    assert!(errors("foo: nop\nfoo: nop").contains("already defined"));
    assert!(errors(".equiv n, 1\n.equiv n, 2").contains("already defined"));
    assert!(errors(".set a, b\n.set b, a\n.byte a").contains("circular"));
}

#[test]
fn label_arithmetic() {
    // A distance between two labels in one section is a constant, so it
    // resolves even in relocatable output.
    assert_eq!(text("a: nop\nnop\nb:\n.byte b - a"), vec![0x90, 0x90, 2]);
    assert_eq!(text("a:\n.long . - a\n"), vec![0, 0, 0, 0]);
    // A bare label address is not: it needs a relocation.
    assert_eq!(text("nop\na:\n.byte a"), vec![0x90, 0]);
    let asm = assemble_flat("nop\na:\n.byte a", 0);
    assert_eq!(section(&asm, ".text"), vec![0x90, 1]);
}

#[test]
fn numeric_local_labels() {
    // Backward and forward references pick the nearest matching label.
    assert_eq!(text("1: nop\n.byte 1b"), vec![0x90, 0]);
    // A forward reference in relocatable output leaves a hole for the
    // linker; resolved absolutely it is just the offset.
    assert_eq!(text(".byte 1f\nnop\n1: nop"), vec![0, 0x90, 0x90]);
    let asm = assemble_flat(".byte 1f\nnop\n1: nop", 0);
    assert_eq!(section(&asm, ".text"), vec![2, 0x90, 0x90]);
    // The same number can be reused; each `1:` starts a new scope.
    assert_eq!(
        text("1: nop\n.byte 1b\n1: nop\n.byte 1b"),
        vec![0x90, 0, 0x90, 0]
    );
    // Each `1:` opens a new scope, so the second `1b` sees the second label.
    let asm = assemble_flat("1: nop\n.byte 1b\n1: nop\n.byte 1b", 0);
    assert_eq!(section(&asm, ".text"), vec![0x90, 0, 0x90, 2]);
    assert!(errors(".byte 1b").contains("no previous local label"));
    assert!(errors("jmp 1f").contains("no local label"));
}

#[test]
fn conditional_assembly() {
    assert_eq!(text(".if 1\n.byte 1\n.else\n.byte 2\n.endif"), vec![1]);
    assert_eq!(text(".if 0\n.byte 1\n.else\n.byte 2\n.endif"), vec![2]);
    assert_eq!(text(".set x,1\n.ifdef x\n.byte 1\n.endif"), vec![1]);
    assert_eq!(text(".ifndef nope\n.byte 1\n.endif"), vec![1]);
    assert_eq!(
        text(".if 0\n.byte 1\n.elseif 1\n.byte 2\n.else\n.byte 3\n.endif"),
        vec![2]
    );
    // Only the first true branch runs.
    assert_eq!(text(".if 1\n.byte 1\n.elseif 1\n.byte 2\n.endif"), vec![1]);
    // A false outer branch keeps its inner branches off, whatever they say.
    assert_eq!(
        text(
            ".if 0\n  .if 1\n    .byte 1\n  .else\n    .byte 2\n  .endif\n.else\n  .byte 3\n.endif"
        ),
        vec![3]
    );
    assert!(errors(".if 1\n.byte 1").contains("unterminated"));
    assert!(errors(".endif").contains("without a matching"));
    assert!(errors(".if 1\n.else\n.else\n.endif").contains("duplicate `.else`"));
}

#[test]
fn sections() {
    let asm = assemble(".text\nnop\n.data\n.byte 1\n.text\nret\n");
    assert_eq!(section(&asm, ".text"), vec![0x90, 0xc3]);
    assert_eq!(section(&asm, ".data"), vec![1]);

    // `.pushsection` / `.popsection` nest.
    let asm = assemble("nop\n.pushsection .data\n.byte 1\n.popsection\nret\n");
    assert_eq!(section(&asm, ".text"), vec![0x90, 0xc3]);
    assert_eq!(section(&asm, ".data"), vec![1]);

    // `.previous` swaps back and forth.
    let asm = assemble("nop\n.data\n.byte 1\n.previous\nret\n");
    assert_eq!(section(&asm, ".text"), vec![0x90, 0xc3]);

    // A custom section keeps the flags it was given.
    let asm = assemble(".section .mine,\"ax\",@progbits\nnop\n");
    let s = asm
        .sections
        .iter()
        .find(|s| asm.interner.get(s.name) == ".mine")
        .unwrap();
    assert!(s.flags.alloc && s.flags.exec && !s.flags.write);
}

#[test]
fn bss_rejects_data() {
    assert!(errors(".bss\n.byte 1").contains("allocates no file space"));
    // But it still tracks size.
    let asm = assemble(".bss\n.space 16");
    let s = asm
        .sections
        .iter()
        .find(|s| asm.interner.get(s.name) == ".bss")
        .unwrap();
    assert_eq!(s.size, 16);
}

#[test]
fn diagnostics_point_at_the_problem() {
    let e = errors("movq %rax");
    assert!(e.contains("operand"), "{e}");
    assert!(e.contains("test.s:1:1"), "{e}");

    let e = errors("movq %nosuch, %rax");
    assert!(e.contains("unknown register `%nosuch`"), "{e}");

    let e = errors("frobnicate %rax");
    assert!(e.contains("unknown instruction `frobnicate`"), "{e}");

    let e = errors(".nosuchdirective");
    assert!(e.contains("unknown directive"), "{e}");

    let e = errors(".byte 300");
    assert!(e.contains("does not fit in 1 byte"), "{e}");

    let e = errors(".arch nosucharch");
    assert!(
        e.contains("unknown architecture") && e.contains("x86-64"),
        "{e}"
    );
}

#[test]
fn diagnostics_survive_after_the_first_error() {
    let e = errors("frobnicate\nzimzam\n.byte 300\n");
    assert!(e.contains("frobnicate"), "{e}");
    assert!(e.contains("zimzam"), "{e}");
    assert!(e.contains("does not fit"), "{e}");
}

#[test]
fn symbol_attributes() {
    use rsasm::symbol::{Binding, SymType, Visibility};
    let asm = assemble(
        ".globl g\n.weak w\n.hidden h\n.type f, @function\n\
         g: nop\nw: nop\nh: nop\nf: nop\n.size f, 1\n",
    );
    let find = |n: &str| {
        asm.symbols
            .iter()
            .find(|(_, s)| asm.interner.get(s.name) == n)
            .map(|(_, s)| s)
            .unwrap_or_else(|| panic!("no symbol `{n}`"))
    };
    assert_eq!(find("g").binding, Binding::Global);
    assert_eq!(find("w").binding, Binding::Weak);
    assert_eq!(find("h").visibility, Visibility::Hidden);
    assert_eq!(find("f").ty, SymType::Func);
}

#[test]
fn error_and_warning_directives() {
    assert!(errors(r#".error "boom""#).contains("boom"));
    let asm = assemble(r#".warning "careful""#);
    assert!(!asm.diags.has_errors());
    assert!(asm.diags.render(&asm.sm, false).contains("careful"));
}

#[test]
fn word_takes_its_width_from_the_target() {
    // x86 keeps the 16-bit word of the 8086.
    assert_eq!(text(".word 1").len(), 2);
    // The fixed-width synonyms do not depend on the target at all.
    assert_eq!(text(".half 1").len(), 2);
    assert_eq!(text(".dword 1").len(), 8);
    assert_eq!(text(".xword 1").len(), 8);
}

#[test]
#[cfg(feature = "sparc")]
fn word_is_four_bytes_on_sparc() {
    // Measured against llvm-mc, which agrees with GNU as here.
    assert_eq!(text_for("sparc", ".word 1"), vec![0, 0, 0, 1]);
}

#[test]
#[cfg(feature = "mips")]
fn word_is_four_bytes_on_mips() {
    assert_eq!(text_for("mips", ".word 1"), vec![0, 0, 0, 1]);
    assert_eq!(text_for("mipsel", ".word 1"), vec![1, 0, 0, 0]);
}

#[test]
fn a_set_option_the_backend_does_not_know_is_explained() {
    // `.set noreorder` means something on MIPS and nothing on x86; the error
    // should say so rather than calling `.set` an unknown directive.
    let e = errors(".set noreorder\n");
    assert!(
        e.contains("not an option the `x86-64` backend understands"),
        "{e}"
    );
    assert!(e.contains(".set name, value"), "{e}");
}

#[test]
fn an_unknown_relocation_modifier_in_data_is_an_error() {
    // It used to fall back to the plain data relocation, which is a different
    // program: `.quad foo@bogus` became an absolute reference to `foo`.
    let e = errors(".quad foo@bogus\n");
    assert!(e.contains("`@bogus` is not a relocation modifier"), "{e}");
    // A modifier the target does know still selects its relocation.
    let asm = assemble(".quad foo@GOTPCREL\n");
    assert!(!asm.diags.has_errors());
    assert_eq!(asm.relocs[0].kind, 9, "R_X86_64_GOTPCREL");
}

#[test]
fn flat_output_sizes_see_real_addresses() {
    // During layout a flat image's labels used to sit at their offset within
    // the section rather than at base + offset, so this went negative.
    let asm = assemble_flat(
        "start:\n.space start + 4 - 0x8000, 0xaa\n.byte 0xbb\n",
        0x8000,
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(section(&asm, ".text"), vec![0xaa, 0xaa, 0xaa, 0xaa, 0xbb]);
}

#[test]
fn range_errors_name_the_actual_limit() {
    // A short jump that cannot be relaxed further, forced through `.byte`
    // arithmetic, so the message comes from the core.
    let e = errors("a:\n.byte a - b\n.space 300\nb:\n");
    assert!(e.contains("out of range (-128 to 255)"), "{e}");
}

#[test]
fn a_true_comparison_is_minus_one_as_in_gnu_as() {
    // GNU as: comparisons give -1, while `!`, `&&` and `||` give 1.
    assert_eq!(
        hex(&text(
            ".byte 1<2, 2<1, 1==1, 1!=1, 3>=2, 1<>2, !0, !5, 1&&2, 0||3\n"
        )),
        "ff 00 ff 00 ff ff 01 00 01 01"
    );
    assert_eq!(
        hex(&text(
            ".if 1<2\n.byte 7\n.endif\n.if (2<1)\n.byte 8\n.endif\n.byte (1<2)&3, -(1==1)\n"
        )),
        "07 03 01"
    );
}

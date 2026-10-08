//! PE/COFF object output (`-f coff`).
//!
//! The whole-object expectations below are llvm-mc 22's objects for the same
//! source, byte for byte: for these the two assemblers write identical files,
//! down to the order of the symbol and string tables, so the object llvm-mc
//! wrote is what rsasm has to write. `tools/coff-diff` checks many more cases
//! in a canonical form that does not depend on table order.

#![cfg(feature = "x86")]

use rsasm::arch;
use rsasm::assembler::{Assembler, Options};
use rsasm::lexer::Dialect;
use rsasm::output::{self, Format};

fn assemble(arch: &str, dialect: Dialect, format: Format, src: &str) -> Assembler {
    let options = Options::new().with_format(format).with_dialect(dialect);
    let mut asm = Assembler::new(arch::lookup(arch).expect("backend"), options);
    asm.assemble_str("in.s", src);
    asm.finish();
    asm
}

fn coff(arch: &str, src: &str) -> Vec<u8> {
    let asm = assemble(arch, Dialect::Gas, Format::Coff, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    output::coff::build(&asm).expect("COFF output")
}

fn errors(arch: &str, format: Format, src: &str) -> String {
    let asm = assemble(arch, Dialect::Gas, format, src);
    assert!(asm.diags.has_errors(), "expected an error:\n{src}");
    asm.diags.render(&asm.sm, false)
}

fn unhex(parts: &[&str]) -> Vec<u8> {
    let s: String = parts.concat();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn u16at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// Each section header's short name and characteristics.
fn sections(b: &[u8]) -> Vec<(String, u32)> {
    (0..u16at(b, 2) as usize)
        .map(|i| {
            let h = 20 + 40 * i;
            let name = String::from_utf8_lossy(&b[h..h + 8])
                .trim_end_matches('\0')
                .to_string();
            (name, u32at(b, h + 36))
        })
        .collect()
}

#[test]
fn a_hello_world_for_x86_64_matches_llvm_mc() {
    // llvm-mc -triple=x86_64-windows-msvc -filetype=obj
    let src = r#"        .text
        .globl  main
main:
        subq    $40, %rsp
        leaq    msg(%rip), %rcx
        call    puts
        xorl    %eax, %eax
        addq    $40, %rsp
        ret
        .section .rdata,"dr"
msg:    .asciz  "Hello"
"#;
    let expected = unhex(&[
        "6486040000000000e50000000b000000000000002e74657874000000000000000000000017000000b4000000cb000000",
        "0000000002000000200030602e64617461000000000000000000000000000000df000000000000000000000000000000",
        "400030c02e6273730000000000000000000000000000000000000000000000000000000000000000800030c02e726461",
        "74610000000000000000000006000000df000000000000000000000000000000400010404883ec28488d0d00000000e8",
        "0000000031c04883c428c3070000000900000004000c0000000a000000040048656c6c6f002e74657874000000000000",
        "000100000003011700000002000000977f1b960100000000002e64617461000000000000000200000003010000000000",
        "000000000000000200000000002e62737300000000000000000300000003010000000000000000000000000300000000",
        "002e72646174610000000000000400000003010600000000000000ab7d81600400000000006d61696e00000000000000",
        "000100000002006d736700000000000000000004000000030070757473000000000000000000000000020004000000",
    ]);
    assert_eq!(coff("x86-64", src), expected);
}

#[test]
fn a_hello_world_for_i386_matches_llvm_mc() {
    // llvm-mc -triple=i686-windows-msvc -filetype=obj
    let src = r#"        .text
        .globl  _main
_main:
        pushl   $msg
        call    _puts
        addl    $4, %esp
        xorl    %eax, %eax
        ret
        .section .rdata,"dr"
msg:    .asciz  "Hello"
"#;
    let expected = unhex(&[
        "4c01040000000000de0000000b000000000000002e74657874000000000000000000000010000000b4000000c4000000",
        "0000000002000000200030602e64617461000000000000000000000000000000d8000000000000000000000000000000",
        "400030c02e6273730000000000000000000000000000000000000000000000000000000000000000800030c02e726461",
        "74610000000000000000000006000000d8000000000000000000000000000000400010406800000000e80000000083c4",
        "0431c0c301000000090000000600060000000a000000140048656c6c6f002e7465787400000000000000010000000301",
        "1000000002000000f32449300100000000002e6461746100000000000000020000000301000000000000000000000000",
        "0200000000002e62737300000000000000000300000003010000000000000000000000000300000000002e7264617461",
        "0000000000000400000003010600000000000000ab7d81600400000000005f6d61696e00000000000000010000000200",
        "6d73670000000000000000000400000003005f707574730000000000000000000000020004000000",
    ]);
    assert_eq!(coff("i386", src), expected);
}

#[cfg(feature = "aarch64")]
#[test]
fn a_tail_call_for_arm64_matches_llvm_mc() {
    // llvm-mc -triple=aarch64-windows-msvc -filetype=obj
    let src = r#"        .text
        .globl  main
main:
        adrp    x0, msg
        add     x0, x0, :lo12:msg
        b       puts
        .section .rdata,"dr"
msg:    .asciz  "Hello"
"#;
    let expected = unhex(&[
        "64aa040000000000e40000000b000000000000002e7465787400000000000000000000000c000000b4000000c0000000",
        "0000000003000000200030602e64617461000000000000000000000000000000de000000000000000000000000000000",
        "400030c02e6273730000000000000000000000000000000000000000000000000000000000000000800030c02e726461",
        "74610000000000000000000006000000de00000000000000000000000000000040001040000000900000009100000014",
        "0000000009000000040004000000090000000600080000000a000000030048656c6c6f002e7465787400000000000000",
        "0100000003010c00000003000000e398293c0100000000002e6461746100000000000000020000000301000000000000",
        "0000000000000200000000002e6273730000000000000000030000000301000000000000000000000000030000000000",
        "2e72646174610000000000000400000003010600000000000000ab7d81600400000000006d61696e0000000000000000",
        "0100000002006d736700000000000000000004000000030070757473000000000000000000000000020004000000",
    ]);
    assert_eq!(coff("aarch64", src), expected);
}

#[test]
fn unwind_data_for_a_function_matches_llvm_mc() {
    // llvm-mc -triple=x86_64-windows-msvc -filetype=obj
    let src = r#"        .text
        .globl  f
        .def    f; .scl 2; .type 32; .endef
        .seh_proc f
f:
        pushq   %rbp
        .seh_pushreg %rbp
        subq    $32, %rsp
        .seh_stackalloc 32
        .seh_endprologue
        call    g
        addq    $32, %rsp
        popq    %rbp
        ret
        .seh_endproc
"#;
    let expected = unhex(&[
        "6486050000000000280100000c000000000000002e74657874000000000000000000000010000000dc000000ec000000",
        "0000000001000000200030602e64617461000000000000000000000000000000f6000000000000000000000000000000",
        "400030c02e6273730000000000000000000000000000000000000000000000000000000000000000800030c02e786461",
        "74610000000000000000000008000000f6000000000000000000000000000000400030402e7064617461000000000000",
        "000000000c000000fe0000000a010000000000000300000040003040554883ec20e8000000004883c4205dc306000000",
        "0b0000000400010502000532015000000000100000000000000000000000000000000300040000000000000003000800",
        "00000600000003002e74657874000000000000000100000003011000000001000000f87ad3200100000000002e646174",
        "61000000000000000200000003010000000000000000000000000200000000002e627373000000000000000003000000",
        "03010000000000000000000000000300000000002e786461746100000000000004000000030108000000000000004b2f",
        "1bb10400000000002e70646174610000000000000500000003010c000000030000002b31bb7c05000000000066000000",
        "000000000000000001002000020067000000000000000000000000000000020004000000",
    ]);
    assert_eq!(coff("x86-64", src), expected);
}

#[test]
fn comdat_weak_rva_and_secrel_match_llvm_mc() {
    // llvm-mc -triple=x86_64-windows-msvc -filetype=obj
    let src = r#"        .section .text$f,"xr",discard,f
        .globl  f
f:      ret
        .weak   w
w:      ret
        .data
        .rva    f
        .secrel32 f
        .quad   w
"#;
    let expected = unhex(&[
        "6486040000000000e40000000c000000000000002e74657874000000000000000000000000000000b400000000000000",
        "0000000000000000200030602e64617461000000000000000000000010000000b4000000c40000000000000003000000",
        "400030c02e6273730000000000000000000000000000000000000000000000000000000000000000800030c02e746578",
        "74246600000000000000000002000000e200000000000000000000000000000020101060000000000000000000000000",
        "000000000000000008000000030004000000080000000b0008000000090000000100c3c32e7465787400000000000000",
        "0100000003010000000000000000000000000100000000002e6461746100000000000000020000000301100000000300",
        "0000000000000200000000002e6273730000000000000000030000000301000000000000000000000000030000000000",
        "2e746578742466000000000004000000030102000000000000008717bae2040002000000660000000000000000000000",
        "0400000002007700000000000000000000000000000069010b0000000300000000000000000000000000000000000400",
        "000001000000040000000200160000002e7765616b2e772e64656661756c742e6600",
    ]);
    assert_eq!(coff("x86-64", src), expected);
}

#[test]
fn nasm_sections_take_nasm_characteristics() {
    // From `nasm -f win64` 2.16.03: its COFF writer's flags for each kind of
    // section, alignment included.
    let src = "section .text\nret\nsection .data\ndd 1\nsection .rdata\ndb 1\n\
               section .bss\nresb 4\nsection .other\ndb 2\n\
               section .d8 data align=8\ndq 3\nsection .inf info\ndb 1\n";
    let asm = assemble("x86-64", Dialect::Nasm, Format::Coff, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let b = output::coff::build(&asm).unwrap();
    let got = sections(&b);
    let want = [
        (".text", 0x6050_0020),
        (".data", 0xc030_0040),
        (".rdata", 0x4040_0040),
        (".bss", 0xc030_0080),
        (".other", 0x6050_0020),
        (".d8", 0xc040_0040),
        (".inf", 0x0010_0a00),
    ];
    for (name, flags) in want {
        assert!(
            got.iter().any(|(n, f)| n == name && *f == flags),
            "{name} should have {flags:#x}; got {got:x?}"
        );
    }
}

#[test]
fn coff_directives_need_coff_output() {
    let e = errors("x86-64", Format::Elf, ".rva foo\n");
    assert!(e.contains("-f coff"), "{e}");
}

#[test]
fn a_field_coff_has_no_relocation_for_is_refused() {
    // A one-byte PC-relative field, which llvm-mc refuses too.
    let e = errors("x86-64", Format::Coff, "jecxz foo\n");
    assert!(e.contains("no relocation"), "{e}");
}

#[test]
fn dwarf_is_refused_in_coff_objects() {
    let e = errors(
        "x86-64",
        Format::Coff,
        ".cfi_startproc\nret\n.cfi_endproc\n",
    );
    assert!(e.contains("DWARF"), "{e}");
}

#[test]
fn unwind_data_in_a_comdat_section_is_refused() {
    let src = ".section .text$f,\"xr\",discard,f\n.seh_proc f\nf: ret\n\
               .seh_endprologue\n.seh_endproc\n";
    let e = errors("x86-64", Format::Coff, src);
    assert!(e.contains("COMDAT"), "{e}");
}

#[test]
fn common_alignment_is_a_power_of_two_up_to_32_bytes() {
    // llvm-mc stops at 2^5 in a Windows object.
    let e = errors("x86-64", Format::Coff, ".comm big, 8, 6\n");
    assert!(e.contains("32 bytes"), "{e}");
}

#[cfg(feature = "aarch64")]
#[test]
fn x86_64_unwind_directives_are_refused_for_arm64() {
    let e = errors(
        "aarch64",
        Format::Coff,
        ".seh_proc f\nf: ret\n.seh_endproc\n",
    );
    assert!(e.contains("x86-64 unwind data"), "{e}");
}

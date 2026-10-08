//! DWARF line tables and call frame information.
//!
//! Every expected byte string and relocation here came out of a reference
//! run: GNU as 2.47 (`x86_64-elf-as` from tools/oracles, with
//! `--nocompress-debug-sections`) for x86-64, llvm-mc 22 for AArch64 and
//! SPARC, dumped with `llvm-objcopy --dump-section` and `llvm-readelf -r`.
//! tools/dwarf-diff/run.sh compares whole sections against both references
//! across every target; these pin a few of those results without them.

mod common;

#[allow(unused_imports)]
use common::*;
#[allow(unused_imports)]
use rsasm::assembler::{Assembler, Options};

/// Assembles `src` as `test.s`, with `-g` if `debug_source`.
#[allow(dead_code)]
fn assemble_with(arch: &str, src: &str, debug_source: bool) -> Assembler {
    let arch = rsasm::arch::lookup(arch).expect("backend in this build");
    let options = Options::new().with_debug_source(debug_source);
    let mut asm = Assembler::new(arch, options);
    asm.assemble_str("test.s", src);
    asm.finish();
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm
}

#[allow(dead_code)]
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The relocations against a section, as (offset, type).
#[allow(dead_code)]
fn relocs_in(asm: &Assembler, name: &str) -> Vec<(u64, u32)> {
    asm.relocs
        .iter()
        .filter(|r| asm.interner.get(asm.section(r.section).name) == name)
        .map(|r| (r.offset, r.kind))
        .collect()
}

#[allow(dead_code)]
const LINES: &str = "\t.file 1 \"a.c\"\n\t.text\nf:\n\t.loc 1 1 0\n\tnop\n\t.loc 1 2 0\n\tnop\n\tnop\n\t.loc 1 3 4\n\tnop\n";

#[cfg(feature = "x86")]
#[test]
fn gnu_line_table_is_version_3() {
    let asm = assemble_with("x86-64", LINES, false);
    assert_eq!(
        section(&asm, ".debug_line"),
        unhex(
            "3500000003001a0000000101fb0e0d00010101010000000100000100612e630000000000\
             0009020000000000000000012105042f0201000101"
        )
    );
    // R_X86_64_64 for the sequence's address.
    assert_eq!(relocs_in(&asm, ".debug_line"), [(0x27, 1)]);
}

#[cfg(feature = "aarch64")]
#[test]
fn llvm_line_table_is_version_4() {
    let asm = assemble_with("aarch64", LINES, false);
    assert_eq!(
        section(&asm, ".debug_line"),
        unhex(
            "3600000004001b000000010101fb0e0d00010101010000000100000100612e630000000000\
             0009020000000000000000014b0504830204000101"
        )
    );
}

#[cfg(feature = "x86")]
#[test]
fn gnu_eh_frame() {
    let src = "\t.text\nf:\n\t.cfi_startproc\n\tpushq %rbp\n\t.cfi_def_cfa_offset 16\n\
               \t.cfi_offset %rbp, -16\n\tpopq %rbp\n\t.cfi_def_cfa_offset 8\n\tret\n\
               \t.cfi_endproc\n";
    let asm = assemble_with("x86-64", src, false);
    assert_eq!(
        section(&asm, ".eh_frame"),
        unhex(
            "1400000000000000017a5200017810011b0c0708900100001c0000001c000000000000000300\
             000000410e108602410e0800000000000000"
        )
    );
    // R_X86_64_PC32 for the FDE's start.
    assert_eq!(relocs_in(&asm, ".eh_frame"), [(0x20, 2)]);
}

#[cfg(feature = "aarch64")]
#[test]
fn llvm_debug_frame() {
    let src = "\t.cfi_sections .debug_frame\n\t.text\nf:\n\t.cfi_startproc\n\
               \tstp x29, x30, [sp, #-16]!\n\t.cfi_def_cfa_offset 16\n\t.cfi_offset w30, -8\n\
               \t.cfi_offset w29, -16\n\tldp x29, x30, [sp], #16\n\t.cfi_def_cfa_offset 0\n\
               \tret\n\t.cfi_endproc\n";
    let asm = assemble_with("aarch64", src, false);
    assert_eq!(
        section(&asm, ".debug_frame"),
        unhex(
            "14000000ffffffff04000800017c1e0c1f00000000000000240000000000000000000000000000\
             000c00000000000000440e109e029d04440e00000000000000"
        )
    );
    // R_AARCH64_ABS32 for the CIE pointer, R_AARCH64_ABS64 for the start.
    assert_eq!(relocs_in(&asm, ".debug_frame"), [(0x1c, 258), (0x20, 257)]);
}

#[cfg(feature = "x86")]
#[test]
fn gnu_describes_the_source_with_g() {
    let asm = assemble_with("x86-64", "\t.text\n\tnop\n\n\tnop\n", true);
    assert_eq!(
        section(&asm, ".debug_line"),
        unhex(
            "3200000002001a0000000101fb0e0a00010101010000000100746573742e73000000000000\
             09020000000000000000101f0201000101"
        )
    );
}

#[cfg(feature = "aarch64")]
#[test]
fn llvm_describes_the_source_with_g() {
    let asm = assemble_with("aarch64", "\t.text\n\tnop\n\n\tnop\n", true);
    assert_eq!(
        section(&asm, ".debug_line"),
        unhex(
            "3600000004001e000000010101fb0e0d00010101010000000100000100746573742e7300000000\
             000009020000000000000000134c0204000101"
        )
    );
}

/// llvm-mc picks SPARC's unaligned data relocation by the offset of the field
/// in its fragment, and each sequence of a line table starts a fragment.
#[cfg(feature = "sparc")]
#[test]
fn sparc_sequence_addresses_are_aligned_within_their_fragment() {
    let src = "\t.file 1 \"a.c\"\n\t.file 2 \"b.c\"\n\t.text\n\t.loc 1 1\n\tnop\n\
               \t.section .text.b,\"ax\",%progbits\n\t.loc 2 2 200\n\tnop\n\tnop\n\
               \t.data\n\t.loc 1 3\n\t.long 1\n";
    let asm = assemble_with("sparc", src, false);
    // R_SPARC_UA32, R_SPARC_32, R_SPARC_UA32.
    assert_eq!(
        relocs_in(&asm, ".debug_line"),
        [(0x2f, 23), (0x41, 3), (0x4e, 23)]
    );
}

#[cfg(feature = "v850")]
#[test]
fn cfi_is_an_error_where_the_reference_has_none() {
    let e = errors_for("v850", "\t.text\n\t.cfi_startproc\n\tnop\n\t.cfi_endproc\n");
    assert!(e.contains("CFI is not supported for this target"), "{e}");
}

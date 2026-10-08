//! Per-target GNU as conventions the core applies on a backend's say-so:
//! what `.align n` counts, and whether a section's end is rounded up to its
//! alignment. Every expectation is what the target's GNU as (cross binutils
//! 2.47) or llvm-mc produced for the same source.

mod common;
#[allow(unused_imports)]
use common::*;

/// Asserts `.data` bytes for `.byte 1; .align 2, 0; .byte 2` on a target.
#[allow(dead_code)]
fn align_2(arch: &str) -> String {
    hex(&section(
        &assemble_for(arch, ".data\n.byte 1\n.align 2, 0\n.byte 2\n"),
        ".data",
    ))
}

#[allow(dead_code)]
fn size_of(asm: &rsasm::assembler::Assembler, name: &str) -> u64 {
    asm.sections
        .iter()
        .find(|s| asm.interner.get(s.name) == name)
        .unwrap_or_else(|| panic!("no section named `{name}`"))
        .size
}

macro_rules! align_counts {
    ($($feature:literal, $test:ident, $arch:literal => $bytes:literal;)*) => {$(
        #[cfg(feature = $feature)]
        #[test]
        fn $test() {
            assert_eq!(align_2($arch), $bytes);
        }
    )*};
}

// Bytes: x86 ELF, SPARC, RX. Powers of two: the rest.
align_counts! {
    "x86", align_counts_bytes_on_x86_64, "x86-64" => "01 00 02";
    "sparc", align_counts_bytes_on_sparc, "sparc" => "01 00 02";
    "rx", align_counts_bytes_on_rx, "rx" => "01 00 02 00";
    "arm", align_is_a_power_of_two_on_arm, "arm" => "01 00 00 00 02";
    "arm", align_is_a_power_of_two_on_thumb, "thumb" => "01 00 00 00 02";
    "aarch64", align_is_a_power_of_two_on_aarch64, "aarch64" => "01 00 00 00 02";
    "riscv", align_is_a_power_of_two_on_riscv, "riscv64" => "01 00 00 00 02";
    "mips", align_is_a_power_of_two_on_mips, "mips" => "01 00 00 00 02";
    "powerpc", align_is_a_power_of_two_on_powerpc, "powerpc" => "01 00 00 00 02";
    "rl78", align_is_a_power_of_two_on_rl78, "rl78" => "01 00 00 00 02 00 00 00";
    "v850", align_is_a_power_of_two_on_v850, "v850" => "01 00 00 00 02 00 00 00";
    "superh", align_is_a_power_of_two_on_superh, "sh" => "01 00 00 00 02";
}

#[allow(dead_code)]
const TAIL: &str = ".byte 1\n.balign 4\n.byte 2\n\
                    .data\n.byte 3\n.balign 4\n.byte 4\n\
                    .bss\n.skip 1\n.balign 4\n.skip 1\n";

macro_rules! tails {
    ($($feature:literal, $test:ident, $arch:literal =>
        $text:literal, $data:literal, $bss:literal;)*) => {$(
        #[cfg(feature = $feature)]
        #[test]
        fn $test() {
            let asm = assemble_for($arch, TAIL);
            assert_eq!(hex(&section(&asm, ".text")), $text);
            assert_eq!(hex(&section(&asm, ".data")), $data);
            assert_eq!(size_of(&asm, ".bss"), $bss);
        }
    )*};
}

tails! {
    "rl78", rl78_rounds_every_section, "rl78" =>
        "01 00 00 00 02 00 00 00", "03 00 00 00 04 00 00 00", 8;
    "rx", rx_rounds_every_section_padding_code_with_no_ops, "rx" =>
        "01 fc 13 00 02 fc 13 00", "03 00 00 00 04 00 00 00", 8;
    "v850", v850_rounds_every_section, "v850" =>
        "01 00 00 00 02 00 00 00", "03 00 00 00 04 00 00 00", 8;
    "superh", superh_rounds_only_code_sections, "sh" =>
        "01 00 00 09 02 00 00 09", "03 00 00 00 04", 5;
    // llvm-mc's padding: GNU as pads after a data-only fragment with
    // `90 66 90` instead, though after an instruction it agrees.
    "x86", x86_64_rounds_nothing, "x86-64" =>
        "01 0f 1f 00 02", "03 00 00 00 04", 5;
    "sparc", sparc_rounds_nothing, "sparc" =>
        "01 00 00 00 02", "03 00 00 00 04", 5;
}

macro_rules! three_byte {
    ($($feature:literal, $test:ident, $arch:literal, $section:literal, $reloc:literal;)*) => {$(
        #[cfg(feature = $feature)]
        #[test]
        fn $test() {
            let asm = assemble_for($arch, ".3byte 0x123456, x+1, -1\n");
            assert!(!asm.diags.has_errors(), "{}", asm.diags.render(&asm.sm, false));
            assert_eq!(hex(&section(&asm, $section)), "56 34 12 00 00 00 ff ff ff");
            assert_eq!(asm.relocs.len(), 1);
            let r = &asm.relocs[0];
            assert_eq!((r.kind, r.offset, r.addend), ($reloc, 3, 1));
        }
    )*};
}

// `R_RL78_DIR24S` and `R_RX_DIR24S` are both type 2.
three_byte! {
    "rl78", three_byte_data_on_rl78, "rl78", ".text", 2;
    "rx", three_byte_data_on_rx, "rx", ".text", 2;
}

/// `e_flags` of the ELF object for `src`.
#[allow(dead_code)]
fn e_flags(arch: &str, src: &str) -> u32 {
    let asm = assemble_for(arch, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let elf = rsasm::output::elf::build(&asm).expect("ELF output");
    let at = if elf[4] == 2 { 0x30 } else { 0x24 };
    let b: [u8; 4] = elf[at..at + 4].try_into().unwrap();
    if elf[5] == 2 {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    }
}

macro_rules! header_flags {
    ($($feature:literal, $test:ident, $arch:literal, $src:literal => $flags:literal;)*) => {$(
        #[cfg(feature = $feature)]
        #[test]
        fn $test() {
            assert_eq!(e_flags($arch, $src), $flags);
        }
    )*};
}

// llvm-mc for ARM, MIPS and RISC-V (with the corpora's `+m,+a,+f,+d,+c`);
// the cross GNU as for RX and V850.
header_flags! {
    "x86", no_header_flags_on_x86_64, "x86-64", "nop\n" => 0;
    "arm", arm_objects_are_eabi_version_5, "arm", "nop\n" => 0x0500_0000;
    "arm", thumb_objects_are_eabi_version_5, "thumb", "nop\n" => 0x0500_0000;
    "mips", mips32_objects_are_o32_cpic, "mips", "nop\n" => 0x5000_1004;
    "mips", mips64_objects_are_mips64_cpic, "mips64el", "nop\n" => 0x6000_0004;
    "riscv", riscv_objects_are_rvc_double_float, "riscv64", "nop\n" => 0x5;
    "rx", rx_objects_use_the_rx_abi, "rx", "nop\n" => 0x8;
    "v850", v850_objects_use_the_rh850_abi, "v850", "nop\n" => 0xf000_0000;
    "v850", rh850_objects_add_the_v3_flag, "rh850", "nop\n" => 0xf010_0000;
    "v850", the_cpu_the_file_ends_in_decides, "v850", ".v850e3v5\nnop\n" => 0xf010_0000;
}

//! Mach-O objects, compared byte for byte with llvm-mc's.
//!
//! Every expected object below is what `llvm-mc 22 -filetype=obj` wrote for
//! the same source with the triple named in the test, dumped as hex; none was
//! written by hand. `tools/macho-diff/run.sh` checks many more cases the same
//! way, and these keep the important ones checked where llvm-mc is not
//! installed.

mod common;

#[allow(unused_imports)]
use rsasm::arch;
#[allow(unused_imports)]
use rsasm::assembler::{Assembler, Options};
use rsasm::output::{self, Format};

/// Assembles `src` for `arch` as a Mach-O object.
#[allow(dead_code)]
fn macho_for(arch: &str, src: &str) -> Result<Vec<u8>, String> {
    let a = arch::lookup(arch).unwrap_or_else(|| panic!("no `{arch}` backend in this build"));
    let options = Options::new().with_format(Format::MachO);
    let mut asm = Assembler::new(a, options);
    asm.assemble_str("test.s", src);
    if !asm.finish() || asm.diags.has_errors() {
        return Err(asm.diags.render(&asm.sm, false));
    }
    output::macho::build(&asm).map_err(|e| e.to_string())
}

#[allow(dead_code)]
fn unhex(lines: &[&str]) -> Vec<u8> {
    lines
        .iter()
        .flat_map(|l| l.split_whitespace())
        .map(|b| u8::from_str_radix(b, 16).expect("hex byte"))
        .collect()
}

/// Asserts that `src` assembles to exactly the object llvm-mc wrote for it,
/// showing where the two first differ otherwise.
#[allow(dead_code)]
fn assert_object(arch: &str, src: &str, expected: &[&str]) {
    let got = macho_for(arch, src).unwrap_or_else(|e| panic!("assembly failed:\n{e}"));
    let want = unhex(expected);
    if got != want {
        let at = got
            .iter()
            .zip(&want)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        panic!(
            "object differs from llvm-mc's at byte {at:#x} (lengths {} and {})\n  rsasm:   {}\n  llvm-mc: {}",
            got.len(),
            want.len(),
            common::hex(&got[at.min(got.len())..(at + 32).min(got.len())]),
            common::hex(&want[at.min(want.len())..(at + 32).min(want.len())]),
        );
    }
}

#[test]
fn darwin_triples_choose_mach_o() {
    assert_eq!(Format::for_target("apple-macos"), Format::MachO);
    assert_eq!(Format::for_target("apple-darwin23.1.0"), Format::MachO);
    assert_eq!(Format::for_target("unknown-linux-gnu"), Format::Elf);
    assert_eq!(Format::from_name("macho"), Some(Format::MachO));
}

/// A C `main` calling `printf` with a string literal.
#[cfg(feature = "x86")]
#[test]
fn x86_64_hello_as_clang_writes_it() {
    let src = r#"	.section	__TEXT,__text,regular,pure_instructions
	.build_version macos, 14, 0	sdk_version 14, 4
	.globl	_main
	.p2align	4, 0x90
_main:
	pushq	%rbp
	movq	%rsp, %rbp
	leaq	L_.str(%rip), %rdi
	movb	$0, %al
	callq	_printf
	xorl	%eax, %eax
	popq	%rbp
	retq
	.section	__TEXT,__cstring,cstring_literals
L_.str:
	.asciz	"Hello, %s!\n"
.subsections_via_symbols
"#;
    assert_object(
        "x86-64",
        src,
        &[
            "cf fa ed fe 07 00 00 01 03 00 00 00 01 00 00 00",
            "04 00 00 00 68 01 00 00 00 20 00 00 00 00 00 00",
            "19 00 00 00 e8 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "22 00 00 00 00 00 00 00 88 01 00 00 00 00 00 00",
            "22 00 00 00 00 00 00 00 07 00 00 00 07 00 00 00",
            "02 00 00 00 00 00 00 00 5f 5f 74 65 78 74 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "16 00 00 00 00 00 00 00 88 01 00 00 04 00 00 00",
            "b0 01 00 00 02 00 00 00 00 04 00 80 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 63 73 74 72 69 6e",
            "67 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 16 00 00 00 00 00 00 00",
            "0c 00 00 00 00 00 00 00 9e 01 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 32 00 00 00 18 00 00 00",
            "01 00 00 00 00 00 0e 00 00 04 0e 00 00 00 00 00",
            "02 00 00 00 18 00 00 00 c0 01 00 00 02 00 00 00",
            "e0 01 00 00 10 00 00 00 0b 00 00 00 50 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 01 00 00 00",
            "01 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 55 48 89 e5 48 8d 3d 0b",
            "00 00 00 b0 00 e8 00 00 00 00 31 c0 5d c3 48 65",
            "6c 6c 6f 2c 20 25 73 21 0a 00 00 00 00 00 00 00",
            "0e 00 00 00 01 00 00 2d 07 00 00 00 02 00 00 15",
            "01 00 00 00 0f 01 00 00 00 00 00 00 00 00 00 00",
            "07 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00",
            "00 5f 6d 61 69 6e 00 5f 70 72 69 6e 74 66 00 00",
        ],
    );
}

/// Every x86-64 relocation type, local and external relocations, and the field biases.
#[cfg(feature = "x86")]
#[test]
fn x86_64_relocation_kinds() {
    let src = r#"	.text
	.globl	_f
_f:
	callq	_ext
	jmp	_g
	leaq	_ext+8(%rip), %rax
	movl	$1, _ext(%rip)
	movw	$1, _ext(%rip)
	cmpb	$1, Lt(%rip)
	movq	_ext@GOTPCREL(%rip), %rax
	pushq	_ext@GOTPCREL(%rip)
	movl	$_ext, %eax
	movabsq	$_g, %rcx
	leaq	Ls(%rip), %rdi
	movb	$0, Ls2(%rip)
_g:
	nop
Lt:
	ret
	.cstring
Ls:
	.asciz	"a"
Ls2:
	.asciz	"b"
	.data
_d:
	.quad	_ext
	.long	_g - _f
	.quad	Lt
	.long	_ext@GOTPCREL
	.long	_later - _d
_later:
	.long	0
"#;
    assert_object(
        "x86-64",
        src,
        &[
            "cf fa ed fe 07 00 00 01 03 00 00 00 01 00 00 00",
            "03 00 00 00 a0 01 00 00 00 00 00 00 00 00 00 00",
            "19 00 00 00 38 01 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "7b 00 00 00 00 00 00 00 c0 01 00 00 00 00 00 00",
            "7b 00 00 00 00 00 00 00 07 00 00 00 07 00 00 00",
            "03 00 00 00 00 00 00 00 5f 5f 74 65 78 74 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "57 00 00 00 00 00 00 00 c0 01 00 00 00 00 00 00",
            "40 02 00 00 0c 00 00 00 00 04 00 80 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 63 73 74 72 69 6e",
            "67 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 57 00 00 00 00 00 00 00",
            "04 00 00 00 00 00 00 00 17 02 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 64 61 74 61 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 44 41 54 41 00 00",
            "00 00 00 00 00 00 00 00 5b 00 00 00 00 00 00 00",
            "20 00 00 00 00 00 00 00 1b 02 00 00 00 00 00 00",
            "a0 02 00 00 07 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 18 00 00 00",
            "d8 02 00 00 06 00 00 00 38 03 00 00 20 00 00 00",
            "0b 00 00 00 50 00 00 00 00 00 00 00 04 00 00 00",
            "04 00 00 00 01 00 00 00 05 00 00 00 01 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "e8 00 00 00 00 e9 00 00 00 00 48 8d 05 08 00 00",
            "00 c7 05 fc ff ff ff 01 00 00 00 66 c7 05 fe ff",
            "ff ff 01 00 80 3d 00 00 00 00 01 48 8b 05 00 00",
            "00 00 ff 35 00 00 00 00 b8 00 00 00 00 48 b9 00",
            "00 00 00 00 00 00 00 48 8d 3d 09 00 00 00 c6 05",
            "ff ff ff ff 00 90 c3 61 00 62 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "50 00 00 00 01 00 00 6d 4a 00 00 00 02 00 00 15",
            "3f 00 00 00 00 00 00 0e 39 00 00 00 05 00 00 0c",
            "34 00 00 00 05 00 00 4d 2e 00 00 00 05 00 00 3d",
            "26 00 00 00 00 00 00 6d 1e 00 00 00 05 00 00 7d",
            "13 00 00 00 05 00 00 8d 0d 00 00 00 05 00 00 1d",
            "06 00 00 00 00 00 00 2d 01 00 00 00 05 00 00 2d",
            "18 00 00 00 02 00 00 5c 18 00 00 00 03 00 00 0c",
            "14 00 00 00 05 00 00 4d 0c 00 00 00 00 00 00 0e",
            "08 00 00 00 04 00 00 5c 08 00 00 00 00 00 00 0c",
            "00 00 00 00 05 00 00 0e 0d 00 00 00 0e 01 00 00",
            "55 00 00 00 00 00 00 00 16 00 00 00 0e 02 00 00",
            "59 00 00 00 00 00 00 00 13 00 00 00 0e 03 00 00",
            "5b 00 00 00 00 00 00 00 06 00 00 00 0e 03 00 00",
            "77 00 00 00 00 00 00 00 10 00 00 00 0f 01 00 00",
            "00 00 00 00 00 00 00 00 01 00 00 00 01 00 00 00",
            "00 00 00 00 00 00 00 00 00 5f 65 78 74 00 5f 6c",
            "61 74 65 72 00 5f 67 00 5f 66 00 5f 64 00 4c 73",
            "32 00 00 00 00 00 00 00",
        ],
    );
}

/// A switch whose table is data in code, with the region in `LC_DATA_IN_CODE`.
#[cfg(feature = "x86")]
#[test]
fn x86_64_jump_table_in_a_data_region() {
    let src = r#"	.text
	.globl	_classify
_classify:
	cmpl	$2, %edi
	ja	Ldefault
	movl	%edi, %eax
	leaq	Ltable(%rip), %rcx
	movslq	(%rcx,%rax,4), %rax
	addq	%rcx, %rax
	jmpq	*%rax
L0:
	movl	$10, %eax
	retq
L1:
	movl	$20, %eax
	retq
Ldefault:
	movl	$-1, %eax
	retq
	.p2align	2, 0x90
	.data_region jt32
Ltable:
	.long	L0-Ltable
	.long	L1-Ltable
	.long	Ldefault-Ltable
	.end_data_region
"#;
    assert_object(
        "x86-64",
        src,
        &[
            "cf fa ed fe 07 00 00 01 03 00 00 00 01 00 00 00",
            "04 00 00 00 10 01 00 00 00 00 00 00 00 00 00 00",
            "19 00 00 00 98 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "38 00 00 00 00 00 00 00 30 01 00 00 00 00 00 00",
            "38 00 00 00 00 00 00 00 07 00 00 00 07 00 00 00",
            "01 00 00 00 00 00 00 00 5f 5f 74 65 78 74 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "38 00 00 00 00 00 00 00 30 01 00 00 02 00 00 00",
            "00 00 00 00 00 00 00 00 00 04 00 80 00 00 00 00",
            "00 00 00 00 00 00 00 00 29 00 00 00 10 00 00 00",
            "68 01 00 00 08 00 00 00 02 00 00 00 18 00 00 00",
            "70 01 00 00 01 00 00 00 80 01 00 00 10 00 00 00",
            "0b 00 00 00 50 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 01 00 00 00 01 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "83 ff 02 77 1e 89 f8 48 8d 0d 1e 00 00 00 48 63",
            "04 81 48 01 c8 ff e0 b8 0a 00 00 00 c3 b8 14 00",
            "00 00 c3 b8 ff ff ff ff c3 90 90 90 eb ff ff ff",
            "f1 ff ff ff f7 ff ff ff 2c 00 00 00 0c 00 04 00",
            "01 00 00 00 0f 01 00 00 00 00 00 00 00 00 00 00",
            "00 5f 63 6c 61 73 73 69 66 79 00 00 00 00 00 00",
        ],
    );
}

/// Symbol attributes, zero-filled storage and section types.
#[cfg(feature = "x86")]
#[test]
fn x86_64_symbols_sections_and_storage() {
    let src = r#"	.text
	.globl	_zeta, _alpha
	.private_extern _hidden
	.weak_definition _weak
	.globl	_weak
	.weak_reference _maybe
_alpha:
	callq	_maybe
	retq
_hidden:
	nop
	.alt_entry _inside
_inside:
	retq
_weak:
	retq
_zeta:
	retq
	.lcomm	_small, 4, 2
	.zerofill __DATA,__common,_big,64,5
	.comm	_shared, 256, 4
_constant = 42
	.set	_flags, 0x10
	.literal8
	.quad	0x3ff0000000000000
	.section	__DATA,__mod_init_func,mod_init_funcs
	.p2align	3
	.quad	_alpha
	.section	__DATA,__objc_data,regular,no_dead_strip
	.byte	1
"#;
    assert_object(
        "x86-64",
        src,
        &[
            "cf fa ed fe 07 00 00 01 03 00 00 00 01 00 00 00",
            "03 00 00 00 90 02 00 00 00 00 00 00 00 00 00 00",
            "19 00 00 00 28 02 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "80 00 00 00 00 00 00 00 b0 02 00 00 00 00 00 00",
            "21 00 00 00 00 00 00 00 07 00 00 00 07 00 00 00",
            "06 00 00 00 00 00 00 00 5f 5f 74 65 78 74 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "0a 00 00 00 00 00 00 00 b0 02 00 00 00 00 00 00",
            "d8 02 00 00 01 00 00 00 00 04 00 80 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 62 73 73 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 44 41 54 41 00 00",
            "00 00 00 00 00 00 00 00 24 00 00 00 00 00 00 00",
            "04 00 00 00 00 00 00 00 00 00 00 00 02 00 00 00",
            "00 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 63 6f 6d 6d 6f 6e",
            "00 00 00 00 00 00 00 00 5f 5f 44 41 54 41 00 00",
            "00 00 00 00 00 00 00 00 40 00 00 00 00 00 00 00",
            "40 00 00 00 00 00 00 00 00 00 00 00 05 00 00 00",
            "00 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 6c 69 74 65 72 61",
            "6c 38 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 10 00 00 00 00 00 00 00",
            "08 00 00 00 00 00 00 00 c0 02 00 00 03 00 00 00",
            "00 00 00 00 00 00 00 00 04 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 6d 6f 64 5f 69 6e",
            "69 74 5f 66 75 6e 63 00 5f 5f 44 41 54 41 00 00",
            "00 00 00 00 00 00 00 00 18 00 00 00 00 00 00 00",
            "08 00 00 00 00 00 00 00 c8 02 00 00 03 00 00 00",
            "e0 02 00 00 01 00 00 00 09 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 6f 62 6a 63 5f 64",
            "61 74 61 00 00 00 00 00 5f 5f 44 41 54 41 00 00",
            "00 00 00 00 00 00 00 00 20 00 00 00 00 00 00 00",
            "01 00 00 00 00 00 00 00 d0 02 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 10 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 18 00 00 00",
            "e8 02 00 00 0b 00 00 00 98 03 00 00 50 00 00 00",
            "0b 00 00 00 50 00 00 00 00 00 00 00 05 00 00 00",
            "05 00 00 00 04 00 00 00 09 00 00 00 02 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "e8 00 00 00 00 c3 90 c3 c3 c3 00 00 00 00 00 00",
            "00 00 00 00 00 00 f0 3f 00 00 00 00 00 00 00 00",
            "01 00 00 00 00 00 00 00 01 00 00 00 09 00 00 2d",
            "00 00 00 00 05 00 00 0e 2c 00 00 00 0e 01 00 02",
            "07 00 00 00 00 00 00 00 1a 00 00 00 0e 02 00 00",
            "24 00 00 00 00 00 00 00 27 00 00 00 0e 03 00 00",
            "40 00 00 00 00 00 00 00 01 00 00 00 02 00 00 00",
            "2a 00 00 00 00 00 00 00 0b 00 00 00 02 00 20 00",
            "10 00 00 00 00 00 00 00 49 00 00 00 0f 01 00 00",
            "00 00 00 00 00 00 00 00 12 00 00 00 1f 01 00 00",
            "06 00 00 00 00 00 00 00 21 00 00 00 0f 01 80 00",
            "08 00 00 00 00 00 00 00 43 00 00 00 0f 01 00 00",
            "09 00 00 00 00 00 00 00 34 00 00 00 01 00 40 00",
            "00 00 00 00 00 00 00 00 3b 00 00 00 01 00 00 04",
            "00 01 00 00 00 00 00 00 00 5f 63 6f 6e 73 74 61",
            "6e 74 00 5f 66 6c 61 67 73 00 5f 68 69 64 64 65",
            "6e 00 5f 73 6d 61 6c 6c 00 5f 77 65 61 6b 00 5f",
            "62 69 67 00 5f 69 6e 73 69 64 65 00 5f 6d 61 79",
            "62 65 00 5f 73 68 61 72 65 64 00 5f 7a 65 74 61",
            "00 5f 61 6c 70 68 61 00",
        ],
    );
}

/// A C `main` calling `printf`, with Darwin's `;` comments.
#[cfg(feature = "aarch64")]
#[test]
fn arm64_hello_as_clang_writes_it() {
    let src = r#"	.section	__TEXT,__text,regular,pure_instructions
	.build_version macos, 14, 0	sdk_version 14, 4
	.globl	_main                           ; -- Begin function main
	.p2align	2
_main:                                  ; @main
	stp	x29, x30, [sp, #-16]!
	mov	x29, sp
	adrp	x0, l_.str@PAGE
	add	x0, x0, l_.str@PAGEOFF
	bl	_printf
	mov	w0, #0
	ldp	x29, x30, [sp], #16
	ret
	.section	__TEXT,__cstring,cstring_literals
l_.str:                                 ; @.str
	.asciz	"Hello, %s!\n"
.subsections_via_symbols
"#;
    assert_object(
        "aarch64",
        src,
        &[
            "cf fa ed fe 0c 00 00 01 00 00 00 00 01 00 00 00",
            "04 00 00 00 68 01 00 00 00 20 00 00 00 00 00 00",
            "19 00 00 00 e8 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "2c 00 00 00 00 00 00 00 88 01 00 00 00 00 00 00",
            "2c 00 00 00 00 00 00 00 07 00 00 00 07 00 00 00",
            "02 00 00 00 00 00 00 00 5f 5f 74 65 78 74 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "20 00 00 00 00 00 00 00 88 01 00 00 02 00 00 00",
            "b8 01 00 00 03 00 00 00 00 04 00 80 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 63 73 74 72 69 6e",
            "67 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 20 00 00 00 00 00 00 00",
            "0c 00 00 00 00 00 00 00 a8 01 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 32 00 00 00 18 00 00 00",
            "01 00 00 00 00 00 0e 00 00 04 0e 00 00 00 00 00",
            "02 00 00 00 18 00 00 00 d0 01 00 00 05 00 00 00",
            "20 02 00 00 28 00 00 00 0b 00 00 00 50 00 00 00",
            "00 00 00 00 03 00 00 00 03 00 00 00 01 00 00 00",
            "04 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 fd 7b bf a9 fd 03 00 91",
            "00 00 00 90 00 00 00 91 00 00 00 94 00 00 80 52",
            "fd 7b c1 a8 c0 03 5f d6 48 65 6c 6c 6f 2c 20 25",
            "73 21 0a 00 00 00 00 00 10 00 00 00 04 00 00 2d",
            "0c 00 00 00 01 00 00 4c 08 00 00 00 01 00 00 3d",
            "1c 00 00 00 0e 01 00 00 00 00 00 00 00 00 00 00",
            "01 00 00 00 0e 02 00 00 20 00 00 00 00 00 00 00",
            "16 00 00 00 0e 02 00 00 20 00 00 00 00 00 00 00",
            "08 00 00 00 0f 01 00 00 00 00 00 00 00 00 00 00",
            "0e 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00",
            "00 6c 5f 2e 73 74 72 00 5f 6d 61 69 6e 00 5f 70",
            "72 69 6e 74 66 00 6c 74 6d 70 31 00 6c 74 6d 70",
            "30 00 00 00 00 00 00 00",
        ],
    );
}

/// Page references with and without addends, loads through the GOT, and pairs.
#[cfg(feature = "aarch64")]
#[test]
fn arm64_pages_the_got_and_addends() {
    let src = r#"	.text
	.p2align	2
_f:
	bl	_ext
	bl	_ext+8
	adrp	x0, (_ext+16)@PAGE
	add	x0, x0, (_ext+16)@PAGEOFF
	adrp	x1, Lnear@PAGE
	ldr	x1, [x1, Lnear@PAGEOFF]
	adrp	x2, _ext@GOTPAGE
	ldr	x2, [x2, _ext@GOTPAGEOFF]
	adrp	x3, Ls@PAGE
	add	x3, x3, Ls@PAGEOFF
_g:
	ret
Lnear:
	ret
	.cstring
Ls:
	.asciz	"s"
	.data
_d:
	.quad	_ext
	.quad	_g+4
	.long	_g - Ldata
	.long	_personality@GOT - .
	.quad	_personality@GOT
Ldata:
	.long	0
	.subsections_via_symbols
"#;
    assert_object(
        "aarch64",
        src,
        &[
            "cf fa ed fe 0c 00 00 01 00 00 00 00 01 00 00 00",
            "03 00 00 00 a0 01 00 00 00 20 00 00 00 00 00 00",
            "19 00 00 00 38 01 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "56 00 00 00 00 00 00 00 c0 01 00 00 00 00 00 00",
            "56 00 00 00 00 00 00 00 07 00 00 00 07 00 00 00",
            "03 00 00 00 00 00 00 00 5f 5f 74 65 78 74 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "30 00 00 00 00 00 00 00 c0 01 00 00 02 00 00 00",
            "18 02 00 00 0f 00 00 00 00 04 00 80 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 63 73 74 72 69 6e",
            "67 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 30 00 00 00 00 00 00 00",
            "02 00 00 00 00 00 00 00 f0 01 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 64 61 74 61 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 44 41 54 41 00 00",
            "00 00 00 00 00 00 00 00 32 00 00 00 00 00 00 00",
            "24 00 00 00 00 00 00 00 f2 01 00 00 00 00 00 00",
            "90 02 00 00 06 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 18 00 00 00",
            "c0 02 00 00 09 00 00 00 50 03 00 00 38 00 00 00",
            "0b 00 00 00 50 00 00 00 00 00 00 00 07 00 00 00",
            "07 00 00 00 00 00 00 00 07 00 00 00 02 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 94 00 00 00 94 00 00 00 90 00 00 00 91",
            "01 00 00 90 21 00 40 f9 02 00 00 90 42 00 40 f9",
            "03 00 00 90 63 00 00 91 c0 03 5f d6 c0 03 5f d6",
            "73 00 00 00 00 00 00 00 00 00 04 00 00 00 00 00",
            "00 00 e0 ff ff ff ec ff ff ff 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 24 00 00 00 02 00 00 4c",
            "20 00 00 00 02 00 00 3d 1c 00 00 00 07 00 00 6c",
            "18 00 00 00 07 00 00 5d 14 00 00 00 04 00 00 a4",
            "14 00 00 00 03 00 00 4c 10 00 00 00 04 00 00 a4",
            "10 00 00 00 03 00 00 3d 0c 00 00 00 10 00 00 a4",
            "0c 00 00 00 07 00 00 4c 08 00 00 00 10 00 00 a4",
            "08 00 00 00 07 00 00 3d 04 00 00 00 08 00 00 a4",
            "04 00 00 00 07 00 00 2d 00 00 00 00 07 00 00 2d",
            "18 00 00 00 08 00 00 7e 14 00 00 00 08 00 00 7d",
            "10 00 00 00 06 00 00 1c 10 00 00 00 03 00 00 0c",
            "08 00 00 00 03 00 00 0e 00 00 00 00 07 00 00 0e",
            "2b 00 00 00 0e 01 00 00 00 00 00 00 00 00 00 00",
            "19 00 00 00 0e 01 00 00 00 00 00 00 00 00 00 00",
            "13 00 00 00 0e 02 00 00 30 00 00 00 00 00 00 00",
            "16 00 00 00 0e 01 00 00 28 00 00 00 00 00 00 00",
            "25 00 00 00 0e 02 00 00 30 00 00 00 00 00 00 00",
            "1f 00 00 00 0e 03 00 00 32 00 00 00 00 00 00 00",
            "1c 00 00 00 0e 03 00 00 32 00 00 00 00 00 00 00",
            "0e 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00",
            "01 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00",
            "00 5f 70 65 72 73 6f 6e 61 6c 69 74 79 00 5f 65",
            "78 74 00 4c 73 00 5f 67 00 5f 66 00 5f 64 00 6c",
            "74 6d 70 32 00 6c 74 6d 70 31 00 6c 74 6d 70 30",
            "00 00 00 00 00 00 00 00",
        ],
    );
}

/// Which branches within a section llvm-mc resolves when the file does not ask for real atoms.
#[cfg(feature = "aarch64")]
#[test]
fn arm64_branches_within_a_section() {
    let src = r#"	.text
	.p2align	2
_f:
	bl	_g
	b	Lnear
	b.eq	Lnear
	cbz	x0, Lnear
	adr	x2, Lnear
	ldr	x3, Lpool
Lnear:
	bl	_f
	ret
_g:
	b	_f
Lpool:
	.quad	0x1122334455667788
"#;
    assert_object(
        "aarch64",
        src,
        &[
            "cf fa ed fe 0c 00 00 01 00 00 00 00 01 00 00 00",
            "03 00 00 00 00 01 00 00 00 00 00 00 00 00 00 00",
            "19 00 00 00 98 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "2c 00 00 00 00 00 00 00 20 01 00 00 00 00 00 00",
            "2c 00 00 00 00 00 00 00 07 00 00 00 07 00 00 00",
            "01 00 00 00 00 00 00 00 5f 5f 74 65 78 74 00 00",
            "00 00 00 00 00 00 00 00 5f 5f 54 45 58 54 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "2c 00 00 00 00 00 00 00 20 01 00 00 02 00 00 00",
            "00 00 00 00 00 00 00 00 00 04 00 80 00 00 00 00",
            "00 00 00 00 00 00 00 00 02 00 00 00 18 00 00 00",
            "50 01 00 00 03 00 00 00 80 01 00 00 10 00 00 00",
            "0b 00 00 00 50 00 00 00 00 00 00 00 03 00 00 00",
            "03 00 00 00 00 00 00 00 03 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            "08 00 00 94 05 00 00 14 80 00 00 54 60 00 00 b4",
            "42 00 00 10 83 00 00 58 fa ff ff 97 c0 03 5f d6",
            "f8 ff ff 17 88 77 66 55 44 33 22 11 00 00 00 00",
            "07 00 00 00 0e 01 00 00 00 00 00 00 00 00 00 00",
            "04 00 00 00 0e 01 00 00 00 00 00 00 00 00 00 00",
            "01 00 00 00 0e 01 00 00 20 00 00 00 00 00 00 00",
            "00 5f 67 00 5f 66 00 6c 74 6d 70 30 00 00 00 00",
        ],
    );
}

/// llvm-mc refuses this too.
#[cfg(feature = "aarch64")]
#[test]
fn arm64_adr_to_another_object_is_refused() {
    let err = macho_for("aarch64", "\tadr x0, _ext\n").expect_err("should be refused");
    assert!(err.contains("no Mach-O relocation"), "{err}");
}

/// llvm-mc refuses this too.
#[cfg(feature = "aarch64")]
#[test]
fn arm64_page_references_need_the_darwin_spelling() {
    let err = macho_for("aarch64", "\tadrp x0, _ext\n").expect_err("should be refused");
    assert!(err.contains("@PAGE"), "{err}");
}

/// llvm-mc refuses this too.
#[cfg(feature = "aarch64")]
#[test]
fn arm64_got_references_take_no_addend() {
    let err = macho_for("aarch64", "\tadrp x0, (_ext+8)@GOTPAGE\n").expect_err("should be refused");
    assert!(err.contains("cannot have an addend"), "{err}");
}

/// llvm-mc refuses this too.
#[cfg(feature = "x86")]
#[test]
fn x86_64_absolute_32_bit_addresses_are_refused() {
    let err = macho_for("x86-64", "\tmovq $_ext, %rax\n").expect_err("should be refused");
    assert!(err.contains("32-bit absolute address"), "{err}");
}

/// llvm-mc refuses this too.
#[cfg(feature = "x86")]
#[test]
fn x86_64_differences_name_defined_symbols() {
    let err = macho_for("x86-64", "\t.long _ext - _f\n_f: ret\n").expect_err("should be refused");
    assert!(err.contains("only subtract symbols defined"), "{err}");
}

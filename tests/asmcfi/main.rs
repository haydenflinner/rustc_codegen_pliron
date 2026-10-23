// global_asm functions with .cfi_* directives (wasmtime-fiber style) on
// aarch64 Mach-O. Four things broke on the way: rsasm wrote no Mach-O
// `__eh_frame` at all, had no relocation for an FDE's pcrel `pc_begin`
// (SUBTRACTOR+UNSIGNED pair), objmerge dropped the pair's SUBTRACTOR and
// turned `ltmpN` labels into section relocations (FDE -> text start), and
// raw Mach-O asm names (`_cfi_add1`) were prefixed again (`__cfi_add1`).
use std::arch::global_asm;

global_asm!(
    ".globl _cfi_add1",
    ".p2align 2",
    "_cfi_add1:",
    ".cfi_startproc",
    "stp x29, x30, [sp, -16]!",
    ".cfi_def_cfa_offset 16",
    ".cfi_offset x30, -8",
    ".cfi_offset x29, -16",
    "mov x29, sp",
    "add x0, x0, 1",
    "ldp x29, x30, [sp], 16",
    ".cfi_def_cfa_offset 0",
    "ret",
    ".cfi_endproc",
    ".globl _cfi_add2",
    ".p2align 2",
    "_cfi_add2:",
    ".cfi_startproc",
    "stp x29, x30, [sp, -16]!",
    ".cfi_def_cfa_offset 16",
    "mov x29, sp",
    "bl _cfi_add1",
    "add x0, x0, 1",
    "ldp x29, x30, [sp], 16",
    "ret",
    ".cfi_endproc",
);

unsafe extern "C" {
    fn cfi_add1(x: u64) -> u64;
    fn cfi_add2(x: u64) -> u64;
}

fn main() {
    let r = unsafe { cfi_add1(std::hint::black_box(40)) + cfi_add2(std::hint::black_box(1)) };
    assert_eq!(r, 41 + 3);
    println!("ok");
}

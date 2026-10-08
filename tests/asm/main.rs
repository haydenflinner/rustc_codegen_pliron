use std::arch::{asm, global_asm};

#[cfg(target_arch = "x86_64")]
global_asm!(".globl pliron_ga_add\npliron_ga_add:\n    lea rax, [rdi + rsi]\n    ret");
#[cfg(target_arch = "aarch64")]
global_asm!(".globl pliron_ga_add\npliron_ga_add:\n    add x0, x0, x1\n    ret");
unsafe extern "C" {
    fn pliron_ga_add(a: u64, b: u64) -> u64;
}

#[cfg(target_arch = "x86_64")]
fn main() {
    let mut x: u64 = 40;
    unsafe { asm!("add {0}, {1}", inout(reg) x, in(reg) 2u64) };
    let y: u32;
    unsafe { asm!("mov {0:e}, {1}", out(reg) y, const 7) };
    let src = *b"rep movsb ok";
    let mut dst = [0u8; 12];
    unsafe {
        asm!("rep movsb", inout("rcx") src.len() => _, inout("rsi") src.as_ptr() => _,
             inout("rdi") dst.as_mut_ptr() => _, options(nostack, preserves_flags));
    }
    let z = unsafe { pliron_ga_add(1, 2) };
    println!("{x} {y} {} {z}", std::str::from_utf8(&dst).unwrap());
}

#[cfg(target_arch = "aarch64")]
fn main() {
    let mut x: u64 = 40;
    unsafe { asm!("add {0}, {0}, {1}", inout(reg) x, in(reg) 2u64) };
    let y: u32;
    unsafe { asm!("mov {0:w}, {1}", out(reg) y, const 7) };
    let src = *b"rep movsb ok";
    let mut dst = [0u8; 12];
    unsafe {
        asm!("1: ldrb w9, [{1}], #1\n     strb w9, [{2}], #1\n     subs {0}, {0}, #1\n     b.ne 1b",
             inout(reg) src.len() => _, inout(reg) src.as_ptr() => _,
             inout(reg) dst.as_mut_ptr() => _, out("x9") _,
             options(nostack, preserves_flags));
    }
    let z = unsafe { pliron_ga_add(1, 2) };
    println!("{x} {y} {} {z}", std::str::from_utf8(&dst).unwrap());
}

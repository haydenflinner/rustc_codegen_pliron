use std::arch::{asm, global_asm};

global_asm!(".globl pliron_ga_add\npliron_ga_add:\n    lea rax, [rdi + rsi]\n    ret");
unsafe extern "C" {
    fn pliron_ga_add(a: u64, b: u64) -> u64;
}

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

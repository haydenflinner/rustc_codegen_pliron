// Disassembly pinning for PLIRON_IMMSYNTH: the serde_json SWAR constants
// must materialize in at most two instructions each (LLVM's sequences),
// and a random u64 must keep the four-instruction movz/movn/movk form.
use std::hint::black_box;

#[inline(never)]
pub fn serde_backslash() -> u64 { 0x5c5c5c5c5c5c5c5c }

#[inline(never)]
pub fn serde_himask() -> u64 { 0xfefefefefefefeff }

#[inline(never)]
pub fn serde_lomask() -> u64 { 0xdfdfdfdfdfdfdfe0 }

#[inline(never)]
pub fn random_u64() -> u64 { 0x1234567890abcdef }

#[inline(never)]
pub fn exact_mask() -> u64 { 0x8080808080808080 }

fn main() {
    println!("{:x}", black_box(serde_backslash()));
    println!("{:x}", black_box(serde_himask()));
    println!("{:x}", black_box(serde_lomask()));
    println!("{:x}", black_box(random_u64()));
    println!("{:x}", black_box(exact_mask()));
}

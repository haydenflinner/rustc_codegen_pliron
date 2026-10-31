// r[verify clif.lower.simdshift]
// Native vector shifts and narrowing casts (`src/simd.rs` emits whole-vector
// ops; lowering shifts by a splat's scalar lane, or lane by lane for a
// non-uniform amount, and narrows with mask + unarrow). Prints a checksum
// that must match stock; the memchr movemask idiom is checked directly.
#![feature(portable_simd)]
use std::hint::black_box;
use std::simd::prelude::*;

#[inline(never)]
fn shifts(x: u16x8, y: i16x8, k: u16x8, z: u32x4) -> u64 {
    let a = x >> u16x8::splat(4);
    let b = x << u16x8::splat(3);
    let c = y >> i16x8::splat(5);
    let d = x >> k; // per-lane amounts
    let e: u8x8 = (x >> u16x8::splat(4)).cast(); // memchr's shrn #4
    let f: u16x4 = (z >> u32x4::splat(7)).cast();
    let g: u8x8 = x.cast(); // plain truncation, high bits set
    let mut h = 0u64;
    for v in [a.to_array(), b.to_array(), d.to_array()] {
        for l in v {
            h = h.rotate_left(5) ^ l as u64;
        }
    }
    for l in c.to_array() {
        h = h.rotate_left(5) ^ l as u16 as u64;
    }
    h ^= u64::from_le_bytes(e.to_array()).rotate_left(17);
    h ^= u64::from_le_bytes(g.to_array()).rotate_left(29);
    for l in f.to_array() {
        h = h.rotate_left(7) ^ l as u64;
    }
    h
}

#[inline(never)]
fn movemask(v: u8x16) -> u64 {
    let m: u16x8 = unsafe { std::mem::transmute(v.simd_eq(u8x16::splat(b'"'))) };
    let n: u8x8 = (m >> u16x8::splat(4)).cast();
    u64::from_le_bytes(n.to_array())
}

fn main() {
    let x = black_box(u16x8::from_array([0xffff, 0x1234, 0x8001, 7, 0x00ff, 0xff00, 0x7fff, 0x8000]));
    let y = black_box(i16x8::from_array([-1, -32768, 32767, -100, 100, 0, -2, 1]));
    let k = black_box(u16x8::from_array([0, 1, 2, 3, 15, 8, 4, 12]));
    let z = black_box(u32x4::from_array([0xffff_ffff, 0x1234_5678, 0x8000_0000, 99]));
    println!("{:016x}", shifts(x, y, k, z));
    let mut s = [b'a'; 16];
    s[3] = b'"';
    s[12] = b'"';
    assert_eq!(movemask(black_box(u8x16::from_array(s))), 0x000f_0000_0000_f000);
}

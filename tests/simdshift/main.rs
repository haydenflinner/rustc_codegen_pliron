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

/// 64-bit vectors: Cranelift's aarch64 backend shifts only 128-bit ones,
/// so these go lane by lane (zmij's `u32x2 >> 9` was rejected outright).
#[inline(never)]
fn narrow(p: u32x2, q: i16x4, r: u8x8) -> u64 {
    let a = p >> u32x2::splat(9);
    let b = q >> i16x4::splat(3);
    let c = r << u8x8::splat(2);
    let d: u16x2 = p.cast();
    let mut h = (a[0] as u64) << 32 | a[1] as u64;
    for l in b.to_array() {
        h = h.rotate_left(11) ^ l as u16 as u64;
    }
    h ^= u64::from_le_bytes(c.to_array()).rotate_left(3);
    h ^ ((d[0] as u64) << 16 | d[1] as u64)
}

/// The NEON-intrinsic form: `vshr_n_u32` is `simd_shr(a, vdup_n_u32(N))`,
/// a splat lowering only sees after inlining (zmij's float formatting).
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn neon(p: u32x2) -> u32x2 {
    use std::arch::aarch64::*;
    unsafe { std::mem::transmute(vshr_n_u32::<9>(std::mem::transmute(p))) }
}
#[cfg(not(target_arch = "aarch64"))]
#[inline(never)]
fn neon(p: u32x2) -> u32x2 {
    p >> u32x2::splat(9)
}

fn main() {
    let x = black_box(u16x8::from_array([0xffff, 0x1234, 0x8001, 7, 0x00ff, 0xff00, 0x7fff, 0x8000]));
    let y = black_box(i16x8::from_array([-1, -32768, 32767, -100, 100, 0, -2, 1]));
    let k = black_box(u16x8::from_array([0, 1, 2, 3, 15, 8, 4, 12]));
    let z = black_box(u32x4::from_array([0xffff_ffff, 0x1234_5678, 0x8000_0000, 99]));
    println!("{:016x}", shifts(x, y, k, z));
    let p = black_box(u32x2::from_array([0xdead_beef, 0x0000_ffff]));
    let q = black_box(i16x4::from_array([-32768, -9, 77, 32767]));
    let r = black_box(u8x8::from_array([0xff, 0x81, 1, 0x40, 0x3f, 0, 0x80, 7]));
    println!("{:016x}", narrow(p, q, r));
    let n = neon(p);
    println!("{:08x}{:08x}", n[0], n[1]);
    let mut s = [b'a'; 16];
    s[3] = b'"';
    s[12] = b'"';
    assert_eq!(movemask(black_box(u8x16::from_array(s))), 0x000f_0000_0000_f000);
}

//! wasm32 simd128 test: explicit `core::arch::wasm32` intrinsics, portable SIMD, and
//! auto-vectorizable loops. Every export is deterministic so the pliron build can be
//! checked against a stock-LLVM build of the same crate.
#![no_std]
#![feature(portable_simd, core_intrinsics)]
#![allow(internal_features, unused_unsafe, static_mut_refs)]

use core::arch::wasm32::*;
use core::simd::prelude::*;
use core::simd::simd_swizzle;
use core::simd::num::SimdInt as _;
use core::simd::num::SimdFloat as _;
use core::intrinsics::simd::*;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

static mut BUF: [u8; 4096] = [0; 4096];
static mut BUF32: [u32; 1024] = [0; 1024];

#[unsafe(no_mangle)]
pub extern "C" fn fill(seed: u32) {
    let mut s = seed;
    for b in unsafe { (*(&raw mut BUF)).iter_mut() } {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        *b = (s >> 16) as u8;
    }
    for b in unsafe { (*(&raw mut BUF32)).iter_mut() } {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        *b = s;
    }
}

// --- explicit core::arch intrinsics ---------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn arch_i32x4(a: u32, b: u32) -> u32 {
    let x = i32x4(a as i32, a as i32 + 1, a as i32 + 2, a as i32 + 3);
    let y = i32x4_splat(b as i32);
    let s = i32x4_add(x, y);
    let m = i32x4_mul(s, i32x4_splat(3));
    let m = i32x4_sub(m, i32x4_shl(x, 2));
    (i32x4_extract_lane::<0>(m) as u32)
        ^ (i32x4_extract_lane::<1>(m) as u32).rotate_left(8)
        ^ (i32x4_extract_lane::<2>(m) as u32).rotate_left(16)
        ^ (i32x4_extract_lane::<3>(m) as u32).rotate_left(24)
}

#[unsafe(no_mangle)]
pub extern "C" fn arch_u8x16_sum() -> u32 {
    unsafe {
        let p = &raw const BUF as *const v128;
        let mut acc = u32x4_splat(0);
        for i in 0..256 {
            let v = v128_load(p.add(i));
            let lo = u16x8_extend_low_u8x16(v);
            let hi = u16x8_extend_high_u8x16(v);
            let s = u16x8_add(lo, hi);
            acc = u32x4_add(acc, u32x4_extend_low_u16x8(s));
            acc = u32x4_add(acc, u32x4_extend_high_u16x8(s));
        }
        u32x4_extract_lane::<0>(acc)
            .wrapping_add(u32x4_extract_lane::<1>(acc))
            .wrapping_add(u32x4_extract_lane::<2>(acc))
            .wrapping_add(u32x4_extract_lane::<3>(acc))
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn arch_count_byte(b: u32) -> u32 {
    unsafe {
        let p = &raw const BUF as *const v128;
        let needle = u8x16_splat(b as u8);
        let mut n = 0;
        for i in 0..256 {
            let v = v128_load(p.add(i));
            let m = u8x16_eq(v, needle);
            n += u8x16_bitmask(m).count_ones();
        }
        n
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn arch_f32x4(x: f32) -> f32 {
    let a = f32x4(x, x + 1.0, x + 2.0, x + 3.0);
    let b = f32x4_mul(a, a);
    let c = f32x4_sqrt(f32x4_add(b, f32x4_splat(1.0)));
    let d = f32x4_max(c, f32x4_splat(2.5));
    f32x4_extract_lane::<0>(d) + f32x4_extract_lane::<3>(d)
}

#[unsafe(no_mangle)]
pub extern "C" fn arch_shuffle(a: u32) -> u32 {
    let v = u32x4(a, a + 1, a + 2, a + 3);
    let w = i32x4_shuffle::<3, 2, 5, 0>(v, u32x4_splat(100));
    let r = u8x16_swizzle(w, u8x16(3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8, 15, 14, 13, 12));
    u32x4_extract_lane::<0>(r) ^ u32x4_extract_lane::<2>(r).rotate_left(5)
}

#[unsafe(no_mangle)]
pub extern "C" fn arch_bitselect_any() -> u32 {
    let a = u32x4(1, 2, 3, 4);
    let b = u32x4(10, 20, 30, 40);
    let m = u32x4_gt(a, u32x4_splat(2));
    let r = v128_bitselect(a, b, m);
    (v128_any_true(r) as u32) * 1000
        + u32x4_extract_lane::<0>(r)
        + 2 * u32x4_extract_lane::<2>(r)
        + (i32x4_all_true(r) as u32)
}

// --- portable simd ---------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn psimd_dot() -> u32 {
    let p = unsafe { &*(&raw const BUF32) };
    let mut acc = u32x4::splat(0);
    for c in p.chunks_exact(4) {
        let v = u32x4::from_slice(c);
        acc += v * v;
    }
    acc.reduce_sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn psimd_u8_max() -> u32 {
    let p = unsafe { &*(&raw const BUF) };
    let mut m = u8x16::splat(0);
    for c in p.chunks_exact(16) {
        m = m.simd_max(u8x16::from_slice(c));
    }
    m.reduce_max() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn psimd_f32(x: f32) -> f32 {
    let mut a = f32x4::from_array([x, x * 2.0, x * 3.0, x * 4.0]);
    for _ in 0..3 {
        a = a * a * f32x4::splat(0.5) + f32x4::splat(1.0);
    }
    a.reduce_sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn psimd_cmp_select(t: u32) -> u32 {
    let p = unsafe { &*(&raw const BUF32) };
    let mut acc = u32x4::splat(0);
    for c in p.chunks_exact(4) {
        let v = u32x4::from_slice(c);
        let m = v.simd_gt(u32x4::splat(t));
        acc += m.select(v >> u32x4::splat(3), u32x4::splat(1));
    }
    acc.to_array().iter().fold(0u32, |a, &b| a.wrapping_mul(31).wrapping_add(b))
}

#[unsafe(no_mangle)]
pub extern "C" fn psimd_i16_sat() -> u32 {
    let p = unsafe { &*(&raw const BUF) };
    let mut acc = i16x8::splat(0);
    for c in p.chunks_exact(16) {
        let v = i16x8::from_array(core::array::from_fn(|i| (c[i * 2] as i16) << 7));
        acc = acc.saturating_add(v);
    }
    acc.to_array().iter().fold(0u32, |a, &b| a.wrapping_mul(31).wrapping_add(b as u32))
}

#[unsafe(no_mangle)]
pub extern "C" fn psimd_wide() -> u32 {
    // 256-bit vector: two v128 halves.
    let p = unsafe { &*(&raw const BUF32) };
    let mut acc = u32x8::splat(0);
    for c in p.chunks_exact(8) {
        acc += u32x8::from_slice(c) ^ u32x8::splat(0x55);
    }
    acc.reduce_xor()
}

#[unsafe(no_mangle)]
pub extern "C" fn psimd_small() -> u32 {
    // sub-128-bit vectors: u8x4, u32x2
    let a = u8x4::from_array([1, 2, 3, 4]);
    let b = u8x4::from_array([250, 250, 250, 250]);
    let c = a + b;
    let d = u32x2::from_array([7, 9]) * u32x2::from_array([3, 5]);
    c.to_array().iter().map(|&x| x as u32).sum::<u32>() + d.reduce_sum()
}

// --- plain loops LLVM would auto-vectorize --------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn loop_sum_u32() -> u32 {
    let p = unsafe { &*(&raw const BUF32) };
    p.iter().fold(0u32, |a, &b| a.wrapping_add(b))
}

#[unsafe(no_mangle)]
pub extern "C" fn loop_sum_u8() -> u32 {
    let p = unsafe { &*(&raw const BUF) };
    p.iter().map(|&b| b as u32).sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn loop_axpy(a: u32) -> u32 {
    let (x, y) = unsafe { (&mut *(&raw mut BUF32), &*(&raw const BUF32)) };
    for i in 0..512 {
        x[i] = x[i].wrapping_add(a.wrapping_mul(y[i + 512]));
    }
    x[..512].iter().fold(0u32, |a, &b| a ^ b.rotate_left(3))
}

#[unsafe(no_mangle)]
pub extern "C" fn loop_count_eq(b: u32) -> u32 {
    let p = unsafe { &*(&raw const BUF) };
    p.iter().filter(|&&x| x == b as u8).count() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn loop_max_u8() -> u32 {
    let p = unsafe { &*(&raw const BUF) };
    *p.iter().max().unwrap() as u32
}

// --- wider coverage of lane ops and conversions ---------------------------

fn ld<V: Copy>(off: usize) -> V {
    unsafe { core::ptr::read_unaligned((&raw const BUF as *const u8).add(off) as *const V) }
}

fn fold32(a: &[u32]) -> u32 {
    a.iter().fold(0u32, |x, &y| x.wrapping_mul(31).wrapping_add(y))
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_int(k: u32) -> u32 {
    let b = unsafe { &*(&raw const BUF) };
    let x = ld::<u16x8>(0);
    let y = ld::<u16x8>(16);
    let s = i16x8::from_slice(unsafe { core::slice::from_raw_parts(b.as_ptr() as *const i16, 8) });
    let m = x * y;
    let sh = (m << u16x8::splat((k & 7) as u16)) >> u16x8::splat(3);
    let v = u16x8::from_array(core::array::from_fn(|i| (i as u16 + 1) * 3));
    let sh2 = x >> v; // per-lane shift amounts
    let sh3 = x << (v & u16x8::splat(15));
    let mn = x.simd_min(y).to_array();
    let mx = x.simd_max(y).to_array();
    let sa = (s >> i16x8::splat(2)).to_array();
    let r: [u32; 8] = core::array::from_fn(|i| {
        sh[i] as u32 ^ (sh2[i] as u32) << 3 ^ (sh3[i] as u32) << 5 ^ (mn[i] as u32) << 7
            ^ (mx[i] as u32) << 11 ^ (sa[i] as u32) << 13
    });
    fold32(&r)
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_i32(k: u32) -> u32 {
    let p = unsafe { &*(&raw const BUF32) };
    let a = i32x4::from_slice(&unsafe { core::mem::transmute::<&[u32; 1024], &[i32; 1024]>(p) }[0..4]);
    let b = i32x4::from_slice(&unsafe { core::mem::transmute::<&[u32; 1024], &[i32; 1024]>(p) }[4..8]);
    let c = a * b + (a ^ b) - (a & b);
    let d = c.simd_max(a).simd_min(b);
    let n = -d;
    let sh = n >> i32x4::splat(5) << i32x4::splat((k & 15) as i32);
    let m = a.simd_lt(b).select(a, b);
    let r = (sh + m).to_array();
    fold32(&r.map(|x| x as u32))
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_i64(k: u32) -> u64 {
    let a = i64x2::from_array([k as i64 * 0x1_0000_0001, -(k as i64) - 7]);
    let b = i64x2::from_array([0x7fff_ffff_ffff, 1 << 40]);
    let c = a * b + (a ^ b);
    let u = c.cast::<u64>();
    let lt = u.simd_lt(u64x2::splat(1 << 63));
    let s = lt.select(c, -c);
    let r = (s >> i64x2::splat(3)).to_array();
    (r[0] as u64) ^ (r[1] as u64).rotate_left(17)
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_sat(k: u32) -> u32 {
    let b = unsafe { &*(&raw const BUF) };
    let x = ld::<u8x16>(0);
    let y = ld::<u8x16>(16);
    let sx = i8x16::from_slice(unsafe { core::slice::from_raw_parts(b.as_ptr() as *const i8, 16) });
    let sy = sx.rotate_elements_left::<3>();
    let a = x.saturating_add(y).to_array();
    let s = x.saturating_sub(u8x16::splat(k as u8)).to_array();
    let c = sx.saturating_add(sy).to_array();
    let d = sx.saturating_sub(sy).to_array();
    let r: [u32; 16] = core::array::from_fn(|i| {
        a[i] as u32 ^ (s[i] as u32) << 8 ^ (c[i] as u8 as u32) << 16 ^ (d[i] as u8 as u32) << 24
    });
    fold32(&r)
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_float(x: f32) -> f32 {
    let a = f32x4::from_array([x, -x * 2.5, x + 0.5, x * x - 3.0]);
    let b = f32x4::from_array([1.0, f32::NAN, -0.0, 0.0]);
    let r = a.abs() + unsafe { simd_floor(a) + simd_ceil(a) + simd_fsqrt(simd_fabs(a)) + simd_trunc(a) + simd_round_ties_even(a) }
        + a.simd_max(b) - a.simd_min(b) + (-a);
    r.reduce_sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_f64(x: f64) -> f64 {
    let a = f64x2::from_array([x, -x * 2.5]);
    let b = f64x2::from_array([f64::NAN, 3.5]);
    let r = a.abs() * b.simd_max(a) + a.simd_min(b) - unsafe { simd_floor(a / f64x2::splat(3.0)) } + unsafe { simd_fsqrt(simd_fabs(a)) };
    r.reduce_sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_cast(k: u32) -> u32 {
    let b = unsafe { &*(&raw const BUF) };
    let v8 = ld::<u8x8>(0);
    let w16: u16x8 = v8.cast();
    let s8 = i8x8::from_slice(unsafe { core::slice::from_raw_parts(b.as_ptr() as *const i8, 8) });
    let ws: i16x8 = s8.cast();
    let n8: u8x8 = (w16 * u16x8::splat(k as u16 | 1)).cast();
    let w32: u32x4 = u16x4::from_array([1000, 2000, 3000, 65000 + k as u16 % 500]).cast();
    let f: f32x4 = w32.cast();
    let g = f * f32x4::splat(0.37);
    let i: i32x4 = g.cast();
    let u: u32x4 = (g - f32x4::splat(200.0)).cast(); // saturating negative -> 0
    let d: f64x2 = f32x2::from_array([g[0], g[1]]).cast();
    let d2: f64x2 = d * f64x2::splat(1e10);
    let i2: i32x2 = d2.cast(); // saturates
    let h: f32x2 = d2.cast();
    let w64: u64x2 = u32x2::from_array([k, 0xffff_fff0]).cast();
    let nn: u16x4 = u32x4::from_array([0x12345, 0xabcde, 0x7fffff, k]).cast();
    let r: [u32; 8] = [
        n8.to_array().iter().fold(0u32, |a, &x| a * 7 + x as u32),
        ws.to_array().iter().fold(0u32, |a, &x| a * 7 + x as u32),
        i.to_array().iter().fold(0u32, |a, &x| a * 7 + x as u32),
        u.to_array().iter().fold(0u32, |a, &x| a * 7 + x),
        (i2[0] as u32) ^ (i2[1] as u32) << 1,
        h[0].to_bits() ^ h[1].to_bits(),
        (w64[0] ^ w64[1] >> 3) as u32,
        nn.to_array().iter().fold(0u32, |a, &x| a * 7 + x as u32),
    ];
    fold32(&r)
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_shuffle(k: u32) -> u32 {
    let b = unsafe { &*(&raw const BUF) };
    let x = ld::<u8x16>(0);
    let y = ld::<u8x16>(16);
    let (lo, hi) = x.interleave(y);
    let r = x.reverse();
    let z = simd_swizzle!(x, y, [0, 17, 2, 19, 4, 21, 6, 23, 8, 25, 10, 27, 12, 29, 14, 31]);
    let w = u32x4::from_array([k, k + 1, k + 2, k + 3]);
    let w2 = simd_swizzle!(w, [3, 3, 0, 1]);
    let q = u16x8::from_slice(unsafe { core::slice::from_raw_parts(b.as_ptr() as *const u16, 8) });
    let q2: u16x4 = simd_swizzle!(q, [7, 5, 1, 0]);
    let mut acc = 0u32;
    for v in [lo, hi, r, z] {
        acc = fold32(&[acc, v.to_array().iter().fold(0u32, |a, &c| a.wrapping_mul(131) + c as u32)]);
    }
    fold32(&[acc, w2.to_array().iter().fold(0u32, |a, &c| a.wrapping_mul(131) + c),
             q2.to_array().iter().fold(0u32, |a, &c| a.wrapping_mul(131) + c as u32)])
}

#[unsafe(no_mangle)]
pub extern "C" fn ops_bits() -> u32 {
    let b = unsafe { &*(&raw const BUF) };
    let x = ld::<u8x16>(0);
    let p = x.count_ones().to_array();
    let m = x.simd_gt(u8x16::splat(127));
    let mask = m.to_bitmask() as u32;
    let any = m.any() as u32;
    let all = m.all() as u32;
    let w = u32x4::from_slice(unsafe { core::slice::from_raw_parts(b.as_ptr() as *const u32, 4) });
    let lz = w.leading_zeros().to_array();
    let sw = w.swap_bytes().to_array();
    fold32(&[fold32(&p.map(|x| x as u32)), mask, any, all, fold32(&lz), fold32(&sw)])
}

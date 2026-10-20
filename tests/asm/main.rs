#![cfg_attr(target_arch = "aarch64", feature(portable_simd))]

use std::arch::{asm, global_asm};

#[cfg(all(target_arch = "x86_64", unix))]
global_asm!(".globl pliron_ga_add\npliron_ga_add:\n    lea rax, [rdi + rsi]\n    ret");
#[cfg(all(target_arch = "x86_64", windows))]
global_asm!(".globl pliron_ga_add\npliron_ga_add:\n    lea rax, [rcx + rdx]\n    ret");
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
    clmul();
    fcmp();
    minmax_prefetch();
}

#[cfg(target_arch = "x86_64")]
fn minmax_prefetch() {
    use std::arch::x86_64::*;
    let v = [3.0f32, -1.0, 7.5, 0.25];
    unsafe {
        _mm_prefetch::<_MM_HINT_T0>(v.as_ptr().cast());
        _mm_prefetch::<_MM_HINT_NTA>(v.as_ptr().cast());
        let (a, b) = (_mm_loadu_ps(v.as_ptr()), _mm_set_ps(1.0, 1.0, 1.0, 1.0));
        let (mut lo, mut hi) = ([0f32; 4], [0f32; 4]);
        _mm_storeu_ps(lo.as_mut_ptr(), _mm_min_ps(a, b));
        _mm_storeu_ps(hi.as_mut_ptr(), _mm_max_ps(a, b));
        assert_eq!((lo, hi), ([1.0, -1.0, 1.0, 0.25], [3.0, 1.0, 7.5, 1.0]));
        let mut d = [0f64; 2];
        _mm_storeu_pd(
            d.as_mut_ptr(),
            _mm_min_pd(_mm_set_pd(2.0, -4.0), _mm_set1_pd(0.5)),
        );
        assert_eq!(d, [-4.0, 0.5]);
    }
    println!("min/prefetch ok");
}

// At -O0 the predicate immediate reaches the backend as a runtime value.
#[cfg(target_arch = "x86_64")]
fn fcmp() {
    use std::arch::x86_64::*;
    if !is_x86_feature_detected!("avx") {
        return;
    }
    #[target_feature(enable = "avx")]
    fn f<const P: i32>(a: [f32; 4], b: [f32; 4]) -> [u32; 4] {
        unsafe {
            std::mem::transmute(_mm_cmp_ps::<P>(
                std::mem::transmute(a),
                std::mem::transmute(b),
            ))
        }
    }
    let (a, b) = ([1.0, 2.0, f32::NAN, 4.0], [2.0, 2.0, 1.0, 3.0]);
    unsafe {
        assert_eq!(f::<_CMP_LT_OQ>(a, b), [!0, 0, 0, 0]);
        assert_eq!(f::<_CMP_LE_OQ>(a, b), [!0, !0, 0, 0]);
        assert_eq!(f::<_CMP_UNORD_Q>(a, b), [0, 0, !0, 0]);
        assert_eq!(f::<_CMP_TRUE_UQ>(a, b), [!0; 4]);
    }
    if is_x86_feature_detected!("avx512f") {
        #[target_feature(enable = "avx512f")]
        fn g<const P: i32>(a: [f32; 16], b: [f32; 16]) -> u16 {
            unsafe { _mm512_cmp_ps_mask::<P>(std::mem::transmute(a), std::mem::transmute(b)) }
        }
        let a: [f32; 16] = std::array::from_fn(|i| i as f32);
        unsafe {
            assert_eq!(g::<_CMP_LT_OQ>(a, [8.0; 16]), 0x00ff);
            assert_eq!(g::<_CMP_GE_OQ>(a, [8.0; 16]), 0xff00);
        }
        println!("avx512 cmp ok");
    }
    println!("cmp ok");
}

#[cfg(target_arch = "x86_64")]
fn clmul_ref(a: u64, b: u64) -> u128 {
    (0..64)
        .filter(|i| b >> i & 1 == 1)
        .fold(0, |r, i| r ^ (a as u128) << i)
}

// crc32fast picks these paths at runtime; the intrinsics used to be `ud2` stubs.
#[cfg(target_arch = "x86_64")]
fn clmul() {
    use std::arch::x86_64::*;
    let (a, b) = (
        [0x8000_0000_0000_0001u64, 0x1234_5678_9abc_def0],
        [0xffff_ffff_0000_0001u64, 0x0f0f],
    );
    let want = |i: usize, j: usize| clmul_ref(a[i], b[j]);
    let lanes = |v: __m128i| unsafe { std::mem::transmute::<__m128i, u128>(v) };
    if is_x86_feature_detected!("pclmulqdq") {
        #[target_feature(enable = "pclmulqdq")]
        fn f<const I: i32>(a: __m128i, b: __m128i) -> __m128i {
            _mm_clmulepi64_si128::<I>(a, b)
        }
        let (va, vb) = unsafe {
            (
                std::mem::transmute::<[u64; 2], __m128i>(a),
                std::mem::transmute(b),
            )
        };
        unsafe {
            assert_eq!(lanes(f::<0x00>(va, vb)), want(0, 0));
            assert_eq!(lanes(f::<0x01>(va, vb)), want(1, 0));
            assert_eq!(lanes(f::<0x10>(va, vb)), want(0, 1));
            assert_eq!(lanes(f::<0x11>(va, vb)), want(1, 1));
        }
        println!("pclmulqdq ok");
    }
    if is_x86_feature_detected!("vpclmulqdq") && is_x86_feature_detected!("avx") {
        #[target_feature(enable = "vpclmulqdq,avx")]
        fn g(a: [u64; 4], b: [u64; 4]) -> [u128; 2] {
            unsafe {
                let r = _mm256_clmulepi64_epi128::<0x10>(
                    std::mem::transmute(a),
                    std::mem::transmute(b),
                );
                std::mem::transmute(r)
            }
        }
        let r = unsafe { g([a[0], a[1], a[1], a[0]], [b[0], b[1], b[1], b[0]]) };
        assert_eq!(r, [want(0, 1), clmul_ref(a[1], b[0])]);
        println!("vpclmulqdq ok");
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32s() -> (u32, u32, u32, u32, u32, u32) {
    use std::arch::aarch64::*;
    let b8 = b"12345678";
    let (mut c, mut r) = (!0u32, [0u32; 6]);
    for &b in b8 {
        c = __crc32b(c, b);
    }
    r[0] = c ^ !0;
    let mut c = !0u32;
    for w in b8.chunks(2) {
        c = __crc32h(c, u16::from_le_bytes([w[0], w[1]]));
    }
    r[1] = c ^ !0;
    let mut c = !0u32;
    for w in b8.chunks(4) {
        c = __crc32w(c, u32::from_le_bytes(w.try_into().unwrap()));
    }
    r[2] = c ^ !0;
    let mut c = !0u32;
    c = __crc32d(c, u64::from_le_bytes(*b8));
    r[3] = c ^ !0;
    let mut c = !0u32;
    for &b in b"12345678" {
        c = __crc32cb(c, b);
    }
    r[4] = c ^ !0;
    let mut c = !0u32;
    c = __crc32cd(c, u64::from_le_bytes(*b8));
    r[5] = c ^ !0;
    (r[0], r[1], r[2], r[3], r[4], r[5])
}

#[cfg(target_arch = "aarch64")]
fn neon_stubs() {
    use std::arch::aarch64::*;
    unsafe {
        let v8 = |a: [u8; 16]| std::mem::transmute::<_, uint8x16_t>(a);
        let v16 = |a: [u16; 8]| std::mem::transmute::<_, uint16x8_t>(a);
        let v32 = |a: [u32; 4]| std::mem::transmute::<_, uint32x4_t>(a);
        let i16v = |a: [i16; 8]| std::mem::transmute::<_, int16x8_t>(a);
        let i32v = |a: [i32; 4]| std::mem::transmute::<_, int32x4_t>(a);
        let a16 = |v: int16x4_t| std::mem::transmute::<_, [i16; 4]>(v);
        let a8 = |v: uint8x8_t| std::mem::transmute::<_, [u8; 8]>(v);
        // pairwise add-long: uaddlp (ret type is the wide one)
        let lp: [u16; 8] = std::mem::transmute(vpaddlq_u8(v8([1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])));
        assert_eq!(lp, [3, 7, 11, 15, 19, 23, 27, 31]);
        let lp2: [u32; 4] = std::mem::transmute(vpaddlq_u16(v16([100u16, 200, 300, 400, 500, 600, 700, 800])));
        assert_eq!(lp2, [300, 700, 1100, 1500]);
        // saturating doubling multiply-high: sqdmulh
        let m = i16v([0x4000i16, -0x4000, 0x7fff, -0x8000, 1, -1, 0x1234, 0x4000]);
        let n = i16v([0x4000i16, 0x4000, 0x7fff, -0x8000, -1, -1, -0x4000, 0x2000]);
        let dm: [i16; 8] = std::mem::transmute(vqdmulhq_s16(m, n));
        assert_eq!(dm, [0x2000, -0x2000, 0x7ffe, 0x7fff, -1, 0, -0x91a, 0x1000]);
        let m32 = i32v([0x4000_0000i32, -0x4000_0000, 0x7fff_ffff, -0x8000_0000i32]);
        let dm32: [i32; 4] = std::mem::transmute(vqdmulhq_s32(m32, m32));
        assert_eq!(dm32, [0x2000_0000, 0x2000_0000, 0x7fff_fffe, 0x7fff_ffff]);
        // saturating narrow: sqxtn / uqxtn
        let w = i32v([0x1234i32, 0x8_0000, -1, -0x9_0000]);
        assert_eq!(a16(vqmovn_s32(w)), [0x1234, 0x7fff, -1, -0x8000]);
        let usat = vqmovn_u16(v16([1u16, 0x200, 0xffff, 300, 0, 255, 256, 0x8000]));
        assert_eq!(a8(usat), [1, 0xff, 0xff, 0xff, 0, 0xff, 0xff, 0xff]);
        // saturating shift-right unsigned narrow: sqshrun (shift arrives as an arg)
        let sh = i32v([0x1000i32, -1, 0x7fff_ffff, 0x4000_0000]);
        let shr: [u16; 4] = std::mem::transmute(vqshrun_n_s32::<4>(sh));
        assert_eq!(shr, [0x100, 0, 0xffff, 0xffff]);
        let shr_r: [u16; 4] = std::mem::transmute(vqrshrun_n_s32::<4>(i32v([0x1080i32, -1, 0x7fff_ffff, 0x4000_0000])));
        assert_eq!(shr_r, [0x108, 0, 0xffff, 0xffff]);
        // pairwise add-long and accumulate: uadalp (acc v0, narrow arg v1)
        let acc = i32v([100, 200, 300, 400]);
        let alp: [i32; 4] = std::mem::transmute(vpadalq_s16(acc, i16v([1, 2, 3, 4, 5, 6, 7, 8])));
        assert_eq!(alp, [103, 207, 311, 415]);
        // generic vector intrinsics: llvm.roundeven / llvm.fptoui.sat
        use std::simd::{f32x4, num::SimdFloat, StdFloat};
        let r = f32x4::from_array([1.6f32, -1.6, 0.5, -0.5]).round_ties_even();
        assert_eq!(r.to_array(), [2.0, -2.0, 0.0, -0.0]);
        let c = f32x4::from_array([1.9f32, -1.9, 4e9, f32::NAN]).cast::<i32>();
        assert_eq!(c.to_array(), [1, -1, 2147483647, 0]);
        let _ = v32([0; 4]);
    }
    println!("neon stubs ok");
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
    if std::arch::is_aarch64_feature_detected!("crc") {
        let r = unsafe { crc32s() };
        assert_eq!([r.0, r.1, r.2, r.3], [0x9ae0daaf; 4]);
        assert_eq!([r.4, r.5], [0x6087809a; 2]);
        println!("crc32 ok");
    }
    neon_stubs();
}

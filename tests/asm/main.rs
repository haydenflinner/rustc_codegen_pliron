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
    clmul();
}

fn clmul_ref(a: u64, b: u64) -> u128 {
    (0..64).filter(|i| b >> i & 1 == 1).fold(0, |r, i| r ^ (a as u128) << i)
}

// crc32fast picks these paths at runtime; the intrinsics used to be `ud2` stubs.
fn clmul() {
    use std::arch::x86_64::*;
    let (a, b) = ([0x8000_0000_0000_0001u64, 0x1234_5678_9abc_def0], [0xffff_ffff_0000_0001u64, 0x0f0f]);
    let want = |i: usize, j: usize| clmul_ref(a[i], b[j]);
    let lanes = |v: __m128i| unsafe { std::mem::transmute::<__m128i, u128>(v) };
    if is_x86_feature_detected!("pclmulqdq") {
        #[target_feature(enable = "pclmulqdq")]
        fn f<const I: i32>(a: __m128i, b: __m128i) -> __m128i {
            _mm_clmulepi64_si128::<I>(a, b)
        }
        let (va, vb) = unsafe { (std::mem::transmute::<[u64; 2], __m128i>(a), std::mem::transmute(b)) };
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
                let r = _mm256_clmulepi64_epi128::<0x10>(std::mem::transmute(a), std::mem::transmute(b));
                std::mem::transmute(r)
            }
        }
        let r = unsafe { g([a[0], a[1], a[1], a[0]], [b[0], b[1], b[1], b[0]]) };
        assert_eq!(r, [want(0, 1), clmul_ref(a[1], b[0])]);
        println!("vpclmulqdq ok");
    }
}

//! wasm32 smoke test: a no_std cdylib exercising ints, i128, floats, memory, calls and statics.
#![no_std]
#![feature(core_intrinsics)]
#![allow(internal_features, unused_unsafe)]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[unsafe(no_mangle)]
pub extern "C" fn fib(n: u32) -> u32 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

#[unsafe(no_mangle)]
pub extern "C" fn sum_squares(n: u32) -> u64 {
    (1..=n as u64).map(|x| x * x).sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn wide_mul(a: u64, b: u64) -> u64 {
    let p = a as u128 * b as u128;
    (p >> 64) as u64 ^ p as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn byte_ops(x: u32) -> u32 {
    let b = x as u8;
    let s = (x as i8) >> 2;
    (b.wrapping_add(200) as u32) << 16 | (s as u8 as u32) << 8 | b.count_ones()
}

#[unsafe(no_mangle)]
pub extern "C" fn hypot(a: f64, b: f64) -> f64 {
    unsafe { core::intrinsics::sqrtf64(a * a + b * b) }
}

static mut COUNTER: u32 = 0;
static TABLE: [u16; 5] = [3, 1, 4, 1, 5];

#[unsafe(no_mangle)]
pub extern "C" fn bump(by: u32) -> u32 {
    unsafe {
        COUNTER += by;
        COUNTER
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn table_sum() -> u32 {
    TABLE.iter().map(|&x| x as u32).sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn sort_check(seed: u32) -> u32 {
    let mut a = [0u32; 32];
    let mut s = seed;
    for x in a.iter_mut() {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        *x = s >> 16;
    }
    a.sort_unstable();
    a.windows(2).all(|w| w[0] <= w[1]) as u32 * a[31]
}

fn apply(f: fn(u32) -> u32, x: u32) -> u32 {
    f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn indirect(x: u32) -> u32 {
    let fs: [fn(u32) -> u32; 2] = [|x| x + 1, |x| x * 3];
    apply(fs[(x & 1) as usize], x)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmt_len(x: u32) -> u32 {
    use core::fmt::Write;
    struct W([u8; 64], usize);
    impl Write for W {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0[self.1..self.1 + s.len()].copy_from_slice(s.as_bytes());
            self.1 += s.len();
            Ok(())
        }
    }
    let mut w = W([0; 64], 0);
    write!(w, "x={x} f={:.3}", x as f64 / 7.0).unwrap();
    w.1 as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn eq_bytes_check(x: u32) -> u32 {
    // raw_eq / compare_bytes with constant n: inline chunked equality
    // must match at every chunk width (8/4/2/1) and odd tails.
    let mut a = [0u8; 33];
    for (i, v) in a.iter_mut().enumerate() {
        *v = (i as u32).wrapping_mul(x).rotate_left((i % 13) as u32) as u8;
    }
    let mut b = a;
    let mut hits = 0u32;
    // compare_bytes, dynamic n: all-equal prefixes.
    for flip in 0..33usize {
        if a[..=flip] == b[..=flip] {
            hits += 1;
        }
    }
    // compare_bytes, constant n: inline equal scans.
    if a[..1] == b[..1] {
        hits += 1;
    }
    if a[..7] == b[..7] {
        hits += 1;
    }
    if a[..16] == b[..16] {
        hits += 1;
    }
    if a[..31] == b[..31] {
        hits += 1;
    }
    // constant n, non-equal: the slow-path branch returns ordering.
    b[5] ^= 0x5a;
    if a[..6] != b[..6] {
        hits += 1;
    }
    if a[..5] == b[..5] {
        hits += 1;
    }
    b[5] = a[5];
    // raw_eq n=32 (inline cap): equal and one-byte-different.
    let a32: &[u8; 32] = a[..32].try_into().unwrap();
    let b32: &[u8; 32] = b[..32].try_into().unwrap();
    if a32 == b32 {
        hits += 1;
    }
    let mut c = *a32;
    c[31] ^= 1;
    if *a32 != c {
        hits += 1;
    }
    // raw_eq n=33 (> cap): libcall path still works.
    if a == b {
        hits += 1;
    }
    // ordering via memcmp: a < c2 where c2 differs only at the last byte.
    let mut c2 = a;
    c2[32] = c2[32].wrapping_add(1);
    if a[..] < c2[..] {
        hits += 1;
    }
    hits
}

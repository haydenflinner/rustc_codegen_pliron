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

//! wasm emit smoke, side A: recursion, floats, statics, an indirect call and
//! a cross-object call into b.rs. No_std so nothing is imported but memory.
#![no_std]
#![feature(core_intrinsics)]
#![allow(internal_features, unused_unsafe)]

#[panic_handler]
fn p(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

unsafe extern "C" {
    fn helper(x: u32) -> u32;
}

static BLOB: [u32; 8] = [1, 2, 3, 5, 8, 13, 21, 34];

#[unsafe(no_mangle)]
pub extern "C" fn fib(n: u32) -> u32 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

#[unsafe(no_mangle)]
pub extern "C" fn sum_sq(n: u32) -> u64 {
    (1..=n as u64).map(|x| x * x).sum()
}

#[unsafe(no_mangle)]
pub extern "C" fn hypot(a: f64, b: f64) -> f64 {
    unsafe { core::intrinsics::sqrtf64(a * a + b * b) }
}

#[unsafe(no_mangle)]
pub extern "C" fn statics(i: u32) -> u32 {
    BLOB[(i & 7) as usize] * 3
}

fn inc(x: u32) -> u32 {
    x + 1
}
fn dec(x: u32) -> u32 {
    x - 1
}

/// Both an indirect table call (`f`) and a call into the other object.
#[unsafe(no_mangle)]
pub extern "C" fn indirect(x: u32) -> u32 {
    let f: fn(u32) -> u32 = if x & 1 == 1 { inc } else { dec };
    f(x) + unsafe { helper(x) }
}

/// Cross-object call, unaliased.
#[unsafe(no_mangle)]
pub extern "C" fn crosscall(x: u32) -> u32 {
    unsafe { helper(x).wrapping_mul(2) }
}

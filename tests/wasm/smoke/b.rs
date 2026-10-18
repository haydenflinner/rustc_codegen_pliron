//! wasm emit smoke, side B: the callee side of the cross-object calls.
#![no_std]

#[panic_handler]
fn p(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[unsafe(no_mangle)]
pub extern "C" fn helper(x: u32) -> u32 {
    x.wrapping_mul(3).rotate_left(5) ^ 0x1234
}

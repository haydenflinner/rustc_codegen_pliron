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

unsafe extern "C" {
    fn ctor_note(digit: u32);
}

extern "C" fn ctor_100() {
    unsafe { ctor_note(1) }
}

/// The module's first constructor, in the object linked last (see a.rs).
#[used]
#[unsafe(link_section = ".init_array.100")]
static CTOR_100: extern "C" fn() = ctor_100;

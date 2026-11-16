//! Unoptimized shapes the wasm byte peepholes got wrong, for
//! `harness/wasm/peepdiff.mjs`: compiled with PLIRON_OPT_BISECT=0 by
//! tests/wasm/smoke.sh into an object that is not linked.
#![no_std]

#[panic_handler]
fn p(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

/// `split_whitespace` is `Filter<Split<IsWhitespace>, IsNotEmpty>`: the
/// filter calls its predicate through `<&mut F as FnMut>::call_mut`,
/// whose unoptimized body stores the argument pointer to a slot, takes
/// the slot's address into the same local and loads the pointer back.
/// Keep-on-stack leaves `get B; get V; ..; set V; store`, and
/// store-to-load forwarding named the overwritten `V` as the stored value
/// (`2ba0f69`: the wrong `self` in a wasm-bindgen shim of dsa).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn peep_raw_words(p: *const u8, n: usize) -> usize {
    let s = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(p, n)) };
    s.split_whitespace().count()
}

// `PLIRON_X16=1`: x16 stays call-clobbered (a callee uses it as its own
// scratch and `bl` veneers clobber it), so no value may sit in x16
// across a `bl`. 28 values live across an opaque call.
#![no_std]
#[no_mangle]
pub fn presscall(a: &mut [u64; 40], n: usize) {
    let mut v = [0u64; 28];
    for i in 0..28 { v[i] = a[i]; }
    for k in 0..n {
        let s = unsafe { opaque(k as u64) };
        for i in 0..28 { v[i] = v[i].wrapping_mul(v[(i + 7) % 28] | 1).rotate_left((s + i as u64) as u32 & 63); }
    }
    for i in 0..28 { a[i] = v[i]; }
}
extern "C" { fn opaque(x: u64) -> u64; }
#[panic_handler] fn p(_: &core::panic::PanicInfo) -> ! { loop {} }

// `PLIRON_X18=1` on Linux: x18 is caller-saved scratch, so a `bl`
// clobbers it. 28 values live across an opaque call: more than the
// registers without x18, so without correct clobber accounting the
// allocator can leave one in x18 across the `bl`.
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
// extern: no recorded write set, so FASTCC cannot narrow the call's
// clobbers to the callee body — the full ABI set applies, which is
// where the x18 clobber lived before the fix.
extern "C" { fn opaque(x: u64) -> u64; }
#[panic_handler] fn p(_: &core::panic::PanicInfo) -> ! { loop {} }

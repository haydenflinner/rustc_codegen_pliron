// `PLIRON_X16=1` makes x16 allocatable on every aarch64 target (it is
// only linker-veneer scratch, unlike x18). 24 live values in a loop:
// more than the registers without x16.
#![no_std]
#[no_mangle]
pub fn press(a: &mut [u64; 32], n: usize) {
    let mut v = [0u64; 24];
    for i in 0..24 { v[i] = a[i]; }
    for k in 0..n {
        for i in 0..24 { v[i] = v[i].wrapping_mul(v[(i + 7) % 24] | 1).rotate_left((k + i) as u32 & 63); }
    }
    for i in 0..24 { a[i] = v[i]; }
}
#[panic_handler] fn p(_: &core::panic::PanicInfo) -> ! { loop {} }

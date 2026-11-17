// `PLIRON_INLINE_V8CAP` (wasm): `pick` is small and called from three
// places; inlining `mix` into it would take it past the (test-sized) cap,
// so the rule keeps `mix` a call there. `drive` has no callers: its own
// `mix` call is inlined either way. Compiled for wasm32 (`--emit obj`).
#![no_std]

fn mix(a: u32, b: u32) -> u32 {
    let mut x = a ^ b.rotate_left(7);
    x = x.wrapping_mul(0x9e37_79b9) ^ (x >> 15);
    x = x.wrapping_add(b).rotate_right(3) ^ a.wrapping_mul(31);
    let mut y = x;
    for k in 0..(b & 7) {
        y = y.rotate_left(k).wrapping_mul(0x85eb_ca6b) ^ (y >> 13);
        if y & 1 == 0 { y = y.wrapping_add(a); } else { y ^= b.wrapping_mul(0xc2b2_ae35); }
    }
    x ^ (y >> 11) ^ (a & 0xff)
}

#[inline(never)]
fn pick(v: &[u32], i: usize) -> u32 {
    if i < v.len() { mix(v[i], i as u32) } else { 0 }
}

#[unsafe(no_mangle)]
pub fn drive(v: &[u32]) -> u32 {
    pick(v, 0) ^ pick(v, 1) ^ pick(v, 2) ^ mix(v.len() as u32, 3)
}

#[unsafe(no_mangle)]
pub fn drive2(v: &[u32]) -> u32 {
    pick(v, 3)
}

#[unsafe(no_mangle)]
pub fn drive3(v: &[u32]) -> u32 {
    pick(v, 4).wrapping_add(1)
}

// r[verify clif.lower.shift128k]
// i128 shifts by a constant amount lower as 64-bit halves (rustc's i128
// shift amount is itself an i128, `uextend.i128 (iconst k)` in CLIF, which
// the backends only fold as a plain iconst). Every kind at the boundary
// amounts must match stock; `mulhi` is num-bigint-dig's `(x * y) >> 64`.

use std::hint::black_box;

#[inline(never)]
fn mulhi(x: u64, y: u64, c: u64) -> (u64, u64) {
    let z = x as u128 * y as u128 + c as u128;
    ((z >> 64) as u64, z as u64)
}

macro_rules! shifts {
    ($x:expr, $($k:literal),*) => {{
        let x: u128 = $x;
        let s = x as i128;
        let mut h = 0u128;
        $(
            h = h.rotate_left(7) ^ (x << $k) ^ (x >> $k).rotate_left(3) ^ ((s >> $k) as u128).rotate_left(11);
        )*
        h
    }};
}

#[inline(never)]
fn mix(x: u128) -> u128 {
    shifts!(x, 0, 1, 5, 31, 32, 63, 64, 65, 96, 100, 127)
}

fn main() {
    assert_eq!(mulhi(black_box(u64::MAX), black_box(u64::MAX), black_box(u64::MAX)), (u64::MAX, 0));
    assert_eq!(mulhi(black_box(0x1234_5678_9abc_def0), black_box(0xfedc_ba98_7654_3210), black_box(7)),
               (0x121fa00ad77d7422, 0x236d88fe5618cf07));
    let a = mix(black_box(0x8000_0000_0000_0001_dead_beef_0bad_f00d));
    let b = mix(black_box(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210));
    assert_eq!(a, 0x071648ea5f2a7df9e71c0018f955fd7a);
    assert_eq!(b, 0xbe3f8a3f1c9fb2478374ae911d56caad);
    println!("shift128k ok");
}

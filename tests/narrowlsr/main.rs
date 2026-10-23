// aarch64 isel folded `iadd x, (ushr y, K)` into `add w, w, w, lsr #K` for
// every scalar width. An i8/i16 value's register has unspecified bits above
// its width (here: the full 32-bit product of `mul`), and `lsr` shifted
// them into the result. Fuzz seeds 1939 and 3441 (rustlantis) hit it.
use std::hint::black_box;

#[inline(never)]
fn h16(a: u16) -> u16 {
    let m = a.wrapping_mul(a);
    (m >> 8).wrapping_add(m)
}

#[inline(never)]
fn h16l(a: u16, b: u16) -> u16 {
    let m = a.wrapping_mul(b);
    m.wrapping_add(m >> 3)
}

#[inline(never)]
fn h8(a: u8, b: u8) -> u8 {
    let m = a.wrapping_mul(b);
    (m >> 4).wrapping_add(b)
}

fn main() {
    let mut acc = 0u64;
    for i in 0..4096u32 {
        let a = black_box((i.wrapping_mul(40503) ^ 0xb0b0) as u16);
        let b = black_box((i.wrapping_mul(2654435761u32) >> 16) as u16);
        let m = a.wrapping_mul(a);
        assert_eq!(h16(a), (m >> 8).wrapping_add(m), "h16({a})");
        let m = a.wrapping_mul(b);
        assert_eq!(h16l(a, b), m.wrapping_add(m >> 3), "h16l({a},{b})");
        let (x, y) = (a as u8, b as u8);
        assert_eq!(h8(x, y), (x.wrapping_mul(y) >> 4).wrapping_add(y), "h8({x},{y})");
        acc = acc.wrapping_mul(31).wrapping_add(h16(a) as u64 + h16l(a, b) as u64 + h8(x, y) as u64);
    }
    // Stock rustc's value; the inline references above go through the same
    // isel rule, so only a pinned constant catches the bad fold.
    assert_eq!(acc, 16856741055146080618);
    println!("{acc}");
}

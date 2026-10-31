// r[verify iropt.constload.copy]
// PLIRON_CONSTCOPY: an alloca whose only write is a whole-object memcpy
// from an immutable global is replaced by the global, so constant-index
// loads fold (blake3's `MSG_SCHEDULE[round]`). The other shapes must keep
// their copy: one written after the copy, one whose address escapes. Every
// result is pinned, so stock and pliron must agree.

use std::hint::black_box;

const SCHED: [[usize; 8]; 3] = [
    [0, 1, 2, 3, 4, 5, 6, 7],
    [2, 6, 3, 0, 7, 1, 4, 5],
    [3, 4, 0, 7, 5, 2, 6, 1],
];

#[inline(always)]
fn round(st: &mut [u32; 4], m: &[u32; 8], r: usize) {
    let s = SCHED[r];
    st[0] = st[0].wrapping_add(m[s[0]]).rotate_right(7) ^ m[s[1]];
    st[1] = st[1].wrapping_add(m[s[2]]).rotate_right(9) ^ m[s[3]];
    st[2] = st[2].wrapping_add(m[s[4]]).rotate_right(13) ^ m[s[5]];
    st[3] = st[3].wrapping_add(m[s[6]]).rotate_right(18) ^ m[s[7]];
}

#[inline(never)]
fn mix(m: &[u32; 8]) -> [u32; 4] {
    let mut st = [1, 2, 3, 4];
    round(&mut st, m, 0);
    round(&mut st, m, 1);
    round(&mut st, m, 2);
    st
}

/// The copy is modified afterwards: it is not the global any more.
#[inline(never)]
fn written(i: usize) -> usize {
    let mut s = SCHED[1];
    s[3] = 99;
    s[i % 8]
}

#[inline(never)]
fn sink(p: &[usize; 8]) -> usize {
    p.iter().sum()
}

/// The copy's address escapes to a call: still read-only, but not
/// replaced (the call could compare addresses).
#[inline(never)]
fn escaped() -> usize {
    let s = SCHED[2];
    sink(black_box(&s))
}

fn main() {
    let m = black_box([0x11u32, 0x2222, 0x333333, 0x44444444, 5, 66, 777, 8888]);
    assert_eq!(mix(&m), [3487373654, 680028794, 52137394, 3504264201]);
    assert_eq!(written(black_box(3)), 99);
    assert_eq!(written(black_box(4)), 7);
    assert_eq!(escaped(), 28);
    println!("constcopy ok");
}

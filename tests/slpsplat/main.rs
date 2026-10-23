// Two loads of the same address are the same value only if nothing writes
// those bytes in between. slp's xpack paired `q[0] = *p; f(p); q[1] = *p`
// into `splat(first load)`, losing the call's write (rustlantis seeds 538,
// 683, 730 on nightly-2026-10-22).
use std::hint::black_box;

#[inline(never)]
fn bump(p: &mut u64) {
    *p = p.wrapping_mul(3).wrapping_add(black_box(1));
}

#[inline(never)]
fn pair(p: &mut u64, q: &mut [u64; 2]) {
    let a = *p;
    bump(p);
    let b = *p;
    q[0] = a;
    q[1] = b;
}

#[inline(never)]
fn quad(p: &mut u64, q: &mut [u64; 4]) {
    let a = *p;
    *black_box(&mut *p) += 5;
    let b = *p;
    bump(p);
    let c = *p;
    q[0] = a;
    q[1] = b;
    q[2] = c;
    q[3] = c;
}

fn main() {
    let mut p = 7u64;
    let mut q = [0u64; 2];
    pair(&mut p, &mut q);
    println!("{q:?}");
    assert_eq!(q, [7, 22]);
    let mut r = [0u64; 4];
    quad(&mut p, &mut r);
    println!("{r:?}");
    assert_eq!(r, [22, 27, 82, 82]);
}

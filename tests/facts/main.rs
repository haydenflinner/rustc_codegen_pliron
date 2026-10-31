// r[verify iropt.ipo.datalog]
// Bottom-up summaries (nowrite, nocapture, nnret, retarg, nounwind): test.sh
// checks the facts printed by PLIRON_FACTS=fx_ against the comments here, and
// main checks that code relying on them still behaves.

/// Retains `p` by storing it through `q`: param 0 captured, param 1 not.
#[inline(never)]
fn fx_stash(p: &mut [i32; 4], q: &mut *mut i32) {
    *q = p.as_mut_ptr();
}

/// Retains `p` through a local array and the return value: captured.
#[inline(never)]
fn fx_launder(p: &mut [i32; 4], i: usize) -> &mut [i32; 4] {
    let a = [p as *mut [i32; 4], p as *mut [i32; 4]];
    unsafe { &mut *a[i & 1] }
}

/// Writes through `p` without retaining it: not captured.
#[inline(never)]
fn fx_poke(p: &mut i32, v: i32) {
    *p = v;
}

/// Copies a value read through `p` into `q`: neither is captured, and
/// it only writes through `q`.
#[inline(never)]
fn fx_copy<'a>(p: &&'a i32, q: &mut &'a i32) {
    *q = *p;
}

/// Only reads: write-free, and `p` isn't captured.
#[inline(never)]
fn fx_sum(p: &[i32; 4]) -> i32 {
    p[0] + p[3]
}

/// Returns its param 1: returned(1), nonnull (a reference).
#[inline(never)]
fn fx_second<'a>(_a: &'a i32, b: &'a i32) -> &'a i32 {
    b
}

/// May return null.
#[inline(never)]
fn fx_maybe(p: &i32, b: bool) -> *const i32 {
    if b { p } else { std::ptr::null() }
}

/// Recursion with no calls out: can't unwind, write-free.
#[inline(never)]
fn fx_depth(n: u32) -> u32 {
    if n == 0 { 0 } else { 1 + fx_depth(n / 2) }
}

/// A volatile read: legal infinite-loop fuel and nondeterministic — not
/// willreturn even though it never writes.
#[inline(never)]
fn fx_vread(p: *const u32) -> u32 {
    unsafe { std::ptr::read_volatile(p) }
}

fn main() {
    let n = std::env::args().count() as i32; // 1

    let mut x = [n; 4];
    let mut r: *mut i32 = std::ptr::null_mut();
    fx_stash(&mut x, &mut r);
    unsafe { *r = 99 };
    assert_eq!(x[0], 99);

    let mut y = [n; 4];
    let q = fx_launder(&mut y, n as usize - 1);
    q[0] += 10;
    assert_eq!(y[0] + y[1], 12);

    let mut z = 0;
    fx_poke(&mut z, 5);
    assert_eq!(z, 5);
    let (one, two) = (n, n + 1);
    let mut c = &one;
    fx_copy(&&two, &mut c);
    assert_eq!(*c, 2);
    assert_eq!(fx_sum(&[n, 2, 3, 4]), 5);
    assert_eq!(*fx_second(&one, &two), 2);
    assert!(fx_maybe(&z, n == 2).is_null());
    assert_eq!(fx_depth(1 << (n + 3)), 5);
    let v = n as u32;
    assert_eq!(fx_vread(&v), 1);
    println!("ok");
}

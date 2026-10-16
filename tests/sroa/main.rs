#![feature(portable_simd)]

// PLIRON_SROA_PIN: a non-copyable call arg (callee writes through it or
// retains it) with a known pointee extent pins only that byte range of
// the caller's alloca — the slices covering it share one `pin_a` stack
// object and every other slice still promotes. The callee's writes and
// pointers retained past the call must observe the same memory the
// caller's promoted fields see: a wrong retarget offset or align
// silently reads/writes the wrong bytes.

use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

static SAVED: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Rec {
    a: u64,
    b: u64,
    c: u64,
    tag: u8,
    pad: [u8; 7],
    d: u64,
    inner: Inner,
}

#[repr(C)]
struct Inner {
    x: u64,
    y: u64,
}

// Writes through the arg: fails nowrite inference -> not copyable -> pin.
#[inline(never)]
fn wr(p: &mut u64, v: u64) {
    *p = v;
}

// Retains the arg in a global: fails nocapture -> not copyable -> pin.
#[inline(never)]
fn keep(p: &mut u64) {
    SAVED.store(p as *mut u64 as usize, Ordering::SeqCst);
}

// Reads the whole aggregate by ref: pins its entire extent.
#[inline(never)]
fn sum_rec(r: &Rec) -> u64 {
    r.a + r.b + r.c + r.d + r.inner.x + r.inner.y
}

#[inline(never)]
fn caller(x: u64) -> u64 {
    let mut r = Rec {
        a: x,
        b: 1,
        c: 2,
        tag: 7,
        pad: [0; 7],
        d: 4,
        inner: Inner { x: 5, y: 6 },
    };
    // Pin r.b's slice: the store must reach real memory the caller reads.
    wr(&mut r.b, 0xBEEF);
    // Pin r.c's slice and leak its address: a later write *through the
    // retained pointer* must land where the caller's r.c lives.
    keep(&mut r.c);
    // Nested-field gep: pins only inner.y's range.
    wr(&mut r.inner.y, 0xF00D);
    unsafe {
        *(SAVED.load(Ordering::SeqCst) as *mut u64) = 9;
    }
    // Whole-struct ref: covers the entire extent; all fields read back.
    let total = sum_rec(&r);
    // r.a / r.d / r.tag stay unpinned; reads must be unaffected by the
    // aliased writes above.
    r.a.wrapping_add(total).wrapping_add(r.tag as u64)
}

// Two pinned args on the same call must retarget independently.
#[inline(never)]
fn mix(p: &mut u64, q: &mut u64) {
    *p = *p + 100;
    *q = *q + 200;
}

#[inline(never)]
fn two_pins(x: u64) -> u64 {
    let mut r = Rec {
        a: x,
        b: 10,
        c: 20,
        tag: 1,
        pad: [0; 7],
        d: 30,
        inner: Inner { x: 40, y: 50 },
    };
    mix(&mut r.b, &mut r.inner.x);
    r.a + r.b + r.c + r.d + r.inner.x + r.inner.y
}

// Pinning a single array element must not disturb its neighbors.
#[inline(never)]
fn arr_elem(x: u64) -> u64 {
    let mut a = [x, 1, 2, 3, 4];
    keep(&mut a[2]);
    unsafe {
        *(SAVED.load(Ordering::SeqCst) as *mut u64) = 77;
    }
    a.iter().sum()
}

// sret2reg removes the sret pointer from a call's operand list;
// `CallInfo::arg_extent`/`nocap` were indexed by the old positions, so a
// by-value arg looked up the *previous* operand's extent. Here `big`'s pin
// read `x`'s (16,8) instead of its own (24,8): the `c` slice promoted out
// of the pinned copy and `rd` observed garbage in `big.c`.
#[repr(C)]
struct Big {
    a: u64,
    b: u64,
    c: u64,
}

#[repr(C)]
struct Ret {
    x: u64,
    y: u64,
    z: u64,
}

#[inline(never)]
fn rd(x: &[u64; 2], big: Big) -> Ret {
    // Passing `&big` onward keeps it non-copyable, so the arg stays a Pin
    // rather than an Escape.
    keep_ptr(&big);
    Ret {
        x: x[0].wrapping_add(big.a),
        y: big.b,
        z: big.c,
    }
}

#[inline(never)]
fn keep_ptr(p: &Big) {
    SAVED.store(p as *const Big as usize, Ordering::SeqCst);
}

#[inline(never)]
fn sret_caller(x: u64) -> u64 {
    let arr = [x, 9];
    let big = Big {
        a: 1,
        b: 2,
        c: 0xDEAD,
    };
    let r = rd(&arr, big);
    r.x.wrapping_add(r.y).wrapping_add(r.z)
}

// A nonzero memset covering a >16-byte typed slice must repeat the byte
// across the whole slice: the rewrite's pattern const is a u128, and
// wider slices used to get zeros above byte 16. `body`'s vector store
// makes a 32-byte typed slice; `write_bytes` covers it in one memset.
#[repr(C)]
struct SetWide {
    head: u64,
    body: core::simd::u64x4,
    tail: u64,
}

#[inline(never)]
fn memset_wide(x: u64) -> u64 {
    let mut w = SetWide {
        head: x,
        body: core::simd::u64x4::from_array([x, x + 1, x + 2, x + 3]),
        tail: x + 4,
    };
    unsafe {
        std::ptr::write_bytes(
            &mut w as *mut SetWide as *mut u8,
            0xA5,
            std::mem::size_of::<SetWide>(),
        );
    }
    let b = w.body;
    let lanes_bad = b
        .as_array()
        .iter()
        .map(|&v| (v != 0xA5A5A5A5A5A5A5A5) as u64)
        .sum::<u64>();
    lanes_bad
        + ((w.head != 0xA5A5A5A5A5A5A5A5) as u64)
        + ((w.tail != 0xA5A5A5A5A5A5A5A5) as u64)
}

fn main() {
    // caller: r.a=x, r.b=0xBEEF, r.c=9 (via SAVED), r.d=4, inner={5,0xF00D},
    // tag=7. total = x + 0xBEEF + 9 + 4 + 5 + 0xF00D; result adds x + 7.
    let x = black_box(3);
    let expect = x + (x + 0xBEEF + 9 + 4 + 5 + 0xF00D) + 7;
    assert_eq!(caller(x), expect);

    // two_pins: b=10+100=110, inner.x=40+200=240; sum = x+110+20+30+240+50.
    let x = black_box(2);
    assert_eq!(two_pins(x), x + 110 + 20 + 30 + 240 + 50);

    // arr_elem: a[2] becomes 77 via the retained pointer.
    let x = black_box(5);
    assert_eq!(arr_elem(x), x + 1 + 77 + 3 + 4);

    // sret_caller: r.x = x + 1, r.y = 2, r.z = 0xDEAD.
    let x = black_box(7);
    assert_eq!(sret_caller(x), (x + 1).wrapping_add(2).wrapping_add(0xDEAD));

    // memset_wide: every byte of w must be 0xA5 after write_bytes.
    let x = black_box(3);
    assert_eq!(memset_wide(x), 0);

    println!("sroa-pin ok");
}

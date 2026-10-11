// loadpre (loadfwd's PRE half) eliminates a load that is redundant after a
// join: available on every arm it becomes a block param; available on only
// some arms it gets a compensating load inserted on the missing edges
// (LLVM GVN's load PRE). These must hold:
//  - the merged value is the memory state at the join, so a may-aliasing
//    store or a writing call on one arm feeds the post-write value, not a
//    stale pre-write one;
//  - a clobber inside the join block before the load or a pred the load
//    can't be sunk into (no dominating address, non-branch terminator)
//    leaves the load alone;
//  - volatile and atomic accesses are never forwarded.
// Wrong answers here mean a stale value crossed an edge.

use std::hint::black_box;
use std::sync::atomic::{AtomicI32, Ordering};

#[inline(never)]
fn bump(q: *mut i32, d: i32) {
    unsafe { *q += d };
}

// The classic partial-availability shape: `*p` is loaded on the `cond`
// arm, so the join load is redundant there only — a compensating load on
// the `else` arm must read AFTER `*q += 10`, which may alias `*p`.
#[inline(never)]
fn edge(cond: bool, p: *const i32, q: *mut i32) -> i32 {
    let mut s = black_box(0);
    unsafe {
        if cond {
            s += *p;
        } else {
            *q += 10;
        }
        s + *p
    }
}

// A may-write call on the other arm: the join load can only forward the
// post-call state, either by a compensating load after the call returns
// or by not firing at all. Forwarding the pre-call value is a miscompile.
#[inline(never)]
fn call_arm(cond: bool, p: *const i32, q: *mut i32) -> i32 {
    let mut s = black_box(0);
    unsafe {
        if cond {
            s += *p;
        } else {
            bump(q, 100);
        }
        s + *p
    }
}

// Fully available on both arms (different values) -> block param merge.
#[inline(never)]
fn both_arms(cond: bool, p: *const i32, q: *const i32) -> i32 {
    unsafe {
        let a = if cond { *p } else { *q };
        // The join reload is redundant on both edges.
        a * 10 + if cond { *p } else { *q }
    }
}

// A clobber between the join and the load keeps the load live: the store
// through `q` may alias `p`, so the second `*p` must be re-read.
#[inline(never)]
fn clobbered(cond: bool, p: *const i32, q: *mut i32) -> i32 {
    unsafe {
        let a = if cond { *p } else { *p + 1 };
        *q = -7;
        a + *p
    }
}

// Loop-carried availability: `*p` is available on the header's back-edge,
// so promotion can thread it as a param — but the value must still be the
// one read each iteration.
#[inline(never)]
fn loop_sum(n: usize, p: *const i32) -> i32 {
    let mut s = black_box(0);
    unsafe {
        for _ in 0..n {
            s += *p;
        }
        s + *p
    }
}

// Volatile reads must not fold into an earlier non-volatile load.
#[inline(never)]
fn volatile_twice(p: *const i32) -> i32 {
    unsafe {
        let a = *p;
        let b = std::ptr::read_volatile(p);
        let c = *p;
        a + b + c
    }
}

// Atomics are not plain loads: an atomic load between two plain loads of
// the same word is a barrier the second load can't see across... but its
// own result is the real memory value.
#[inline(never)]
fn atomic_read(a: &AtomicI32) -> i32 {
    a.load(Ordering::SeqCst) + a.load(Ordering::SeqCst)
}

fn main() {
    let mut cell = 5i32;
    let p = &cell as *const i32;
    let q = &mut cell as *mut i32;

    // edge: else-arm store aliases p; the join must see 15, not a stale 5.
    assert_eq!(edge(false, p, q), 15);
    let cell = 5i32;
    let p = &cell as *const i32;
    assert_eq!(edge(true, p, std::ptr::null_mut()), 10);

    // call_arm: bump writes *p through the aliasing q; join must see 105.
    let mut cell = 5i32;
    let p = &cell as *const i32;
    let q = &mut cell as *mut i32;
    assert_eq!(call_arm(false, p, q), 105);
    let cell = 5i32;
    let p = &cell as *const i32;
    assert_eq!(call_arm(true, p, std::ptr::null_mut()), 10);

    // both_arms: cond -> 3*10+3 = 33; else -> 4*10+4 = 44.
    let (x, y) = (3i32, 4i32);
    assert_eq!(both_arms(true, &x, &y), 33);
    assert_eq!(both_arms(false, &x, &y), 44);

    // clobbered: join value is pre-store, the reload is post-store (-7).
    let mut cell = 9i32;
    let p = &cell as *const i32;
    let q = &mut cell as *mut i32;
    // cond: a = 9, *q = -7, a + *p = 9 + -7 = 2.
    assert_eq!(clobbered(true, p, q), 2);
    let mut cell = 9i32;
    let p = &cell as *const i32;
    let q = &mut cell as *mut i32;
    // else: a = 10, *q = -7, a + *p = 10 + -7 = 3.
    assert_eq!(clobbered(false, p, q), 3);

    let cell = 7i32;
    assert_eq!(loop_sum(0, &cell), 7);
    assert_eq!(loop_sum(5, &cell), 42);

    let cell = 11i32;
    assert_eq!(volatile_twice(&cell), 33);

    let a = AtomicI32::new(21);
    assert_eq!(atomic_read(&a), 42);

    println!("loadpre ok");
}

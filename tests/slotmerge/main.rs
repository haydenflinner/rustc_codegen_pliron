// Explicit stack slots may only merge when their live ranges are
// disjoint. The win is frame size — one aggregate per match arm costs
// the sum otherwise (rustc_parse::parse_item_kind allocated ~18 KB vs
// LLVM's <1 KB; deep parser recursion then SIGBUSed the stage2 rustc).
// The risk is aliasing live data, so this exercises:
//   * per-arm aggregates passed to sret / read-only callees (mergable)
//   * overlapping locals (must not share)
//   * a pointer stored to a global (must stay live to the end)
//   * a pointer carried through a block arg and read in a later block
use std::hint::black_box;

#[derive(Clone)]
struct Big {
    a: [u64; 8],
    b: [u64; 8],
    tag: u64,
}

#[inline(never)]
fn make(i: u64) -> Big {
    Big {
        a: [i; 8],
        b: [i + 1; 8],
        tag: i,
    }
}

#[inline(never)]
fn sum(b: &Big) -> u64 {
    b.a[0] + b.a[7] + b.b[3] + b.tag
}

fn expected(i: u64) -> u64 {
    i + i + (i + 1) + i
}

// Each arm's aggregates are only live inside the arm.
#[inline(never)]
fn dispatch(sel: u64, x: u64) -> u64 {
    match sel {
        0 => {
            let a = make(x);
            let b = make(x + 100);
            sum(&a) + sum(&b)
        }
        1 => {
            let a = make(x + 1);
            let b = make(x + 101);
            sum(&a) + sum(&b)
        }
        2 => {
            let a = make(x + 2);
            let b = make(x + 102);
            sum(&a) + sum(&b)
        }
        3 => {
            let a = make(x + 3);
            let b = make(x + 103);
            sum(&a) + sum(&b)
        }
        4 => {
            let a = make(x + 4);
            let b = make(x + 104);
            sum(&a) + sum(&b)
        }
        5 => {
            let a = make(x + 5);
            let b = make(x + 105);
            sum(&a) + sum(&b)
        }
        _ => x,
    }
}

// Both locals live to the end: they must not share memory.
#[inline(never)]
fn overlap(x: u64) -> u64 {
    let a = make(x);
    let b = make(x + 1);
    sum(&a) + sum(&b)
}

static mut SAVED: *const Big = std::ptr::null();

// `a`'s address escapes into a global: it must stay live to the end and
// may not share with `b`/`c`, or the saved read returns garbage.
#[inline(never)]
fn stash(x: u64) -> u64 {
    let a = make(x);
    unsafe { SAVED = &a };
    let b = make(x + 1);
    let c = make(x + 2);
    black_box(&b);
    black_box(&c);
    let _ = sum(&b);
    let _ = sum(&c);
    unsafe { (*SAVED).tag }
}

// The pointer flows through a block arg into a read in a later block:
// the slot is live across the edge, so `b` must not be folded onto it.
#[inline(never)]
fn carried(x: u64, cond: bool) -> u64 {
    let a = make(x);
    let p = &a as *const Big;
    if cond {
        let b = make(x + 7);
        black_box(&b);
        unsafe { (*p).tag + b.tag }
    } else {
        unsafe { (*p).a[0] }
    }
}

fn main() {
    let sel = black_box(0u64);
    let x = black_box(10u64);
    assert_eq!(dispatch(sel, x), expected(10) + expected(110));
    assert_eq!(dispatch(1, 10), expected(11) + expected(111));
    assert_eq!(dispatch(3, 0), expected(3) + expected(103));
    assert_eq!(dispatch(5, 100), expected(105) + expected(205));
    assert_eq!(dispatch(6, 42), 42);
    assert_eq!(overlap(7), expected(7) + expected(8));
    assert_eq!(stash(20), 20);
    assert_eq!(carried(30, true), 30 + 37);
    assert_eq!(carried(30, false), 30);
    println!("slotmerge ok");
}

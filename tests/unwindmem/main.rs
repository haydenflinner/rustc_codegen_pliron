// Memory a caller can read after catching a panic: stores through `&mut`
// params must not be held in registers or deleted across a call that may
// unwind. `a`: licm scalar promotion of `t.x` across a loop calling a
// panicking fn left the caller's copy stale (x=0, stock 19). `b`: Vec
// push, counters, drop guards, swaps and Option::take around the same
// call. Output must match stock LLVM rustc's (expected.out).
use std::hint::black_box;
use std::panic::{catch_unwind, AssertUnwindSafe};

mod a {
    use super::*;

    pub struct T { pub x: u64, pub y: u64, pub v: [u64; 4] }

    #[inline(never)]
    fn may_panic(i: u64, at: u64) { if black_box(i) == at { panic!("boom") } }

    // LICM scalar promotion of t.x across a loop with an unwinding call.
    #[inline(never)]
    fn loop_promote(t: &mut T, n: u64, at: u64) {
        for i in 0..n { t.x += 1; may_panic(i, at); }
    }
    // Store sinking: write before the call, conditionally rewrite after.
    #[inline(never)]
    fn sink(t: &mut T, c: bool, at: u64) {
        t.y = 5;
        may_panic(0, at);
        if c { t.y = 6 } else { t.y = 7 }
    }
    // Array element updates in a loop with the call between.
    #[inline(never)]
    fn arr(t: &mut T, at: u64) {
        for i in 0..4 { t.v[i] = i as u64 + 10; may_panic(i as u64, at); t.v[i] += 100; }
    }
    // Accumulate in place, call inside a nested condition.
    #[inline(never)]
    fn nested(t: &mut T, n: u64, at: u64) {
        for i in 0..n { t.x = t.x.wrapping_mul(3) + i; if i % 2 == 1 { may_panic(i, at); } }
    }
    // Load after a store then call; the reload must see the stored value even on the error path.
    #[inline(never)]
    fn reload(t: &mut T, at: u64) -> u64 {
        t.y = 41; may_panic(1, at); t.y += 1; t.y
    }

    pub fn run() {
        let mut out = Vec::new();
        for at in [0u64, 1, 3, 99] {
            let mut t = T { x: 0, y: 0, v: [0; 4] };
            let a = catch_unwind(AssertUnwindSafe(|| loop_promote(&mut t, 6, at))).is_ok();
            let b = catch_unwind(AssertUnwindSafe(|| sink(&mut t, at % 2 == 0, at))).is_ok();
            let c = catch_unwind(AssertUnwindSafe(|| arr(&mut t, at))).is_ok();
            let d = catch_unwind(AssertUnwindSafe(|| nested(&mut t, 7, at))).is_ok();
            let e = catch_unwind(AssertUnwindSafe(|| reload(&mut t, at))).ok();
            out.push(format!("at={at} ok={a}{b}{c}{d} e={e:?} x={} y={} v={:?}", t.x, t.y, t.v));
        }
        for l in out { println!("{l}"); }
    }

}

mod b {
    use super::*;

    #[inline(never)]
    fn may_panic(i: u64, at: u64) { if black_box(i) == at { panic!("boom") } }

    #[inline(never)]
    fn push_loop(v: &mut Vec<u64>, at: u64) { for i in 0..8 { v.push(i * 3); may_panic(i, at); } }

    pub struct Counter { pub n: u64, pub sum: u64 }
    #[inline(never)]
    fn count(c: &mut Counter, xs: &[u64], at: u64) {
        for &x in xs { c.n += 1; c.sum = c.sum.wrapping_add(x); may_panic(x, at); }
    }

    struct Guard<'a> { flag: &'a mut u64 }
    impl Drop for Guard<'_> { fn drop(&mut self) { *self.flag += 100; } }
    #[inline(never)]
    fn guarded(flag: &mut u64, at: u64) {
        *flag = 1;
        let g = Guard { flag };
        *g.flag += 10;
        may_panic(0, at);
        *g.flag += 1000;
    }

    #[inline(never)]
    fn swap_then_call(a: &mut [u64; 4], at: u64) {
        for i in 0..3 { a.swap(i, i + 1); may_panic(i as u64, at); }
    }

    #[inline(never)]
    fn opt_take(o: &mut Option<Box<u64>>, at: u64) -> u64 {
        let b = o.take();
        may_panic(0, at);
        *o = b.map(|x| Box::new(*x + 1));
        o.as_deref().copied().unwrap_or(0)
    }

    pub fn run() {
        for at in [0u64, 1, 2, 5, 99] {
            let mut v = Vec::new();
            let a = catch_unwind(AssertUnwindSafe(|| push_loop(&mut v, at))).is_ok();
            let mut c = Counter { n: 0, sum: 0 };
            let b = catch_unwind(AssertUnwindSafe(|| count(&mut c, &[3, 1, 4, 1, 5, 9, 2, 6], at))).is_ok();
            let mut f = 0u64;
            let g = catch_unwind(AssertUnwindSafe(|| guarded(&mut f, at))).is_ok();
            let mut arr = [1u64, 2, 3, 4];
            let s = catch_unwind(AssertUnwindSafe(|| swap_then_call(&mut arr, at))).is_ok();
            let mut o = Some(Box::new(7u64));
            let t = catch_unwind(AssertUnwindSafe(|| opt_take(&mut o, at))).ok();
            println!("at={at} {a}{b}{g}{s} v={v:?} c=({},{}) f={f} arr={arr:?} t={t:?} o={o:?}", c.n, c.sum);
        }
    }

}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    a::run();
    b::run();
}

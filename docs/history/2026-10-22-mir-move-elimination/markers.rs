// Storage markers in the shapes MoveElimination's storage reconstruction
// ([5/6]) emits: StorageLive on only one branch, StorageDead on every path
// (so dead-without-live on some), duplicated StorageDead, StorageLive sunk to
// the first write, and a borrowed local whose StorageDead sits after the loop.
// Two address-taken locals must keep distinct storage while both are live.
#![feature(custom_mir, core_intrinsics)]
#![allow(internal_features)]
use std::intrinsics::mir::*;

#[derive(Clone, Copy, Debug)]
pub struct B4 {
    pub a: u64,
    pub b: u64,
    pub c: u64,
    pub d: u64,
}

#[inline(never)]
pub fn fill(p: *mut B4, v: u64) {
    unsafe { *p = B4 { a: v, b: v + 1, c: v + 2, d: v + 3 } }
}

#[inline(never)]
pub fn sum(p: *const B4) -> u64 {
    unsafe { (*p).a + 3 * (*p).b + 5 * (*p).c + 7 * (*p).d }
}

#[custom_mir(dialect = "runtime", phase = "optimized")]
#[inline(never)]
pub fn fork_join(flag: bool, v: u64) -> u64 {
    mir! {
        type RET = u64;
        let x: B4;
        let y: B4;
        let px: *mut B4;
        let py: *mut B4;
        let u: ();
        let s: u64;
        let t: u64;
        {
            match flag {
                true => only_x,
                _ => join,
            }
        }
        only_x = {
            StorageLive(x);
            px = &raw mut x;
            Call(u = fill(px, v), ReturnTo(join), UnwindContinue())
        }
        join = {
            StorageLive(y);
            py = &raw mut y;
            Call(u = fill(py, v), ReturnTo(j2), UnwindContinue())
        }
        j2 = {
            Call(t = sum(py), ReturnTo(j3), UnwindContinue())
        }
        j3 = {
            match flag {
                true => use_x,
                _ => done,
            }
        }
        use_x = {
            Call(s = sum(px), ReturnTo(sum_x), UnwindContinue())
        }
        sum_x = {
            t = t + s;
            Goto(done)
        }
        done = {
            StorageDead(x);
            StorageDead(x);
            StorageDead(y);
            RET = t;
            Return()
        }
    }
}

// `a` is borrowed inside the loop; its StorageDead is after the loop.
// `b` is re-made each trip and dies at the bottom of the trip.
#[custom_mir(dialect = "runtime", phase = "optimized")]
#[inline(never)]
pub fn loop_borrowed(n: u64) -> u64 {
    mir! {
        type RET = u64;
        let a: B4;
        let b: B4;
        let pa: *mut B4;
        let pb: *mut B4;
        let i: u64;
        let w: u64;
        let s: u64;
        let t: u64;
        let u: ();
        let c: bool;
        {
            i = 0;
            RET = 0;
            StorageLive(a);
            pa = &raw mut a;
            Call(u = fill(pa, n), ReturnTo(head), UnwindContinue())
        }
        head = {
            StorageLive(b);
            pb = &raw mut b;
            w = i * 10;
            Call(u = fill(pb, w), ReturnTo(l1), UnwindContinue())
        }
        l1 = {
            Call(s = sum(pa), ReturnTo(l2), UnwindContinue())
        }
        l2 = {
            Call(t = sum(pb), ReturnTo(l3), UnwindContinue())
        }
        l3 = {
            StorageDead(b);
            RET = RET + s;
            RET = RET + t;
            Call(u = fill(pa, RET), ReturnTo(l4), UnwindContinue())
        }
        l4 = {
            i = i + 1;
            c = i < n;
            match c {
                true => head,
                _ => out,
            }
        }
        out = {
            StorageDead(a);
            Return()
        }
    }
}

fn main() {
    let mut h = 0u64;
    for v in 0..12u64 {
        let a = fork_join(v % 2 == 0, v);
        let b = loop_borrowed(v % 4 + 1);
        println!("{v}: {a} {b}");
        h = h.wrapping_mul(31).wrapping_add(a ^ b);
    }
    println!("hash {h}");
}

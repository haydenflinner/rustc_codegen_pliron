// Hand-written MIR in the shapes MoveElimination ([4/6], -Zmir-move-elimination)
// produces, so the pinned toolchain can push them through our backend today:
//  - call destinations projected into the return place (interior sret pointer),
//    including an enum payload written before SetDiscriminant;
//  - the return place's address escaping mid-function, with writes through it
//    in a callee, and plain writes before and after;
//  - no StorageLive/StorageDead anywhere;
//  - a callee writing into its own by-value (indirect) argument.
// Output must match stock rustc's byte for byte.
#![feature(custom_mir, core_intrinsics)]
#![allow(internal_features)]
use core::intrinsics::mir::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Big4 {
    pub a: u64,
    pub b: u64,
    pub c: u64,
    pub d: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outer2 {
    pub head: u64,
    pub inner: Big4,
    pub tail: [u64; 3],
}

#[inline(never)]
pub fn peek_ab(p: *mut Big4) -> u64 {
    unsafe { (*p).a.wrapping_mul(3).wrapping_add((*p).b) }
}

#[inline(never)]
pub fn poke_d(p: *mut Big4, v: u64) {
    unsafe { (*p).d = v }
}

// `b = make4(x)` with `b` unified into `_0`: the return place escapes while
// only partly written; a callee reads it, another writes through it.
#[custom_mir(dialect = "runtime", phase = "optimized")]
#[inline(never)]
pub fn make4(x: u64) -> Big4 {
    mir! {
        type RET = Big4;
        let p: *mut Big4;
        let s: u64;
        let u: ();
        let v: u64;
        {
            RET.a = x;
            RET.b = x + 1;
            p = &raw mut RET;
            Call(s = peek_ab(p), ReturnTo(bb1), UnwindContinue())
        }
        bb1 = {
            RET.c = s;
            v = s ^ 0x55;
            Call(u = poke_d(p, v), ReturnTo(bb2), UnwindContinue())
        }
        bb2 = {
            Return()
        }
    }
}

// Call destination at a non-zero offset inside the return place, then the
// field's address escapes and the rest of `_0` is written afterwards.
#[custom_mir(dialect = "runtime", phase = "optimized")]
#[inline(never)]
pub fn wrap(x: u64) -> Outer2 {
    mir! {
        type RET = Outer2;
        let p: *mut Big4;
        let s: u64;
        {
            RET.head = x * 7;
            Call(RET.inner = make4(x), ReturnTo(bb1), UnwindContinue())
        }
        bb1 = {
            p = &raw mut RET.inner;
            Call(s = peek_ab(p), ReturnTo(bb2), UnwindContinue())
        }
        bb2 = {
            RET.tail = [s, RET.inner.d, RET.head];
            Return()
        }
    }
}

// Enum payload written in place by a call, discriminant set afterwards.
#[custom_mir(dialect = "runtime", phase = "optimized")]
#[inline(never)]
pub fn wrap_opt(x: u64) -> Option<Outer2> {
    mir! {
        type RET = Option<Outer2>;
        let c: bool;
        {
            c = x == 13;
            match c {
                true => none,
                _ => some,
            }
        }
        none = {
            SetDiscriminant(RET, 0);
            Return()
        }
        some = {
            Call(place!(Field::<Outer2>(Variant(RET, 1), 0)) = wrap(x), ReturnTo(done), UnwindContinue())
        }
        done = {
            SetDiscriminant(RET, 1);
            Return()
        }
    }
}

// Two address-taken locals in a loop with no storage markers; `a` is written
// at the top and read at the bottom, `b` in between: their slots must not
// share storage. `_0` is also a call destination on every trip.
#[custom_mir(dialect = "runtime", phase = "optimized")]
#[inline(never)]
pub fn loopy(n: u64) -> Big4 {
    mir! {
        type RET = Big4;
        let j: u64;
        let i: u64;
        let acc: u64;
        let a: Big4;
        let b: Big4;
        let pa: *mut Big4;
        let pb: *mut Big4;
        let s: u64;
        let t: u64;
        let c: bool;
        {
            i = 0;
            acc = 0;
            Goto(head)
        }
        head = {
            Call(a = make4(i), ReturnTo(l1), UnwindContinue())
        }
        l1 = {
            j = i + 100;
            Call(b = make4(j), ReturnTo(l2), UnwindContinue())
        }
        l2 = {
            pb = &raw mut b;
            Call(t = peek_ab(pb), ReturnTo(l3), UnwindContinue())
        }
        l3 = {
            pa = &raw mut a;
            Call(s = peek_ab(pa), ReturnTo(l4), UnwindContinue())
        }
        l4 = {
            acc = acc + s;
            acc = acc + t;
            acc = acc + a.c;
            Call(RET = make4(acc), ReturnTo(l5), UnwindContinue())
        }
        l5 = {
            i = i + 1;
            c = i < n;
            match c {
                true => head,
                _ => out,
            }
        }
        out = {
            Return()
        }
    }
}

// A by-value (indirect) argument used as scratch by the callee: temps mapped
// into an argument local.
#[custom_mir(dialect = "runtime", phase = "optimized")]
#[inline(never)]
pub fn consume(v: Big4) -> u64 {
    mir! {
        type RET = u64;
        let p: *mut Big4;
        let s: u64;
        {
            v.a = v.c + v.d;
            p = &raw mut v;
            Call(s = peek_ab(p), ReturnTo(bb1), UnwindContinue())
        }
        bb1 = {
            RET = s + v.b;
            Return()
        }
    }
}

fn main() {
    let mut h = 0u64;
    for x in 0..20u64 {
        let b = make4(x);
        let w = wrap(x);
        let o = wrap_opt(x);
        let l = loopy(x % 5 + 1);
        let c = consume(Big4 { a: x, b: x + 2, c: x + 3, d: x + 4 });
        println!("{x}: {b:?} {w:?} {o:?} {l:?} {c}");
        h = h.wrapping_mul(31).wrapping_add(b.d ^ w.tail[0] ^ l.c ^ c);
    }
    println!("hash {h}");
}

//! SROA must not split an alloca whose by-value call arg is reached
//! through a loaded-back pointer (`flow`) without pinning the arg's whole
//! extent. `big` is written directly (its slices promote), `r = &mut big`
//! lives in memory (assigned twice), and only `big`'s tag and first field
//! are written through `r`. `sum(Move(*r))` passes `r`'s pointer itself
//! (Rust ABI, Indirect, `captures(none)` so "copyable"): SROA classified
//! it as an Escape, but the rewrite skips flow accesses, so no copy was
//! made and only the flow-touched bytes stayed in `pin_a` (16 of 168).
//! `sum` read the rest from beyond `pin_a` — garbage; with a callee that
//! writes its param, the caller's frame (fuzz seed 16787: a crash).
//! Compiled with `-Zmir-opt-level=0` so the MIR stays as written.
#![feature(custom_mir, core_intrinsics)]
#![allow(internal_features)]
extern crate core;
use core::intrinsics::mir::*;

pub enum Big {
    A(u8, i64),
    C(i64, [i64; 19]),
}

#[inline(never)]
fn sum(b: Big) -> i64 {
    match b {
        Big::A(t, x) => t as i64 + x,
        Big::C(h, a) => {
            let mut s = h;
            for (i, v) in a.iter().enumerate() {
                s = s.wrapping_add(*v * (i as i64 + 2));
            }
            s
        }
    }
}

#[custom_mir(dialect = "runtime", phase = "initial")]
#[inline(never)]
fn caller(x: i64) -> i64 {
    mir! {
        let big: Big;
        let r: &mut Big;
        let out: i64;
        let arr: [i64; 19];
        {
            arr = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20];
            big = Big::C(0, arr);
            r = &mut big;
            Goto(bb1)
        }
        bb1 = {
            r = &mut big;
            place!(Field::<i64>(Variant(*r, 1), 0)) = x;
            SetDiscriminant(*r, 1);
            Call(out = sum(Move(*r)), ReturnTo(bb2), UnwindUnreachable())
        }
        bb2 = {
            RET = out;
            Return()
        }
    }
}

fn main() {
    let x = std::hint::black_box(1i64);
    let got = caller(x);
    let want = (1..=20i64).map(|i| i * i).sum::<i64>();
    assert_eq!(got, want, "by-value arg moved through a stored &mut lost bytes");
    println!("ok {got}");
}

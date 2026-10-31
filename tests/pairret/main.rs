// A `PassMode::Pair` return with a bool half must hand the caller an i1
// immediate. The pair used to come back as the memory type `{i8, i8}`, so
// `!p.1` was `p.1 ^ 0xff` and `(!false) as u8` printed 255 (fuzz seed 18889).
#![feature(custom_mir, core_intrinsics)]
#![allow(internal_features, unnecessary_transmutes)]
use core::intrinsics::mir::*;
use std::hint::black_box;

#[inline(never)]
fn bb(a: bool) -> (bool, bool) {
    (a, a)
}
#[inline(never)]
fn ub(a: bool) -> (u32, bool) {
    (7, a)
}
#[inline(never)]
fn bu(a: bool) -> (bool, u64) {
    (a, 9)
}
#[inline(never)]
fn nested(a: bool) -> ((bool, bool),) {
    ((a, !a),)
}

#[inline(never)]
fn not_bb(a: bool) -> u8 {
    let p = bb(a);
    (!p.1) as u8 + 2 * (!p.0) as u8
}
#[inline(never)]
fn not_ub(a: bool) -> u8 {
    let p = ub(a);
    (!p.1) as u8
}
#[inline(never)]
fn not_bu(a: bool) -> u8 {
    let p = bu(a);
    (!p.0) as u8
}

// The fuzz shape: the pair stays an SSA operand and is projected twice.
#[custom_mir(dialect = "runtime", phase = "initial")]
#[inline(never)]
fn not_nested(a: bool) -> u8 {
    mir! {
        let p: ((bool, bool),);
        let n: bool;
        {
            Call(p = nested(a), ReturnTo(bb1), UnwindUnreachable())
        }
        bb1 = {
            n = !p.0.1;
            RET = n as u8;
            Return()
        }
    }
}

fn main() {
    for a in [false, true] {
        let a = black_box(a);
        let n = (!a) as u8;
        assert_eq!(not_bb(a), 3 * n);
        assert_eq!(not_ub(a), n);
        assert_eq!(not_bu(a), n);
        assert_eq!(not_nested(a), a as u8);
        // Hashing writes the bool's byte: it must be 0 or 1.
        assert_eq!(unsafe { std::mem::transmute::<bool, u8>(!bb(a).1) }, n);
    }
}

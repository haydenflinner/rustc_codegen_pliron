// `PLIRON_WASM_MASKELIM=1`: the borrow of `core::num::imp::bignum`'s
// `Big32x40::sub` is an i8 bool carried around the limb loop. Each use
// widens it with `& 255` unless the mask pass sees that the loop header's
// parameter only ever receives compares (`or` of two `lt_u`) and the
// constant 1. The back-edge argument is computed later in the body than
// the header, which the fixpoint must not read as "32 bits". Compiled for
// wasm32 (`--emit obj`); the stanza counts masks in `big_sub`.
#![no_std]
use core::cmp::max;
use core::iter;

pub struct Big {
    size: usize,
    base: [u32; 40],
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn big_sub(a: &mut Big, other: &Big) {
    let sz = max(a.size, other.size);
    let mut noborrow = true;
    for (x, y) in iter::zip(&mut a.base[..sz], &other.base[..sz]) {
        let (v, c) = (*x).carrying_add(!*y, noborrow);
        *x = v;
        noborrow = c;
    }
    assert!(noborrow);
    a.size = sz;
}

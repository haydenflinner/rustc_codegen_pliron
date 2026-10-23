// A store through a `noalias` param (`&mut self`) followed by a call that
// unwinds is visible to the caller that catches the panic, even when a later
// store would overwrite it on the normal path or the path ends in a trap.
// dse treated calls as barriers only for non-isolated memory and only
// `return` as exposing caller memory, so it deleted the store: hashbrown's
// `reserve_rehash` lost `items` when the hasher panicked mid-rehash
// (`hashmap_reserve_survives_panicking_build_hasher`, nightly-2026-10-22).
use std::hint::black_box;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub struct Table {
    pub items: u64,
    pub growth: u64,
}

#[inline(never)]
fn hash(x: u64) -> u64 {
    if black_box(x) == 7 {
        panic!("hasher panicked");
    }
    x.wrapping_mul(31)
}

// Overwritten on the normal path: the first store looks dead.
#[inline(never)]
fn update(t: &mut Table, n: u64) {
    t.items = n;
    let h = hash(n);
    t.items = h;
}

// The panic path ends in a trap after the call (`call; trap`).
#[inline(never)]
fn update_trap(t: &mut Table, n: u64) {
    t.growth = n;
    if black_box(n) == 7 {
        hash(n);
        unreachable!();
    }
    t.growth = n + 1;
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut t = Table { items: 0, growth: 0 };
    let r = catch_unwind(AssertUnwindSafe(|| update(&mut t, 7)));
    assert!(r.is_err());
    println!("items after caught panic: {}", t.items);
    assert_eq!(t.items, 7);
    let r = catch_unwind(AssertUnwindSafe(|| update_trap(&mut t, 7)));
    assert!(r.is_err());
    println!("growth after caught panic: {}", t.growth);
    assert_eq!(t.growth, 7);
    update(&mut t, 3);
    update_trap(&mut t, 3);
    assert_eq!((t.items, t.growth), (93, 4));
    println!("ok");
}

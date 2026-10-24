// Inlined helper chains: variables of inlined frames.
use std::hint::black_box;

#[inline(always)]
fn clamp_add(a: u32, b: u32, hi: u32) -> u32 {
    let s = a.saturating_add(b);
    let r = if s > hi { hi } else { s };
    sink(r); // BREAK
    r
}

#[inline(always)]
fn scale(v: u32, k: u32) -> u32 {
    let m = v * k;
    clamp_add(m, k, 1000)
}

#[inline(never)]
fn run(xs: &[u32], k: u32) -> u32 {
    let mut tot = 0;
    for &x in xs {
        tot += scale(x, k);
    }
    sink(tot); // BREAK
    tot
}

fn main() {
    let n = std::env::args().count() as u32;
    println!("{}", run(&[3, 50, 7], 6 + n));
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}

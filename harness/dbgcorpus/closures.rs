// Closures, iterator chains, generics.
use std::hint::black_box;

#[inline(never)]
fn apply<F: Fn(i32) -> i32>(f: F, xs: &[i32]) -> Vec<i32> {
    let mut out = Vec::new();
    for &x in xs {
        let y = f(x);
        out.push(y);
        if out.len() == 2 {
            sink(y); // BREAK
        }
    }
    out
}

#[inline(never)]
fn chain(xs: &[i32], lim: i32) -> i32 {
    let k = lim / 2;
    let s: i32 = xs.iter().filter(|&&x| x < lim).map(|&x| x * k).sum();
    let m = xs.iter().copied().max().unwrap_or(0);
    sink(m); // BREAK
    s + m
}

#[inline(never)]
fn largest<T: PartialOrd + Copy + std::fmt::Debug>(xs: &[T]) -> T {
    let mut best = xs[0];
    for &x in xs {
        if x > best {
            best = x;
        }
    }
    sink(&best); // BREAK
    best
}

fn main() {
    let n = std::env::args().count() as i32;
    let off = 100 * n;
    let v = apply(move |x| x + off, &[1, 2, 3]);
    println!("{:?}", v);
    println!("{}", chain(&[5, 1, 9, 3], 6 + n));
    println!("{} {}", largest(&[3u8, 9, 2]), largest(&[1.5f64, -2.0, 0.25]));
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}

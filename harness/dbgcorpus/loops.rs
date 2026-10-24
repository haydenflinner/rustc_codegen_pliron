// Numeric loops, mutable accumulators, branches.
use std::hint::black_box;

#[inline(never)]
fn sum_sq(xs: &[i64], bias: i64) -> i64 {
    let mut acc = 0i64;
    let mut odd = 0u32;
    for (i, &x) in xs.iter().enumerate() {
        let sq = x * x + bias;
        if sq % 2 == 1 {
            odd += 1;
        }
        acc += sq * i as i64;
        if i == 2 {
            sink(acc); // BREAK
        }
    }
    let avg = acc / xs.len().max(1) as i64;
    sink(odd); // BREAK
    avg + odd as i64
}

#[inline(never)]
fn collatz(mut n: u64) -> u32 {
    let start = n;
    let mut steps = 0u32;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        steps += 1;
        if steps == 5 {
            sink(start); // BREAK
        }
    }
    steps
}

#[inline(never)]
fn fib(k: u32) -> u64 {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..k {
        let t = a + b;
        a = b;
        b = t;
    }
    let ratio = b as f64 / a.max(1) as f64;
    sink(ratio); // BREAK
    a
}

fn main() {
    let v: Vec<i64> = (1..=6).collect();
    let n = std::env::args().count() as i64;
    println!("{}", sum_sq(&v, n));
    println!("{}", collatz(27 + n as u64));
    println!("{}", fib(20 + n as u32));
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}

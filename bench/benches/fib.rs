use std::hint::black_box;

#[inline(never)]
fn fib(n: u32) -> u64 {
    if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
}

pub fn run() -> u64 {
    let mut acc = 0u64;
    for _ in 0..12 {
        acc = acc.wrapping_add(fib(black_box(28)));
    }
    acc
}

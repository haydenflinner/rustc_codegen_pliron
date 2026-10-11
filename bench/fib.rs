use std::hint::black_box;
use std::time::Instant;

#[inline(never)]
fn fib(n: u32) -> u64 {
    if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    let t = Instant::now();
    let mut acc = 0u64;
    for _ in 0..12 {
        acc = acc.wrapping_add(fib(black_box(28)));
    }
    let d = t.elapsed();
    println!("{:.3} {}", d.as_secs_f64() * 1e3, acc);
}

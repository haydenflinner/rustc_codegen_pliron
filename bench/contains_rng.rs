use std::hint::black_box;
use std::time::Instant;
fn main() {
    let mut v: Vec<u32> = Vec::with_capacity(1_000_000);
    let mut x = 1u64;
    for _ in 0..1_000_000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x & 0xffff) as u32);
    }
    let lo = black_box(100u32);
    let hi = black_box(60_000u32);
    let t = Instant::now();
    let mut n = 0usize;
    for _ in 0..10 {
        n += v.iter().filter(|&&i| (lo..=hi).contains(&i)).count();
    }
    println!("{:.3} {}", t.elapsed().as_secs_f64() * 1e3, n);
}

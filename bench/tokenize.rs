use std::hint::black_box;
use std::time::Instant;
fn main() {
    let mut s = String::new();
    let mut x = 1u64;
    while s.len() < black_box(400_000) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push_str(&format!("w{:x} ", x & 0xfffff));
    }
    let t = Instant::now();
    let mut n = 0usize;
    for _ in 0..10 {
        n += s.split_whitespace().count();
    }
    println!("{:.3} {}", t.elapsed().as_secs_f64() * 1e3, n);
}

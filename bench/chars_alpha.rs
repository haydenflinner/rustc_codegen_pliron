use std::hint::black_box;
use std::time::Instant;
fn main() {
    let mut s = String::new();
    while s.len() < black_box(300_000) {
        s.push_str("the quick brown fox jumps over the lazy dog, and then some! ");
    }
    let t = Instant::now();
    let mut n = 0usize;
    for _ in 0..10 {
        n += s.chars().filter(|c| c.is_alphabetic()).count();
    }
    println!("{:.3} {}", t.elapsed().as_secs_f64() * 1e3, n);
}

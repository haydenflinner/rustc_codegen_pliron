use std::hint::black_box;
use std::time::Instant;
fn main() {
    let mut v: Vec<u64> = (0..black_box(200_000u64)).map(|i| i.wrapping_mul(6364136223846793005).rotate_left(17)).collect();
    let t = Instant::now();
    let mut acc = 0u64;
    for _ in 0..30 {
        v.reverse();
        v.sort_unstable();
        acc = acc.wrapping_add(v[v.len() / 2]);
    }
    println!("{:.3} {}", t.elapsed().as_secs_f64() * 1e3, acc);
}

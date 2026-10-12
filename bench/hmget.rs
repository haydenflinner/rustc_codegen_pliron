use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let mut m: HashMap<u64, u32> = HashMap::with_capacity(8192);
    let mut x = 1u64;
    for _ in 0..5000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        m.insert(x, 1);
    }
    let t = Instant::now();
    let mut total = 0u64;
    for round in 0..black_box(300u32) {
        for i in 0..5000u64 {
            let k = (i * 977).wrapping_add(round as u64 & 1);
            if let Some(v) = m.get(&k) {
                total += *v as u64;
            }
        }
    }
    let d = t.elapsed();
    println!("{:.3} {}", d.as_secs_f64() * 1e3, total);
}

use std::collections::HashMap;
use std::hint::black_box;

pub fn run() -> u64 {
    let mut total = 0u64;
    for round in 0..black_box(300u32) {
        let mut m: HashMap<u64, u32> = HashMap::with_capacity(1024);
        let mut x = round as u64 + 1;
        for i in 0..5000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *m.entry(x).or_insert(0) += 1;
            if m.get(&(i * 977)).is_some() {
                total += 1;
            }
        }
        total += m.len() as u64;
    }
    total
}

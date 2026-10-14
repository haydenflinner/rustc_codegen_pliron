use std::collections::HashMap;
use std::hint::black_box;

pub fn setup() -> HashMap<u64, u32> {
    let mut m: HashMap<u64, u32> = HashMap::with_capacity(8192);
    let mut x = 1u64;
    for _ in 0..5000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        m.insert(x, 1);
    }
    m
}

pub fn run(m: &HashMap<u64, u32>) -> u64 {
    let mut total = 0u64;
    for round in 0..black_box(300u32) {
        for i in 0..5000u64 {
            let k = (i * 977).wrapping_add(round as u64 & 1);
            if let Some(v) = m.get(&k) {
                total += *v as u64;
            }
        }
    }
    total
}

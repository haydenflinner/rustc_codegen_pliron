use std::hint::black_box;

pub fn setup() -> Vec<u64> {
    (0..black_box(200_000u64))
        .map(|i| i.wrapping_mul(6364136223846793005).rotate_left(17))
        .collect()
}

pub fn run(v: &mut Vec<u64>) -> u64 {
    let mut acc = 0u64;
    for _ in 0..30 {
        v.reverse();
        v.sort_unstable();
        acc = acc.wrapping_add(v[v.len() / 2]);
    }
    acc
}

pub fn setup() -> Vec<u32> {
    let mut v: Vec<u32> = Vec::with_capacity(1_000_000);
    let mut x = 1u64;
    for _ in 0..1_000_000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x & 0xffff) as u32);
    }
    v
}

pub fn run(v: &[u32], lo: u32, hi: u32) -> usize {
    let mut n = 0usize;
    for _ in 0..10 {
        n += v.iter().filter(|&&i| (lo..=hi).contains(&i)).count();
    }
    n
}

pub fn setup() -> String {
    let mut s = String::new();
    let mut x = 1u64;
    while s.len() < 400_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push_str(&format!("w{:x} ", x & 0xfffff));
    }
    s
}

pub fn run(s: &str) -> usize {
    let mut n = 0usize;
    for _ in 0..10 {
        n += s.split_whitespace().count();
    }
    n
}

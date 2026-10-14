pub fn setup() -> String {
    let mut s = String::new();
    while s.len() < 300_000 {
        s.push_str("the quick brown fox jumps over the lazy dog, and then some! ");
    }
    s
}

pub fn run(s: &str) -> usize {
    let mut n = 0usize;
    for _ in 0..10 {
        n += s.chars().filter(|c| c.is_ascii()).count();
    }
    n
}

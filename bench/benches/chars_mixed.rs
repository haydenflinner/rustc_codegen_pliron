pub fn setup() -> String {
    let mut s = String::new();
    while s.len() < 300_000 {
        s.push_str("naïve café Zürich — déjà vu, 北京 東京, Αθήνα! ");
    }
    s
}

pub fn run(s: &str) -> usize {
    let mut n = 0usize;
    for _ in 0..10 {
        n += s.chars().filter(|c| c.is_alphabetic()).count();
    }
    n
}

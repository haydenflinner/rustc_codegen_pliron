use std::hint::black_box;

pub fn run() -> u64 {
    let mut acc = 0u64;
    let mut buf = String::new();
    for i in 0..black_box(40_000u32) {
        buf.clear();
        buf.push_str(&format!("{} {:>8.3} {:#x} {}", i, i as f64 * 1.5, i, "tail"));
        acc = acc.wrapping_add(buf.len() as u64);
        for w in buf.split_ascii_whitespace() {
            acc = acc.wrapping_add(w.len() as u64);
            if let Ok(v) = w.parse::<u32>() {
                acc = acc.wrapping_add(v as u64);
            }
        }
    }
    acc
}

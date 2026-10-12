use std::hint::black_box;

fn main() {
    let words = ["1234", "567", "89", "12345678", "42"];
    let mut acc: u64 = 0;
    let t = std::time::Instant::now();
    for i in 0..100_000_000u64 {
        let w = words[(i % 5) as usize];
        if let Ok(v) = w.parse::<u32>() {
            acc = acc.wrapping_add(v as u64);
        }
        black_box(&mut acc);
    }
    let d = t.elapsed();
    println!("{:.3} {}", d.as_secs_f64() * 1e3, acc);
}

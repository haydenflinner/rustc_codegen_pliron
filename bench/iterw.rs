use std::hint::black_box;
use std::time::Instant;
fn main() {
    let a: Vec<i32> = (0..black_box(4_000_000i32)).collect();
    let t = Instant::now();
    let mut acc = 0i64;
    for _ in 0..20 {
        acc = acc.wrapping_add(
            a.iter().filter(|&&x| x % 3 != 0).map(|&x| x as i64 * x as i64).sum::<i64>(),
        );
    }
    println!("{:.3} {}", t.elapsed().as_secs_f64() * 1e3, acc);
}

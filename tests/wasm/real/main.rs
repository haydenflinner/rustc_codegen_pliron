//! Native driver for real.rs: prints `name best-ms checksum` per program.
mod real;
use std::time::Instant;
fn main() {
    let only = std::env::args().nth(1);
    for (name, f, reps) in real::PROGRAMS {
        if only.as_deref().is_some_and(|o| !name.contains(o)) {
            continue;
        }
        let mut sum = f(*reps);
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            sum = f(*reps);
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!("{name} {best:.2} {sum}");
    }
}

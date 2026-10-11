use std::hint::black_box;
use std::time::Instant;

#[inline(never)]
fn matmul(a: &[f64], b: &[f64], c: &mut [f64], n: usize) {
    for i in 0..n {
        for k in 0..n {
            let aik = a[i * n + k];
            for j in 0..n {
                c[i * n + j] += aik * b[k * n + j];
            }
        }
    }
}

fn main() {
    let n = 220usize;
    let mut a = vec![0.0f64; n * n];
    let mut b = vec![0.0f64; n * n];
    let mut c = vec![0.0f64; n * n];
    for i in 0..n * n {
        a[i] = (i as f64 * 0.0137).sin();
        b[i] = (i as f64 * 0.0231).cos();
    }
    let t = Instant::now();
    for _ in 0..black_box(4) {
        matmul(&a, &b, &mut c, n);
    }
    let d = t.elapsed();
    println!("{:.3} {:.6}", d.as_secs_f64() * 1e3, c[n * n / 2]);
}

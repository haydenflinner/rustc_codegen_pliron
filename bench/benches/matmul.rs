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

pub fn setup() -> (Vec<f64>, Vec<f64>, Vec<f64>, usize) {
    let n = 220usize;
    let mut a = vec![0.0f64; n * n];
    let mut b = vec![0.0f64; n * n];
    let c = vec![0.0f64; n * n];
    for i in 0..n * n {
        a[i] = (i as f64 * 0.0137).sin();
        b[i] = (i as f64 * 0.0231).cos();
    }
    (a, b, c, n)
}

pub fn run(a: &[f64], b: &[f64], c: &mut [f64], n: usize) -> f64 {
    for _ in 0..4 {
        matmul(a, b, c, n);
    }
    c[n * n / 2]
}

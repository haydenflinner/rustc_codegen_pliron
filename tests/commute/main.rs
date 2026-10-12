// A loop-invariant scalar multiplied against a one-use vector load should
// commute to `fmul load, splat` so x64's two-address `mulpd` clobbers the
// dead load register instead of copying the live splat (movdqa per iter).

#[inline(never)]
fn saxpy(c: &mut [f64], b: &[f64], a: f64) {
    for i in 0..c.len() {
        c[i] += a * b[i];
    }
}

#[inline(never)]
fn scale_add(dst: &mut [u64], src: &[u64], k: u64) {
    for i in 0..dst.len() {
        dst[i] = dst[i].wrapping_add(k.wrapping_mul(src[i]));
    }
}

fn main() {
    let n = 256usize;
    let b: Vec<f64> = (0..n).map(|i| i as f64 * 0.5).collect();
    let mut c = vec![1.0f64; n];
    saxpy(&mut c, &b, 3.5);
    assert!((c[7] - (1.0 + 3.5 * 3.5)).abs() < 1e-9);

    let s: Vec<u64> = (0..n).map(|i| i as u64).collect();
    let mut d = vec![0u64; n];
    scale_add(&mut d, &s, 7);
    assert_eq!(d[200], 1400);
    println!("commute ok {:.2} {}", c[7], d[200]);
}

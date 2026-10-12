// The matmul inner loop must vectorize even when inlined into a function
// whose other loops eat a plain `la.loops()` budget before it, and even
// though its bounds checks unwind through `try_call` panic paths: bcheck's
// dead-end scan must follow a try_call's normal-return edge (handlers can
// only unwind out) and accept the synthetic `trap` continuation, which is
// never marked cold. The versioned fast clone is what loopvec converts.

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

fn hash_mix(mut h: u64, data: &[u64]) -> u64 {
    for &x in data {
        h ^= x;
        h = h.wrapping_mul(0x100000001b3);
        h = h.rotate_left(13);
    }
    h
}

// A count-if reduction: the accumulator passes through unchanged on one
// latch and steps on the other, so it classifies Param::Other, and the
// gating `brif` is the pass latch's own terminator in an interior block —
// not the loop header's. This must vectorize as a masked byte count.
#[inline(never)]
fn cntif(data: &[u8], k: u8) -> usize {
    let mut n = 0;
    for &b in data {
        if b == k {
            n += 1;
        }
    }
    n
}

fn main() {
    let n = 16usize;
    let a: Vec<f64> = (0..n * n).map(|i| i as f64 * 0.5).collect();
    let b: Vec<f64> = (0..n * n).map(|i| i as f64 * 0.25).collect();
    let mut c = vec![0.0; n * n];
    let data: Vec<u64> = (0..1024).collect();

    matmul(&a, &b, &mut c, n);
    // c[i][j] = sum_k (i*n+k)/2 * (k*n+j)/4; spot-check a few entries.
    for i in [0usize, 3, 15] {
        for j in [0usize, 7, 15] {
            let want: f64 = (0..n)
                .map(|k| (i * n + k) as f64 * 0.5 * (k * n + j) as f64 * 0.25)
                .sum();
            assert_eq!(c[i * n + j], want);
        }
    }
    // The extra loops give main enough loop-nest entries that a
    // discovery-order budget reaches exhaustion before the j-loop.
    let h = hash_mix(0xcbf29ce484222325, &data);
    assert_eq!(h, 0x1c1c8b29ceb1e820);
    // Exercise the masked count over mixed data plus tail lengths so the
    // vector remainder path is covered too.
    let bytes: Vec<u8> = (0..1037u32).map(|i| (i * 37 + 11) as u8).collect();
    let want = bytes.iter().filter(|&&b| b == 7).count();
    assert_eq!(cntif(&bytes, 7), want);
    for len in [0usize, 1, 15, 16, 17, 33] {
        let w = bytes[..len].iter().filter(|&&b| b == 7).count();
        assert_eq!(cntif(&bytes[..len], 7), w);
    }
    println!("loopvec ok");
}

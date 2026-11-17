// PLIRON_RA2_REGION: values live through a whole function, used in one hot
// loop and live across a high-pressure loop with calls elsewhere (the
// shape of brotli's `BrotliHistogramCombine`: `best_idx1`/`best_idx2`
// across an inlined histogram add), plus values defined at entry and used
// at exit across a chain of calls (html-rewriter's `Stack::build_state`).
// The region split must not change what this prints, and the regalloc2
// checker (`PLIRON_RA_CHECK=1`) must accept every function.
use std::hint::black_box;

#[inline(never)]
fn sink(a: u64, b: u64) -> u64 {
    black_box(a.rotate_left(7) ^ b)
}

#[derive(Clone, Copy)]
struct Pair {
    a: usize,
    b: usize,
    cost: u64,
}

#[inline(never)]
fn combine(hist: &mut [[u32; 16]], pairs: &mut Vec<Pair>, rounds: usize) -> u64 {
    let mut acc = 0u64;
    for _ in 0..rounds {
        let mut best_idx1 = 0usize;
        let mut best_idx2 = 1usize;
        let mut best = u64::MAX;
        for p in pairs.iter() {
            if p.cost < best {
                best = p.cost;
                best_idx1 = p.a;
                best_idx2 = p.b;
            }
        }
        // High pressure: sixteen lanes live across the loop, with a call.
        let (lo, hi) = hist.split_at_mut(best_idx2.max(best_idx1));
        let dst = &mut lo[best_idx1.min(best_idx2)];
        let src = &hi[0];
        let mut s = [0u64; 8];
        for k in 0..16 {
            dst[k] = dst[k].wrapping_add(src[k]);
            s[k & 7] = s[k & 7].wrapping_add(dst[k] as u64 * (k as u64 + 1));
            if dst[k] & 0xff == 0x7f {
                s[0] = sink(s[0], s[k & 7]);
            }
        }
        for v in s {
            acc = acc.wrapping_mul(31).wrapping_add(v);
        }
        // Filter the pairs that refer to the merged histograms.
        let mut w = 0;
        for r in 0..pairs.len() {
            let p = pairs[r];
            if p.a == best_idx1 || p.b == best_idx1 || p.a == best_idx2 || p.b == best_idx2 {
                continue;
            }
            pairs[w] = p;
            w += 1;
        }
        pairs.truncate(w);
        if pairs.is_empty() {
            break;
        }
        acc ^= (best_idx1 as u64) << 32 | best_idx2 as u64;
    }
    acc
}

#[inline(never)]
fn build_state(xs: &[u64], seed: u64) -> u64 {
    let zero = black_box(0u64);
    let len = xs.len() as u64;
    let first = xs.first().copied().unwrap_or(7);
    let last = xs.last().copied().unwrap_or(9);
    let mut h = seed;
    for (i, &x) in xs.iter().enumerate() {
        h = sink(h, x ^ i as u64);
        if x % 3 == 0 {
            h = sink(h, len);
        }
    }
    h = sink(h, first);
    h = sink(h, last);
    h ^ zero ^ len.wrapping_mul(first) ^ last
}

fn main() {
    let mut hist = vec![[0u32; 16]; 64];
    for (i, h) in hist.iter_mut().enumerate() {
        for (k, v) in h.iter_mut().enumerate() {
            *v = ((i * 31 + k * 7) % 97) as u32;
        }
    }
    let mut pairs = Vec::new();
    for a in 0..64 {
        for b in (a + 1)..64 {
            let cost = ((a * 131 + b * 71) % 1009) as u64;
            pairs.push(Pair { a, b, cost });
        }
    }
    let c = combine(&mut hist, &mut pairs, black_box(40));
    println!("combine {c:x} left {}", pairs.len());
    let xs: Vec<u64> = (0..200u64).map(|i| i * i ^ 0x55).collect();
    println!("build_state {:x}", build_state(&xs, 3));
    println!("build_state {:x}", build_state(&[], 5));
}

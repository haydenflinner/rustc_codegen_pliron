use std::hint::black_box;

#[inline(never)]
fn sieve(n: usize) -> u64 {
    let mut is = vec![true; n];
    let mut c = 0u64;
    for i in 2..n {
        if is[i] {
            c += 1;
            let mut j = i * i;
            while j < n {
                is[j] = false;
                j += i;
            }
        }
    }
    c
}

#[inline(never)]
fn u128work(x: u128) -> u128 {
    let mut v = x;
    for i in 1..1000u128 {
        v = v.wrapping_mul(6364136223846793005).wrapping_add(i);
        v = (v / 7u128.max(1)) * 7 + (v % 97u128.max(1));
    }
    v
}

pub fn run() -> u64 {
    let mut acc = sieve(black_box(5_000_000));
    acc = acc.wrapping_add(u128work(black_box(0x9e3779b97f4a7c15)) as u64);
    acc
}

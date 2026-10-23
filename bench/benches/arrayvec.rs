//! Loops over a small fixed-capacity inline array with a runtime length:
//! an ArrayVec-like `[u32; 8]` + `len`. Each loop's trip count is a
//! runtime value bounded by the capacity: `as_slice` slices
//! `data[..len]`, and that bounds check is the dominating `len <= 8`
//! guard.
//! - `sum_all`: sum every element of many ArrayVecs in memory.
//! - `find_all`: linear search for a key in each ArrayVec.
//! - `push_iter`: build a local ArrayVec with 0..=8 pushes, then iterate
//!   over it. If both loops are fully unrolled, the array can live in
//!   registers.

use std::hint::black_box;

pub const CAP: usize = 8;

#[derive(Clone, Copy)]
pub struct ArrayVec {
    len: u32,
    data: [u32; CAP],
}

impl ArrayVec {
    #[inline]
    pub fn new() -> Self {
        ArrayVec {
            len: 0,
            data: [0; CAP],
        }
    }

    #[inline]
    pub fn push(&mut self, v: u32) {
        let len = self.len as usize;
        assert!(len < CAP, "ArrayVec full");
        self.data[len] = v;
        self.len += 1;
    }

    #[inline]
    pub fn as_slice(&self) -> &[u32] {
        &self.data[..self.len as usize]
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Number of ArrayVecs in the input.
const N: usize = 200_000;

pub fn setup() -> Vec<ArrayVec> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..N)
        .map(|_| {
            let mut v = ArrayVec::new();
            for _ in 0..rng.next() % (CAP as u64 + 1) {
                v.push((rng.next() % 64) as u32);
            }
            v
        })
        .collect()
}

#[inline(never)]
pub fn sum_all(vs: &[ArrayVec]) -> u64 {
    let mut sum = 0u64;
    for v in vs {
        for &x in v.as_slice() {
            sum = sum.wrapping_add(x as u64);
        }
    }
    sum
}

#[inline(never)]
pub fn find_all(vs: &[ArrayVec], key: u32) -> u64 {
    let mut acc = 0u64;
    for v in vs {
        if let Some(i) = v.as_slice().iter().position(|&x| x == key) {
            acc = acc.wrapping_add(i as u64 + 1);
        }
    }
    acc
}

#[inline(never)]
pub fn push_iter(seed: u64, iters: u32) -> u64 {
    let mut x = seed;
    let mut acc = 0u64;
    for _ in 0..iters {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let n = (x % (CAP as u64 + 1)) as u32;
        let mut v = ArrayVec::new();
        for j in 0..n {
            v.push((x >> (j * 4)) as u32 & 0xffff);
        }
        let mut h = 0u32;
        for &e in v.as_slice() {
            h = h.wrapping_mul(31).wrapping_add(e);
        }
        acc = acc.wrapping_add(h as u64);
    }
    acc
}

pub fn run(vs: &[ArrayVec]) -> u64 {
    let a = sum_all(black_box(vs));
    let b = find_all(black_box(vs), black_box(7));
    let c = push_iter(black_box(0x2545_f491_4f6c_dd1d), black_box(150_000));
    a ^ b.rotate_left(21) ^ c.rotate_left(42)
}

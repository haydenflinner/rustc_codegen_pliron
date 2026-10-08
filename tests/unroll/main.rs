// Constant-trip loops for `PLIRON_UNROLL`: unrolled, escaping/aliased, used after the loop,
// dynamic trip count (kept). expected.out is stock LLVM rustc -O output.
use std::hint::black_box;

#[inline(never)]
fn words(input: &[u8; 64]) -> u32 {
    let mut data = [0u32; 16];
    for (o, chunk) in data.iter_mut().zip(input.chunks_exact(4)) {
        *o = u32::from_le_bytes(chunk.try_into().unwrap());
    }
    data.iter().fold(0u32, |a, &w| a.rotate_left(5) ^ w)
}

#[inline(never)]
fn escaped(x: u32) -> u32 {
    let mut a = [0u32; 8];
    for i in 0..8 {
        a[i] = x.wrapping_mul(i as u32 + 1);
        black_box(&mut a);
    }
    a.iter().sum()
}

#[inline(never)]
fn aliased(x: u32) -> u32 {
    let mut a = [1u32; 6];
    let p = a.as_mut_ptr();
    for i in 0..6 {
        unsafe { *p.add(5 - i) = x + i as u32 };
        a[i] = a[i].wrapping_add(a[5 - i]);
    }
    a.iter().fold(0, |s, &v| s.wrapping_mul(31).wrapping_add(v))
}

#[inline(never)]
fn last(x: u64) -> (u64, u64) {
    let mut acc = x;
    let mut i = 0u64;
    while i < 10 {
        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(i);
        i += 3;
    }
    (acc, i)
}

#[inline(never)]
fn narrow() -> u8 {
    let mut s = 0u8;
    for i in 0u8..20 {
        s = s.wrapping_add(i.wrapping_mul(37));
    }
    s
}

#[inline(never)]
fn dynamic(n: usize, v: &[u32]) -> u32 {
    let mut s = 0;
    for i in 0..n.min(v.len()) {
        s ^= v[i] << (i % 7);
    }
    s
}

#[inline(never)]
fn panics(k: usize) -> u32 {
    let a = [3u32; 4];
    let mut s = 0;
    for i in 0..4 {
        s += a[(i + k) % 5 % 4];
    }
    s
}

#[inline(never)]
fn opts() -> u32 {
    let v: [Option<u16>; 5] = [Some(1), None, Some(7), Some(2), None];
    let mut s = 0u32;
    for o in v {
        if let Some(x) = o {
            s = s * 3 + x as u32;
        }
    }
    s
}

fn main() {
    let mut inp = [0u8; 64];
    for (i, b) in inp.iter_mut().enumerate() {
        *b = (i * 29 + 7) as u8;
    }
    let inp = black_box(inp);
    println!("words {}", words(&inp));
    println!("escaped {}", escaped(black_box(7)));
    println!("aliased {}", aliased(black_box(9)));
    println!("last {:?}", last(black_box(5)));
    println!("narrow {}", narrow());
    println!("dynamic {}", dynamic(black_box(11), &[1, 2, 3, 4, 5, 6, 7, 8]));
    println!("panics {}", panics(black_box(2)));
    println!("opts {}", opts());
}

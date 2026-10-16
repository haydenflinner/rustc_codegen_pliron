//! wasm32 kernel benchmarks: each `k_*` export runs the kernel `reps` times over
//! static data and returns a checksum (compared against stock LLVM by
//! `tests/wasm/bench.mjs`).
#![no_std]
#![allow(internal_features, static_mut_refs, clippy::all)]

use core::hint::black_box;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

const N: usize = 4096;
static mut A8: [u8; N * 4] = [0; N * 4];
static mut A32: [u32; N] = [0; N];
static mut B32: [u32; N] = [0; N];
static mut AF: [f32; N] = [0.0; N];
static mut BF: [f32; N] = [0.0; N];
static mut CF: [f32; N] = [0.0; N];
static mut AD: [f64; N] = [0.0; N];
static mut M1: [f32; 64 * 64] = [0.0; 64 * 64];
static mut M2: [f32; 64 * 64] = [0.0; 64 * 64];
static mut M3: [f32; 64 * 64] = [0.0; 64 * 64];
static mut TEXT: [u8; 8192] = [0; 8192];
static mut I16S: [i16; N] = [0; N];
static mut OUT8: [u8; N * 4] = [0; N * 4];

#[unsafe(no_mangle)]
pub extern "C" fn init(seed: u32) {
    let mut s = seed;
    let mut next = || {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        s
    };
    unsafe {
        for x in A8.iter_mut() {
            *x = (next() >> 24) as u8;
        }
        for x in A32.iter_mut() {
            *x = next();
        }
        for x in B32.iter_mut() {
            *x = next();
        }
        for x in AF.iter_mut() {
            *x = (next() >> 8) as f32 / 16777216.0 - 0.5;
        }
        for x in BF.iter_mut() {
            *x = (next() >> 8) as f32 / 16777216.0 - 0.5;
        }
        for x in AD.iter_mut() {
            *x = (next() >> 8) as f64 / 16777216.0 - 0.5;
        }
        for x in M1.iter_mut() {
            *x = (next() >> 8) as f32 / 16777216.0;
        }
        for x in M2.iter_mut() {
            *x = (next() >> 8) as f32 / 16777216.0;
        }
        for x in I16S.iter_mut() {
            *x = (next() >> 16) as i16;
        }
        for (i, x) in TEXT.iter_mut().enumerate() {
            let r = next();
            *x = match r % 7 {
                0 => b' ',
                1 => b'\n',
                _ => b'a' + (r >> 8) as u8 % 26 + (i as u8 & 1) * 0,
            };
        }
    }
}

macro_rules! k {
    ($name:ident, $reps:ident, $body:block) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name($reps: u32) -> u32 {
            let mut acc = 0u32;
            for r in 0..$reps {
                // Perturb the inputs so the kernel cannot be hoisted out of
                // the repetition loop.
                unsafe {
                    let i = (r as usize) & 15;
                    core::ptr::write_volatile(&raw mut A8[i], r as u8);
                    core::ptr::write_volatile(&raw mut A32[i], r);
                    core::ptr::write_volatile(&raw mut TEXT[i], b'a' + (r as u8 & 7));
                    core::ptr::write_volatile(&raw mut I16S[i], r as i16);
                    core::ptr::write_volatile(&raw mut AF[i], r as f32);
                    core::ptr::write_volatile(&raw mut AD[i], r as f64);
                    core::ptr::write_volatile(&raw mut M1[i], r as f32);
                }
                acc = acc.wrapping_add(black_box($body));
            }
            acc
        }
    };
}

k!(k_sum_u8, reps, {
    let a = unsafe { &A8[..] };
    a.iter().map(|&x| x as u32).sum::<u32>()
});

k!(k_sum_u32, reps, {
    let a = unsafe { &A32[..] };
    a.iter().fold(0u32, |s, &x| s.wrapping_add(x))
});

k!(k_dot_i32, reps, {
    let (a, b) = unsafe { (&A32[..], &B32[..]) };
    a.iter().zip(b.iter()).fold(0u32, |s, (&x, &y)| s.wrapping_add(x.wrapping_mul(y)))
});

k!(k_max_u8, reps, {
    let a = unsafe { &A8[..] };
    *a.iter().max().unwrap() as u32
});

k!(k_count_eq, reps, {
    let a = unsafe { &A8[..] };
    let t = black_box(77u8);
    a.iter().filter(|&&x| x == t).count() as u32
});

k!(k_axpy_f32, reps, {
    let (x, y) = unsafe { (&AF[..], &mut CF[..]) };
    let a = black_box(0.37f32);
    for i in 0..N {
        y[i] = a * x[i] + y[i];
    }
    y[17].to_bits()
});

k!(k_vadd_f32, reps, {
    let (x, y, z) = unsafe { (&AF[..], &BF[..], &mut CF[..]) };
    for ((z, &x), &y) in z.iter_mut().zip(x).zip(y) {
        *z = x + y;
    }
    z[3].to_bits()
});

k!(k_sum_f32, reps, {
    let x = unsafe { &AF[..] };
    let mut s = 0.0f32;
    for &v in x {
        s += v;
    }
    s.to_bits()
});

k!(k_dot_f64, reps, {
    let x = unsafe { &AD[..] };
    let mut s = 0.0f64;
    for i in 0..N {
        s += x[i] * x[(i + 1) % N];
    }
    s.to_bits() as u32
});

k!(k_matmul64, reps, {
    let (a, b, c) = unsafe { (&M1, &M2, &mut M3) };
    for i in 0..64 {
        for j in 0..64 {
            let mut s = 0.0f32;
            for k in 0..64 {
                s += a[i * 64 + k] * b[k * 64 + j];
            }
            c[i * 64 + j] = s;
        }
    }
    c[100].to_bits()
});

k!(k_sum_i16_widen, reps, {
    let a = unsafe { &I16S[..] };
    a.iter().map(|&x| x as i32).fold(0i32, |s, v| s.wrapping_add(v)) as u32
});

k!(k_memset_xor, reps, {
    let (a, o) = unsafe { (&A8[..], &mut OUT8[..]) };
    let k = black_box(0x5au8);
    for (o, &a) in o.iter_mut().zip(a) {
        *o = a ^ k;
    }
    o[5] as u32
});

k!(k_saturating_u8, reps, {
    let (a, o) = unsafe { (&A8[..], &mut OUT8[..]) };
    let k = black_box(40u8);
    for (o, &a) in o.iter_mut().zip(a) {
        *o = a.saturating_add(k);
    }
    o[9] as u32
});

k!(k_find_byte, reps, {
    let t = unsafe { &TEXT[..] };
    let needle = black_box(b'~');
    t.iter().position(|&b| b == needle).map_or(0, |p| p as u32 + 1)
});

k!(k_split_ws, reps, {
    let t = unsafe { &TEXT[..] };
    let mut n = 0u32;
    let mut inw = false;
    for &b in t {
        let ws = b == b' ' || b == b'\n';
        if !ws && !inw {
            n += 1;
        }
        inw = !ws;
    }
    n
});

k!(k_adler, reps, {
    let t = unsafe { &A8[..] };
    let (mut a, mut b) = (1u32, 0u32);
    for &x in t {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
});

k!(k_nbody_step, reps, {
    let x = unsafe { &mut AD[..64] };
    let mut e = 0.0f64;
    for i in 0..64 {
        for j in 0..64 {
            if i != j {
                let d = x[i] - x[j];
                e += 1.0 / (d * d + 0.01);
            }
        }
    }
    e.to_bits() as u32
});

k!(k_sort_small, reps, {
    let mut v = [0u32; 64];
    unsafe {
        v.copy_from_slice(&A32[..64]);
    }
    for i in 1..64 {
        let x = v[i];
        let mut j = i;
        while j > 0 && v[j - 1] > x {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
    v[10] ^ v[50]
});

k!(k_utf8_ascii, reps, {
    let t = unsafe { &TEXT[..] };
    t.iter().all(|&b| b < 128) as u32 + t.is_ascii() as u32
});

k!(k_histogram, reps, {
    let a = unsafe { &A8[..] };
    let mut h = [0u32; 256];
    for &x in a {
        h[x as usize] += 1;
    }
    h[3] ^ h[200]
});

k!(k_prefix_sum, reps, {
    let a = unsafe { &mut B32[..] };
    let mut s = 0u32;
    for x in a.iter_mut() {
        s = s.wrapping_add(*x);
        *x = s;
    }
    s
});

k!(k_i16_dot, reps, {
    let a = unsafe { &I16S[..] };
    let mut s = 0i32;
    for i in 0..N - 1 {
        s += a[i] as i32 * a[i + 1] as i32;
    }
    s as u32
});

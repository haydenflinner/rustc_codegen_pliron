//! "Real program" workloads for backend comparisons: ordinary std-using Rust
//! with no unsafe tuning. Built three ways: wasm32-unknown-unknown (cdylib,
//! driven from node), and natively (driven by `main.rs`). Each `run_*` takes a
//! repetition count and returns a checksum.
#![allow(clippy::all, dead_code)]

use std::collections::HashMap;
use std::fmt::Write as _;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

// ---- SHA-256 ----------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

pub fn run_sha256(reps: u32) -> u32 {
    let mut s = 0x1234_5678_9abc_def0u64;
    let data: Vec<u8> = (0..64 * 1024).map(|_| xorshift(&mut s) as u8).collect();
    let mut acc = 0u32;
    for r in 0..reps {
        let mut d = data.clone();
        d[0] = r as u8;
        let h = sha256(&d);
        acc = acc.wrapping_add(u32::from_le_bytes([h[0], h[1], h[2], h[3]]));
    }
    acc
}

// ---- base64 + crc32 ----------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63]);
        out.push(B64[(n >> 12) as usize & 63]);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] } else { b'=' });
    }
    out
}

fn b64_decode(data: &[u8]) -> Vec<u8> {
    let mut lut = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        lut[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    for c in data.chunks_exact(4) {
        let v: Vec<u32> = c.iter().map(|&x| lut[x as usize] as u32 & 63).collect();
        let n = v[0] << 18 | v[1] << 12 | v[2] << 6 | v[3];
        out.push((n >> 16) as u8);
        if c[2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if c[3] != b'=' {
            out.push(n as u8);
        }
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
        }
        table[i as usize] = c;
    }
    let mut c = !0u32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

pub fn run_base64_crc(reps: u32) -> u32 {
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let data: Vec<u8> = (0..96 * 1024).map(|_| xorshift(&mut s) as u8).collect();
    let mut acc = 0u32;
    for r in 0..reps {
        let mut d = data.clone();
        d[1] = r as u8;
        let e = b64_encode(&d);
        let back = b64_decode(&e);
        acc = acc.wrapping_add(crc32(&e)).wrapping_add(crc32(&back));
    }
    acc
}

// ---- mandelbrot + n-body (floating point) -------------------------------

pub fn run_mandelbrot(reps: u32) -> u32 {
    let (w, h, max) = (160usize, 120usize, 150u32);
    let mut acc = 0u32;
    for r in 0..reps {
        let off = r as f64 * 1e-4;
        for y in 0..h {
            for x in 0..w {
                let cr = (x as f64 / w as f64) * 3.5 - 2.5 + off;
                let ci = (y as f64 / h as f64) * 2.0 - 1.0;
                let (mut zr, mut zi, mut i) = (0.0f64, 0.0f64, 0u32);
                while i < max && zr * zr + zi * zi <= 4.0 {
                    let t = zr * zr - zi * zi + cr;
                    zi = 2.0 * zr * zi + ci;
                    zr = t;
                    i += 1;
                }
                acc = acc.wrapping_add(i);
            }
        }
    }
    acc
}

#[derive(Clone, Copy)]
struct Body {
    p: [f64; 3],
    v: [f64; 3],
    m: f64,
}

pub fn run_nbody(reps: u32) -> u32 {
    let mut s = 42u64;
    let mut rnd = || (xorshift(&mut s) >> 11) as f64 / (1u64 << 53) as f64;
    let mut bodies: Vec<Body> = (0..24)
        .map(|_| Body {
            p: [rnd() * 10.0, rnd() * 10.0, rnd() * 10.0],
            v: [rnd() - 0.5, rnd() - 0.5, rnd() - 0.5],
            m: rnd() + 0.5,
        })
        .collect();
    for _ in 0..reps * 20 {
        let n = bodies.len();
        for i in 0..n {
            let mut a = [0.0f64; 3];
            for j in 0..n {
                if i == j {
                    continue;
                }
                let d = [
                    bodies[j].p[0] - bodies[i].p[0],
                    bodies[j].p[1] - bodies[i].p[1],
                    bodies[j].p[2] - bodies[i].p[2],
                ];
                let r2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2] + 0.01;
                let inv = bodies[j].m / (r2 * r2.sqrt());
                for k in 0..3 {
                    a[k] += d[k] * inv;
                }
            }
            for k in 0..3 {
                bodies[i].v[k] += a[k] * 0.01;
            }
        }
        for b in bodies.iter_mut() {
            for k in 0..3 {
                b.p[k] += b.v[k] * 0.01;
            }
        }
    }
    bodies.iter().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add((b.p[0] * 1000.0) as i64 as u32))
}

// ---- word frequency (HashMap, String, sort, fmt) -----------------------

pub fn run_wordfreq(reps: u32) -> u32 {
    let words: Vec<String> = (0..2000)
        .map(|i| {
            let mut s = String::new();
            let mut x = (i as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15);
            for _ in 0..(3 + i % 7) {
                s.push((b'a' + (x % 26) as u8) as char);
                x /= 26;
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            }
            s
        })
        .collect();
    let mut st = 7u64;
    let mut acc = 0u32;
    for _ in 0..reps {
        let mut text = String::new();
        for _ in 0..20_000 {
            // skewed word choice
            let r = xorshift(&mut st);
            let idx = ((r % 2000) * (r >> 40 & 0xff) / 255) as usize;
            text.push_str(&words[idx % words.len()]);
            text.push(' ');
        }
        let mut counts: HashMap<&str, u32> = HashMap::new();
        for w in text.split_whitespace() {
            *counts.entry(w).or_insert(0) += 1;
        }
        let mut v: Vec<(&str, u32)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut out = String::new();
        for (w, c) in v.iter().take(50) {
            let _ = write!(out, "{w}={c};");
        }
        acc = acc.wrapping_add(out.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)));
    }
    acc
}

// ---- records -> text -> records (fmt + parse) ----------------------------

#[derive(Clone, Debug, PartialEq)]
struct Rec {
    id: u32,
    score: f64,
    name: String,
    tags: Vec<u16>,
}

pub fn run_records(reps: u32) -> u32 {
    let mut st = 99u64;
    let mut acc = 0u32;
    for _ in 0..reps {
        let recs: Vec<Rec> = (0..400)
            .map(|i| Rec {
                id: i,
                score: (xorshift(&mut st) % 100000) as f64 / 100.0,
                name: format!("item-{}", xorshift(&mut st) % 10000),
                tags: (0..(i % 5)).map(|t| (t * 7 + i) as u16).collect(),
            })
            .collect();
        let mut text = String::new();
        for r in &recs {
            let tags: Vec<String> = r.tags.iter().map(|t| t.to_string()).collect();
            let _ = writeln!(text, "{}|{:.2}|{}|{}", r.id, r.score, r.name, tags.join(","));
        }
        let mut back = Vec::new();
        for line in text.lines() {
            let mut it = line.split('|');
            let id: u32 = it.next().unwrap().parse().unwrap();
            let score: f64 = it.next().unwrap().parse().unwrap();
            let name = it.next().unwrap().to_string();
            let tags: Vec<u16> = it
                .next()
                .unwrap()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().unwrap())
                .collect();
            back.push(Rec { id, score, name, tags });
        }
        acc = acc
            .wrapping_add(back.len() as u32)
            .wrapping_add(back.iter().map(|r| r.id.wrapping_add(r.tags.len() as u32)).sum::<u32>())
            .wrapping_add(text.len() as u32);
    }
    acc
}

// ---- matrix multiply (f32, cache-blocked) ----------------------------------

pub fn run_matmul(reps: u32) -> u32 {
    const N: usize = 96;
    let mut s = 5u64;
    let a: Vec<f32> = (0..N * N).map(|_| (xorshift(&mut s) % 1000) as f32 / 1000.0).collect();
    let b: Vec<f32> = (0..N * N).map(|_| (xorshift(&mut s) % 1000) as f32 / 1000.0).collect();
    let mut c = vec![0f32; N * N];
    let mut acc = 0u32;
    for _ in 0..reps {
        c.iter_mut().for_each(|x| *x = 0.0);
        for i in 0..N {
            for k in 0..N {
                let aik = a[i * N + k];
                let row = &b[k * N..(k + 1) * N];
                let out = &mut c[i * N..(i + 1) * N];
                for j in 0..N {
                    out[j] += aik * row[j];
                }
            }
        }
        acc = acc.wrapping_add(c[N * 3 + 5].to_bits());
    }
    acc
}

// ---- sorting + binary search -------------------------------------------------

pub fn run_sort(reps: u32) -> u32 {
    let mut st = 1u64;
    let mut acc = 0u32;
    for _ in 0..reps {
        let mut v: Vec<u32> = (0..50_000).map(|_| xorshift(&mut st) as u32).collect();
        v.sort_unstable();
        let mut hits = 0u32;
        for q in 0..20_000u32 {
            if v.binary_search(&q.wrapping_mul(2_654_435_761)).is_ok() {
                hits += 1;
            }
        }
        let mut w: Vec<(u32, u32)> = v.iter().map(|&x| (x % 1000, x)).collect();
        w.sort();
        acc = acc.wrapping_add(hits).wrapping_add(v[100]).wrapping_add(w[500].1);
    }
    acc
}

pub fn run_sort_unstable(reps: u32) -> u32 {
    let mut st = 1u64;
    let mut acc = 0u32;
    for _ in 0..reps {
        let mut v: Vec<u32> = (0..50_000).map(|_| xorshift(&mut st) as u32).collect();
        v.sort_unstable();
        acc = acc.wrapping_add(v[100]);
    }
    acc
}

pub fn run_sort_stable(reps: u32) -> u32 {
    let mut st = 1u64;
    let mut acc = 0u32;
    for _ in 0..reps {
        let mut v: Vec<u32> = (0..50_000).map(|_| xorshift(&mut st) as u32).collect();
        v.sort();
        acc = acc.wrapping_add(v[100]);
    }
    acc
}

pub fn run_bsearch(reps: u32) -> u32 {
    let mut st = 1u64;
    let mut v: Vec<u32> = (0..50_000).map(|_| xorshift(&mut st) as u32).collect();
    v.sort_unstable();
    let mut hits = 0u32;
    for _ in 0..reps {
        for q in 0..20_000u32 {
            if v.binary_search(&q.wrapping_mul(2_654_435_761)).is_ok() {
                hits += 1;
            }
        }
    }
    hits
}

pub const PROGRAMS: &[(&str, fn(u32) -> u32, u32)] = &[
    ("sha256", run_sha256, 12),
    ("base64_crc", run_base64_crc, 12),
    ("mandelbrot", run_mandelbrot, 6),
    ("nbody", run_nbody, 8),
    ("wordfreq", run_wordfreq, 6),
    ("records", run_records, 30),
    ("matmul", run_matmul, 40),
    ("sort", run_sort, 6),
    ("sort_unstable", run_sort_unstable, 6),
    ("sort_stable", run_sort_stable, 6),
    ("bsearch", run_bsearch, 6),
];

#[cfg(target_arch = "wasm32")]
mod exports {
    #[unsafe(no_mangle)]
    pub extern "C" fn count() -> u32 {
        super::PROGRAMS.len() as u32
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn run(i: u32, reps: u32) -> u32 {
        (super::PROGRAMS[i as usize].1)(reps)
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn default_reps(i: u32) -> u32 {
        super::PROGRAMS[i as usize].2
    }
}

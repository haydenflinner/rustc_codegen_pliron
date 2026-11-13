// PLIRON_FASTCC (aarch64): a direct call to a function compiled in the
// same object clobbers only what that function's code writes. Every
// caller here keeps more values live across its call than there are
// callee-saved registers, so a callee register missing from a recorded
// set shows up as a wrong sum. The callee shapes are the ones the set
// has to get right: a leaf, a chain (transitive sets), recursion and
// mutual recursion (no set), calls through pointers and `dyn` (ABI set),
// calls into std, float and vector registers, multi-register and stack
// returns, a path that ends in a panic (not counted) next to one that
// returns after heavy work (counted), and a panic caught above a
// narrowed call. Output must match stock rustc's.
use std::hint::black_box as bb;
use std::panic::{catch_unwind, AssertUnwindSafe};

#[inline(never)]
fn leaf(a: u64, b: u64) -> u64 {
    a.rotate_left(7) ^ b.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

// Writes many integer temporaries on the path that returns.
#[inline(never)]
fn busy(a: u64, b: u64, c: u64, d: u64) -> u64 {
    let (mut x0, mut x1, mut x2, mut x3) = (a, b, c, d);
    let (mut x4, mut x5, mut x6, mut x7) = (a ^ b, b ^ c, c ^ d, d ^ a);
    for i in 0..bb(4u64) {
        x0 = x0.wrapping_add(x4).rotate_left(13);
        x1 = x1.wrapping_add(x5).rotate_left(17);
        x2 = x2.wrapping_add(x6).rotate_left(19);
        x3 = x3.wrapping_add(x7).rotate_left(23);
        x4 ^= x0 ^ i;
        x5 ^= x1.wrapping_mul(3);
        x6 ^= x2.wrapping_mul(5);
        x7 ^= x3.wrapping_mul(7);
    }
    x0 ^ x1 ^ x2 ^ x3 ^ x4 ^ x5 ^ x6 ^ x7
}

// A chain: `mid`'s set must contain `busy`'s.
#[inline(never)]
fn mid(a: u64, b: u64) -> u64 {
    let t = leaf(a, b);
    busy(t, a, b, t ^ 1).wrapping_add(leaf(b, t))
}

#[inline(never)]
fn fact(n: u64, acc: u64) -> u64 {
    if n <= 1 { acc } else { fact(n - 1, acc.wrapping_mul(n) ^ leaf(n, acc)) }
}

#[inline(never)]
fn even(n: u64, a: u64) -> u64 {
    if n == 0 { a } else { odd(n - 1, a.rotate_left(3) ^ n) }
}

#[inline(never)]
fn odd(n: u64, a: u64) -> u64 {
    if n == 0 { !a } else { even(n - 1, a.wrapping_mul(31).wrapping_add(n)) }
}

// The cold path does heavy work and never returns; the hot path is tiny.
#[inline(never)]
fn checked(v: &[u64], i: usize, k: u64) -> u64 {
    if i >= v.len() {
        let mut s = String::new();
        for j in 0..k % 5 {
            s.push_str(&format!("{j},"));
        }
        panic!("index {i} of {} ({s})", v.len());
    }
    v[i] ^ k
}

// Returns after calling into std (allocation, formatting): whole ABI set.
#[inline(never)]
fn alloc_len(n: u64) -> u64 {
    let v: Vec<u64> = (0..n % 7 + 1).collect();
    format!("{v:?}").len() as u64
}

#[inline(never)]
fn fleaf(a: f64, b: f64) -> f64 {
    (a * 1.5 + b).sqrt() - b / 3.0
}

#[inline(never)]
fn fbusy(a: f64, b: f64) -> f64 {
    let mut x = [a, b, a + b, a - b, a * b, a / (b + 1.0), b - a, a + 2.0 * b];
    for i in 0..bb(3usize) {
        for j in 0..8 {
            x[j] = x[j] * 1.000_1 + x[(j + i + 1) % 8] * 0.5;
        }
    }
    x.iter().sum()
}

#[inline(never)]
fn wide(a: u64, b: u64) -> u128 {
    (a as u128) << 64 | (b as u128).wrapping_mul(0x1_0000_0001)
}

// More return values than return registers.
#[inline(never)]
fn many(a: u64) -> (u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) {
    (a, a + 1, a ^ 2, a * 3, a - 4, a << 5, a >> 6, a | 7, a & 8, !a)
}

#[inline(never)]
fn big(a: u64) -> [u64; 12] {
    let mut r = [0u64; 12];
    for (i, x) in r.iter_mut().enumerate() {
        *x = a.rotate_left(i as u32) ^ i as u64;
    }
    r
}

trait Op {
    fn ap(&self, a: u64, b: u64) -> u64;
}
struct Add;
struct Mix(u64);
impl Op for Add {
    #[inline(never)]
    fn ap(&self, a: u64, b: u64) -> u64 {
        a.wrapping_add(b)
    }
}
impl Op for Mix {
    #[inline(never)]
    fn ap(&self, a: u64, b: u64) -> u64 {
        busy(a, b, self.0, a ^ b)
    }
}

macro_rules! live16 {
    ($s:expr) => {{
        let s: u64 = $s;
        [
            bb(s), bb(s ^ 1), bb(s.wrapping_mul(3)), bb(s.wrapping_add(5)),
            bb(s.rotate_left(7)), bb(s ^ 11), bb(s.wrapping_mul(13)), bb(s.wrapping_add(17)),
            bb(s.rotate_left(19)), bb(s ^ 23), bb(s.wrapping_mul(29)), bb(s.wrapping_add(31)),
            bb(s.rotate_left(37)), bb(s ^ 41), bb(s.wrapping_mul(43)), bb(s.wrapping_add(47)),
        ]
    }};
}

macro_rules! fold16 {
    ($v:expr, $r:expr) => {{
        let v = $v;
        let mut h: u64 = $r;
        let mut i = 0;
        while i < 16 {
            h = h.rotate_left(5) ^ v[i];
            i += 1;
        }
        h
    }};
}

// Sixteen integers live across each kind of call. Locals, not an array in
// memory: `bb` on each keeps them apart and in registers where it can.
#[inline(never)]
fn across_leaf(s: u64) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = leaf(a, b);
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_busy(s: u64) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = busy(a, b, c, d);
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_mid(s: u64) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = mid(a, b);
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_rec(s: u64) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = fact(a % 9 + 2, b) ^ even(c % 11, d);
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_checked(s: u64, v: &[u64]) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = checked(v, (a % 4) as usize, b);
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_std(s: u64) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = alloc_len(a);
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_ptr(s: u64, f1: fn(u64, u64) -> u64, op: &dyn Op) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = f1(a, b) ^ op.ap(c, d);
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_rets(s: u64) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let w = wide(a, b);
    let t = many(c);
    let q = big(d);
    let r = (w as u64) ^ ((w >> 64) as u64) ^ t.0 ^ t.3 ^ t.7 ^ t.9 ^ q[0] ^ q[5] ^ q[11];
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

#[inline(never)]
fn across_float(s: u64) -> f64 {
    let x = s as f64;
    let v = [
        bb(x + 0.5), bb(x * 1.25), bb(x - 3.0), bb(x / 7.0), bb(x + 11.0), bb(x * 0.75),
        bb(x - 13.5), bb(x / 17.0), bb(x + 19.0), bb(x * 2.5), bb(x - 23.0), bb(x / 29.0),
        bb(x + 31.0), bb(x * 3.5), bb(x - 37.0), bb(x / 41.0), bb(x + 43.0), bb(x * 4.5),
    ];
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r] = v;
    let y = fleaf(a, b) + fbusy(c, d) + leaf(s, 3) as f64;
    y + a + b + c + d + e + f + g + h + i + j + k + l + m + n + o + p + q + r
}

// The panic unwinds through `across_checked`'s narrowed call and is
// caught here; the values live across `catch_unwind` must survive.
#[inline(never)]
fn caught(s: u64, v: &[u64]) -> u64 {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = live16!(s);
    let r = match catch_unwind(AssertUnwindSafe(|| across_checked(a, &v[..(b % 3) as usize]))) {
        Ok(x) => x,
        Err(_) => 0xDEAD,
    };
    fold16!([a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p], r)
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let v = [3u64, 5, 7, 11];
    let mut h = 0u64;
    let mut fsum = 0f64;
    for s in 1..200u64 {
        let s = bb(s.wrapping_mul(0x2545_F491_4F6C_DD1D));
        h = h.rotate_left(1) ^ across_leaf(s);
        h = h.rotate_left(1) ^ across_busy(s);
        h = h.rotate_left(1) ^ across_mid(s);
        h = h.rotate_left(1) ^ across_rec(s);
        h = h.rotate_left(1) ^ across_checked(s, &v);
        h = h.rotate_left(1) ^ across_std(s);
        h = h.rotate_left(1) ^ across_ptr(s, leaf, &Add);
        h = h.rotate_left(1) ^ across_ptr(s, busy2, &Mix(s));
        h = h.rotate_left(1) ^ across_rets(s);
        h = h.rotate_left(1) ^ caught(s, &v);
        fsum += across_float(s % 1000);
    }
    println!("{h:016x} {fsum:.6}");
}

#[inline(never)]
fn busy2(a: u64, b: u64) -> u64 {
    busy(a, b, a ^ b, 9)
}

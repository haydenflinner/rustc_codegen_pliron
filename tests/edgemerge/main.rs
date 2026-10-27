// PLIRON_EDGEMERGE (identical regalloc edge blocks share one copy) and
// PLIRON_EPIMERGE (one epilogue per function). Shapes: a dense `match`
// whose arms reach one merge through edge blocks (jump-table entries
// aliased to a shared block), early-return chains with many live values
// (one shared epilogue), and drop/unwind fan-in under catch_unwind.
use std::hint::black_box;

#[derive(Clone, Copy)]
enum Op {
    A(u32),
    B(u32, u32),
    C,
    D(u64),
    E,
    F(u8),
    G,
    H(u16),
}

#[inline(never)]
fn dispatch(ops: &[Op], k: u64) -> u64 {
    let (mut a, mut b, mut c, mut d) = (k, k ^ 7, k.wrapping_mul(3), k + 11);
    for op in ops {
        let x = match *op {
            Op::A(v) => a.wrapping_add(v as u64),
            Op::B(v, w) => b.wrapping_mul(v as u64 | 1) ^ w as u64,
            Op::C | Op::E | Op::G => c,
            Op::D(v) => d ^ v,
            Op::F(v) => a.rotate_left(v as u32 & 63),
            Op::H(v) => b.wrapping_sub(v as u64),
        };
        let y = black_box(x);
        a = a.wrapping_add(y);
        b ^= a.rotate_right(7);
        c = c.wrapping_add(b) ^ d;
        d = d.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(c);
    }
    a ^ b ^ c ^ d
}

#[inline(never)]
fn early(v: &[u64], s: u64) -> u64 {
    let (p, q, r) = (black_box(s), black_box(s * 3), black_box(s ^ 0x55));
    if v.is_empty() {
        return p;
    }
    let t = black_box(v[0].wrapping_add(q));
    if t % 3 == 0 {
        return t ^ r;
    }
    let u = black_box(t.wrapping_mul(r));
    if u % 5 == 1 {
        return u.wrapping_add(p);
    }
    if v.len() > 2 && v[2] > u {
        return v[2] - u + q;
    }
    let w = black_box(u ^ p ^ q ^ r);
    if w & 1 == 0 {
        return w >> 1;
    }
    w.wrapping_add(v.iter().sum::<u64>())
}

struct Noisy(u64, &'static std::sync::atomic::AtomicU64);
impl Drop for Noisy {
    fn drop(&mut self) {
        self.1.fetch_add(self.0, std::sync::atomic::Ordering::Relaxed);
    }
}

static DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[inline(never)]
fn cleanup(n: u64) -> u64 {
    let a = Noisy(n, &DROPS);
    let b = Noisy(n * 2, &DROPS);
    let v: Vec<u64> = (0..n).collect();
    let mut acc = 0u64;
    for (i, x) in v.iter().enumerate() {
        let c = Noisy(*x, &DROPS);
        if black_box(*x) == 13 {
            panic!("boom {}", c.0 + a.0);
        }
        acc = acc.wrapping_add(black_box(x * i as u64));
        if acc > 1 << 40 {
            return acc + b.0;
        }
    }
    acc + a.0 + b.0
}

fn main() {
    let ops: Vec<Op> = (0..400u32)
        .map(|i| match i % 8 {
            0 => Op::A(i),
            1 => Op::B(i, i * 7),
            2 => Op::C,
            3 => Op::D(i as u64 * 31),
            4 => Op::E,
            5 => Op::F(i as u8),
            6 => Op::G,
            _ => Op::H(i as u16),
        })
        .collect();
    let mut h = 0u64;
    for k in 0..50 {
        h = h.wrapping_mul(31).wrapping_add(dispatch(&ops[k..], k as u64));
    }
    println!("dispatch {h:x}");
    let mut e = 0u64;
    for s in 0..200u64 {
        let v: Vec<u64> = (0..(s % 6)).map(|i| i * s + 1).collect();
        e = e.wrapping_mul(17).wrapping_add(early(&v, s));
    }
    println!("early {e:x}");
    std::panic::set_hook(Box::new(|_| {}));
    let mut c = 0u64;
    for n in [3u64, 10, 20, 14, 5] {
        match std::panic::catch_unwind(|| cleanup(n)) {
            Ok(v) => c = c.wrapping_add(v),
            Err(_) => c = c.wrapping_add(1000),
        }
    }
    println!("cleanup {c} drops {}", DROPS.load(std::sync::atomic::Ordering::Relaxed));
}

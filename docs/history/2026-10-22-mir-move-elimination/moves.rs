// Move-heavy kernels for comparing -Zmir-move-elimination on/off (stock rustc).
// Addresses are observed (black_box(&x)) in several places: that is the case
// LLVM can't optimize on its own and that MoveElimination targets.
use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Copy)]
pub struct Big {
    pub a: [u64; 16],
    pub tag: u32,
}

#[derive(Clone)]
pub struct Outer {
    pub inner: Big,
    pub extra: [u64; 8],
    pub name_len: usize,
}

// 1. Return-by-value chain with observed addresses.
#[inline(never)]
pub fn make_big(seed: u64) -> Big {
    let mut b = Big { a: [seed; 16], tag: seed as u32 };
    black_box(&mut b); // address observed
    b
}

#[inline(never)]
pub fn wrap(seed: u64) -> Outer {
    let inner = make_big(seed);
    black_box(&inner); // address observed: LLVM must keep inner distinct
    let extra = [seed ^ 1; 8];
    Outer { inner, extra, name_len: 3 }
}

#[inline(never)]
pub fn wrap2(seed: u64) -> Option<Outer> {
    let o = wrap(seed);
    black_box(&o);
    Some(o)
}

// 2. Builder chain.
#[derive(Clone)]
pub struct Builder {
    pub cfg: [u64; 24],
    pub flags: u32,
}

impl Builder {
    #[inline(never)]
    pub fn new() -> Self {
        Builder { cfg: [0; 24], flags: 0 }
    }
    #[inline(never)]
    pub fn set(mut self, i: usize, v: u64) -> Self {
        self.cfg[i % 24] = v;
        black_box(&self);
        self
    }
    #[inline(never)]
    pub fn flag(mut self, f: u32) -> Self {
        self.flags |= f;
        self
    }
}

#[inline(never)]
pub fn build_chain(x: u64) -> Builder {
    Builder::new().set(1, x).set(2, x + 1).flag(4).set(3, x + 2).flag(8)
}

// 3. Vec<Big> pushes.
#[inline(never)]
pub fn push_many(v: &mut Vec<Big>, n: u64) {
    for i in 0..n {
        let b = make_big(i);
        black_box(&b);
        v.push(b);
    }
}

// 4. Result of large payloads through `?`.
#[derive(Debug)]
pub struct PErr(u32);

#[inline(never)]
pub fn parse(x: u64) -> Result<Big, PErr> {
    if x == u64::MAX {
        return Err(PErr(1));
    }
    let b = make_big(x);
    black_box(&b);
    Ok(b)
}

#[inline(never)]
pub fn parse2(x: u64) -> Result<Outer, PErr> {
    let b = parse(x)?;
    let c = parse(x + 1)?;
    black_box(&c);
    Ok(Outer { inner: b, extra: [c.a[0]; 8], name_len: 7 })
}

// 5. Aggregate of aggregates, all address-taken.
#[inline(never)]
pub fn nest(x: u64) -> (Big, Big, [u64; 4]) {
    let p = make_big(x);
    let q = make_big(x + 1);
    black_box((&p, &q));
    let r = [x; 4];
    (p, q, r)
}

fn time<F: FnMut()>(name: &str, iters: u64, mut f: F) {
    // warmup
    for _ in 0..iters / 10 {
        f();
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        let ns = t.elapsed().as_nanos() as f64 / iters as f64;
        best = best.min(ns);
    }
    println!("{name:12} {best:8.2} ns/iter");
}

fn main() {
    let n: u64 = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(2_000_000);
    time("wrap2", n, || {
        black_box(wrap2(black_box(7)));
    });
    time("build_chain", n, || {
        black_box(build_chain(black_box(7)));
    });
    let mut v = Vec::with_capacity(1024);
    time("push_many", n / 64, || {
        v.clear();
        push_many(&mut v, black_box(64));
        black_box(&v);
    });
    time("parse2", n, || {
        black_box(parse2(black_box(7)).ok());
    });
    time("nest", n, || {
        black_box(nest(black_box(7)));
    });
}

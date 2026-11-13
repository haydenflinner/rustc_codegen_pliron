// Cache benchmark for profile-guided field order (docs/pgo.md "Field
// order"): big structs whose few hot fields rustc's default order leaves
// on different cache lines. Three shapes: a sequential sweep over a
// `Vec`, a gather in shuffled order (independent misses) and an
// index-linked walk in shuffled order (dependent misses).
// tests/field_order/cache.sh builds it plain, reordered and with stock
// rustc. Usage: cache [elements, default 1000000] [repetitions, default 9]
use std::hint::black_box;
use std::mem::{offset_of, size_of};
use std::time::Instant;

/// 160 bytes, one alignment class. Hot: `x`, `vx`, `mass`, `alive`
/// (declaration offsets 0, 64, 128, 152: three 64-byte lines, two
/// 128-byte lines; hot-first they share one).
struct Particle {
    x: f64,
    c1: u64,
    c2: u64,
    c3: u64,
    c4: u64,
    c5: u64,
    c6: u64,
    c7: u64,
    vx: f64,
    c8: u64,
    c9: u64,
    c10: u64,
    c11: u64,
    c12: u64,
    c13: u64,
    c14: u64,
    mass: f64,
    c15: u64,
    c16: u64,
    alive: u64,
}

/// 192 bytes. Hot: `key`, `val`, `next`, `hits`.
struct Node {
    key: u64,
    a: [u64; 7],
    val: u64,
    b: [u64; 7],
    next: usize,
    c: [u64; 6],
    hits: u64,
}

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

#[inline(never)]
fn make_particles(n: usize) -> Vec<Particle> {
    let mut s = 0x9e3779b97f4a7c15u64;
    (0..n)
        .map(|i| {
            let r = rng(&mut s);
            Particle {
                x: i as f64,
                c1: r,
                c2: r ^ 2,
                c3: r ^ 3,
                c4: r ^ 4,
                c5: r ^ 5,
                c6: r ^ 6,
                c7: r ^ 7,
                vx: (r % 1000) as f64 / 1000.0,
                c8: r ^ 8,
                c9: r ^ 9,
                c10: r ^ 10,
                c11: r ^ 11,
                c12: r ^ 12,
                c13: r ^ 13,
                c14: r ^ 14,
                mass: 1.0 + (r % 7) as f64,
                c15: r ^ 15,
                c16: r ^ 16,
                alive: r & 1,
            }
        })
        .collect()
}

#[inline(never)]
fn make_nodes(n: usize) -> (Vec<Node>, usize) {
    let mut s = 0x2545f4914f6cdd1du64;
    // A random single cycle through all nodes (Sattolo).
    let mut perm: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = (rng(&mut s) % i as u64) as usize;
        perm.swap(i, j);
    }
    let nodes = (0..n)
        .map(|i| {
            let r = rng(&mut s);
            Node { key: r, a: [r; 7], val: r >> 40, b: [!r; 7], next: perm[i], c: [r ^ 1; 6], hits: 0 }
        })
        .collect();
    (nodes, 0)
}

/// Sequential sweep: four hot fields per element.
#[inline(never)]
fn sweep(ps: &mut [Particle]) -> f64 {
    let mut live = 0u64;
    let mut e = 0.0;
    for p in ps.iter_mut() {
        p.x += p.vx * p.mass;
        live += p.alive;
        e += p.x;
    }
    e + live as f64
}

/// Gather: the same four fields, elements in shuffled order. The visits
/// are independent, so misses overlap and the cost is lines per element.
#[inline(never)]
fn gather(ps: &mut [Particle], order: &[u32]) -> f64 {
    let mut live = 0u64;
    let mut e = 0.0;
    for &i in order {
        let p = &mut ps[i as usize];
        p.x += p.vx * p.mass;
        live += p.alive;
        e += p.x;
    }
    e + live as f64
}

/// Shuffled walk: `steps` visits, four hot fields per node.
#[inline(never)]
fn walk(ns: &mut [Node], start: usize, steps: usize) -> u64 {
    let mut i = start;
    let mut acc = 0u64;
    for _ in 0..steps {
        let n = &mut ns[i];
        acc = acc.wrapping_add(n.val ^ (n.key & 0xff));
        n.hits += 1;
        i = n.next;
    }
    acc ^ i as u64
}

/// Cold fields are read once, so nothing is dead.
#[inline(never)]
fn cold_sum(ps: &[Particle], ns: &[Node]) -> u64 {
    let p = &ps[ps.len() / 2];
    let n = &ns[ns.len() / 2];
    p.c1 ^ p.c2 ^ p.c3 ^ p.c4 ^ p.c5 ^ p.c6 ^ p.c7 ^ p.c8 ^ p.c9 ^ p.c10 ^ p.c11 ^ p.c12
        ^ p.c13 ^ p.c14 ^ p.c15 ^ p.c16
        ^ n.a[3] ^ n.b[2] ^ n.c[1]
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1_000_000);
    let reps: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(9);
    let n = black_box(n);
    println!(
        "Particle {} bytes: x {} vx {} mass {} alive {} | Node {} bytes: key {} val {} next {} hits {}",
        size_of::<Particle>(),
        offset_of!(Particle, x),
        offset_of!(Particle, vx),
        offset_of!(Particle, mass),
        offset_of!(Particle, alive),
        size_of::<Node>(),
        offset_of!(Node, key),
        offset_of!(Node, val),
        offset_of!(Node, next),
        offset_of!(Node, hits),
    );
    let mut ps = make_particles(n);
    let (mut ns, start) = make_nodes(n);
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut seed = 0xdeadbeefcafef00du64;
    for i in (1..n).rev() {
        order.swap(i, (rng(&mut seed) % (i as u64 + 1)) as usize);
    }
    let (mut ts, mut tg, mut tw) = (Vec::new(), Vec::new(), Vec::new());
    let (mut cs, mut cw) = (0.0f64, 0u64);
    for _ in 0..reps {
        let t = Instant::now();
        cs += sweep(&mut ps);
        ts.push(t.elapsed().as_nanos() as f64 / n as f64);
        let t = Instant::now();
        cs += gather(&mut ps, &order);
        tg.push(t.elapsed().as_nanos() as f64 / n as f64);
        let t = Instant::now();
        cw = cw.wrapping_add(walk(&mut ns, start, n));
        tw.push(t.elapsed().as_nanos() as f64 / n as f64);
    }
    let hits: u64 = ns.iter().map(|n| n.hits).sum();
    println!("checksum {:.3} {} {} {}", cs, cw, hits, cold_sum(&ps, &ns));
    println!(
        "sweep {:.2} gather {:.2} walk {:.2} ns/element ({} elements, median of {})",
        median(ts),
        median(tg),
        median(tw),
        n,
        reps
    );
}

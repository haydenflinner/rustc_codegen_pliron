// PLIRON_RA2_COLDSPLIT: hot loops whose loop-carried values are live
// across (or feed) a rarely executed call. With the knob the loops should
// keep their values in caller-saved registers, the functions should save
// no callee-saved register for them, and the stores/reloads should sit in
// the cold blocks. Output must not depend on the knob.
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

static RARE: AtomicU64 = AtomicU64::new(0);

#[cold]
#[inline(never)]
fn note_rare(i: usize, v: u64) {
    RARE.fetch_add(v ^ i as u64, Ordering::Relaxed);
}

#[cold]
#[inline(never)]
fn make_err(pos: usize, byte: u8) -> u64 {
    RARE.fetch_add(1, Ordering::Relaxed);
    (pos as u64) << 8 | byte as u64
}

/// Bounds-check panic in the loop (noreturn: the values only feed it).
#[inline(never)]
fn cs_bounds(xs: &[u32], idx: &[usize]) -> u64 {
    let mut acc = 0u64;
    let mut mix = 0x9e37_79b9u64;
    for (k, &i) in idx.iter().enumerate() {
        let v = xs[i] as u64;
        acc = acc.wrapping_add(v ^ mix);
        mix = mix.rotate_left(5) ^ k as u64;
    }
    acc ^ mix
}

/// `Option::expect` in the loop (noreturn).
#[inline(never)]
fn cs_expect(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    let mut lo = u32::MAX;
    let mut hi = 0u32;
    for &x in xs {
        acc = acc.checked_add(x).expect("sum overflowed");
        lo = lo.min(x);
        hi = hi.max(x);
    }
    acc ^ lo ^ hi
}

/// `Vec::push` that may grow: the cold `grow_one` returns into the loop.
#[inline(never)]
fn cs_push(src: &[u32], out: &mut Vec<u32>) -> u64 {
    let mut acc = 0u64;
    let mut prev = 0u32;
    let mut runs = 0u64;
    for &x in src {
        if x & 3 == 0 {
            out.push(x ^ prev);
        }
        acc = acc.wrapping_mul(31).wrapping_add(x as u64);
        if x < prev {
            runs += 1;
        }
        prev = x;
    }
    acc ^ runs
}

/// A `#[cold]` call that returns: four loop-carried values live across it.
#[inline(never)]
fn cs_cold_call(xs: &[u64]) -> u64 {
    let mut a = 1u64;
    let mut b = 2u64;
    let mut c = 3u64;
    for (i, &x) in xs.iter().enumerate() {
        a = a.wrapping_add(x);
        b ^= a.rotate_left(7);
        c = c.wrapping_mul(5).wrapping_add(b);
        if x == u64::MAX {
            note_rare(i, a);
        }
    }
    a ^ b ^ c
}

/// Error path: a `#[cold]` constructor, then a return that still needs the
/// accumulators.
#[inline(never)]
fn cs_err(bytes: &[u8]) -> Result<(u64, u64), (u64, u64, u64)> {
    let mut val = 0u64;
    let mut sum = 0u64;
    let mut n = 0u64;
    for (pos, &b) in bytes.iter().enumerate() {
        let d = b.wrapping_sub(b'0');
        if d > 9 {
            if b == b' ' {
                sum = sum.wrapping_add(val);
                val = 0;
                n += 1;
                continue;
            }
            let e = make_err(pos, b);
            return Err((e, sum, n));
        }
        val = val.wrapping_mul(10).wrapping_add(d as u64);
    }
    Ok((sum.wrapping_add(val), n))
}

/// Two different cold calls in one loop, six carried values.
#[inline(never)]
fn cs_two(xs: &[u32], out: &mut Vec<u32>) -> u64 {
    let mut a = 0u64;
    let mut b = 1u64;
    let mut c = 2u64;
    let mut d = 3u64;
    for (i, &x) in xs.iter().enumerate() {
        a = a.wrapping_add(x as u64);
        b = b.rotate_left(3) ^ a;
        c = c.wrapping_add(b >> 7);
        d ^= c.wrapping_mul(0x100_0000_01b3);
        if x == 0xdead_beef {
            note_rare(i, d);
        }
        if x & 0xff == 0x7f {
            out.push(x);
        }
    }
    a ^ b ^ c ^ d
}

#[cold]
#[inline(never)]
fn flush(sink: &mut Vec<u32>, buf: &[u32; 64]) {
    sink.extend_from_slice(buf);
}

/// Buffered output: a `#[cold]` flush that returns into the loop, five
/// loop-carried values and two pointers live across it.
#[inline(never)]
fn cs_flush(src: &[u32], sink: &mut Vec<u32>) -> u64 {
    let mut buf = [0u32; 64];
    let mut fill = 0usize;
    let mut acc = 0u64;
    let mut prev = 0u32;
    let mut runs = 0u64;
    for &x in src {
        buf[fill & 63] = x ^ prev;
        fill += 1;
        if fill & 63 == 0 {
            flush(sink, &buf);
        }
        acc = acc.wrapping_mul(31).wrapping_add(x as u64);
        if x < prev {
            runs += 1;
        }
        prev = x;
    }
    acc ^ runs ^ fill as u64
}

fn main() {
    let n = 4096usize;
    let xs32: Vec<u32> = (0..n as u32).map(|i| i.wrapping_mul(2654435761) >> 8).collect();
    let idx: Vec<usize> = (0..n).map(|i| (i * 7919) % n).collect();
    let xs64: Vec<u64> = (0..n as u64)
        .map(|i| if i % 1000 == 999 { u64::MAX } else { i.wrapping_mul(0x9e37_79b9_7f4a_7c15) })
        .collect();
    let small: Vec<u32> = (0..n as u32).map(|i| i % 1000).collect();
    let mut text = Vec::new();
    for i in 0..n {
        text.extend_from_slice(format!("{} ", i * 37 % 100_000).as_bytes());
    }
    let mut bad = text.clone();
    let at = bad.len() / 2;
    bad[at] = b'x';

    let mut tot = 0u64;
    for round in 0..200u64 {
        tot = tot.wrapping_add(cs_bounds(black_box(&xs32), black_box(&idx)));
        tot = tot.wrapping_add(cs_expect(black_box(&small)) as u64);
        let mut out = Vec::new();
        tot = tot.wrapping_add(cs_push(black_box(&xs32), &mut out));
        tot = tot.wrapping_add(out.len() as u64 ^ out.iter().fold(0u64, |a, &x| a.wrapping_add(x as u64)));
        tot = tot.wrapping_add(cs_cold_call(black_box(&xs64)));
        match cs_err(black_box(&text)) {
            Ok((s, k)) => tot = tot.wrapping_add(s ^ k),
            Err(_) => unreachable!(),
        }
        match cs_err(black_box(&bad)) {
            Ok(_) => unreachable!(),
            Err((e, s, k)) => tot = tot.wrapping_add(e ^ s ^ k),
        }
        let mut sink = Vec::new();
        tot = tot.wrapping_add(cs_flush(black_box(&xs32), &mut sink));
        tot = tot.wrapping_add(sink.len() as u64 ^ sink.iter().fold(0u64, |a, &x| a.wrapping_add(x as u64)));
        let mut out2 = Vec::new();
        let mut ys = xs32.clone();
        ys[(round as usize * 13) % n] = 0xdead_beef;
        tot = tot.wrapping_add(cs_two(black_box(&ys), &mut out2));
        tot = tot.wrapping_add(out2.len() as u64);
    }
    println!("coldsplit checksum {tot:016x} rare {:016x}", RARE.load(Ordering::Relaxed));
}

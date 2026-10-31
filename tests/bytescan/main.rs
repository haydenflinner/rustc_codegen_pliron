// PLIRON_BYTESCAN=1 (gate job `optin:bytescan`): byte-set scans over a
// constant table get a 16-byte vector pre-loop. Every length, start
// alignment and stop position around the 16-byte edges, near-miss bytes
// (one bit off a stop byte), and both loop shapes (`position` over a slice
// iterator, an index `while`). Output is diffed against stock rustc's.
use std::hint::black_box;

static TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    t[b'"' as usize] = 1;
    t[b'&' as usize] = 2;
    t[b'<' as usize] = 3;
    t[b'>' as usize] = 4;
    t
};

#[inline(never)]
fn by_position(s: &[u8]) -> Option<usize> {
    s.iter().position(|&c| TABLE[c as usize] != 0)
}

#[inline(never)]
fn by_index(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && TABLE[s[i] as usize] == 0 {
        i += 1;
    }
    i
}

/// pulldown-cmark's `escape_html_scalar` loop: scan, act, continue.
#[inline(never)]
fn escape_like(s: &[u8]) -> u64 {
    let mut h = 0u64;
    let mut i = 0;
    while i < s.len() {
        match s[i..].iter().position(|&c| TABLE[c as usize] != 0) {
            Some(p) => i += p,
            None => break,
        }
        h = h.wrapping_mul(31).wrapping_add((i as u64) << 3 | TABLE[s[i] as usize] as u64);
        i += 1;
    }
    h
}

fn main() {
    let mut buf = vec![b'a'; 128];
    let near = [b'"' ^ 1, b'&' ^ 0x80, b'<' ^ 2, b'>' ^ 0x40, 0, 0xff];
    let mut h = 0u64;
    let mut mix = |v: u64| h = h.wrapping_mul(0x100000001b3).wrapping_add(v);
    for len in 0..72 {
        for off in 0..17 {
            // No stop byte, then a near-miss everywhere.
            for f in [b'a', near[(len + off) % near.len()]] {
                buf.iter_mut().for_each(|b| *b = f);
                let s = black_box(&buf[off..off + len]);
                mix(by_position(s).map_or(u64::MAX, |p| p as u64));
                mix(by_index(s, 0) as u64);
            }
            // One stop byte at each position, over near-miss filler.
            for pos in 0..len {
                buf.iter_mut().enumerate().for_each(|(k, b)| *b = near[k % near.len()]);
                buf[off + pos] = b"\"&<>"[pos % 4];
                let s = black_box(&buf[off..off + len]);
                mix(by_position(s).map_or(u64::MAX, |p| p as u64));
                mix(by_index(s, pos / 2) as u64);
                mix(escape_like(s));
            }
        }
    }
    // Long runs with sparse stops.
    let mut x = 0x2545f491u32;
    let big: Vec<u8> = (0..100_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            if x % 97 == 0 { b"\"&<>"[(x >> 8) as usize % 4] } else { b' ' + (x >> 3) as u8 % 90 }
        })
        .collect();
    for off in 0..17 {
        mix(escape_like(black_box(&big[off..])));
    }
    println!("{h:016x}");
}

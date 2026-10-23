//! LEB128 varint decoding (u64, 1–10 bytes per value).
//!
//! Three handwritten decoders and postcard. Every loop is bounded (at most
//! 10 bytes) and leaves early on the terminator byte.
//! - `decode_shift`: the classic `shift += 7` loop. The bound (10 bytes) is
//!   the `shift < 70` exit, and `buf[pos]` is a bounds check per byte.
//! - `decode_bounded`: `for i in 0..buf.len().min(10)`, a runtime trip
//!   count with a constant upper bound of 10.
//! - `decode_const`: postcard's loop shape, handwritten and inlined:
//!   `for i in 0..10 { pop()? ... }`, a constant trip count with two exits
//!   per byte (EOF, terminator).
//! - postcard (`varint_postcard`): the real thing, through serde, on a
//!   struct of four integer fields.
//!
//! The value mix is mostly small: 60% 1-byte, 20% 2-byte, 12% 3–5-byte,
//! 5% 6–7-byte, 3% full 64-bit (9–10 bytes).

use serde::{Deserialize, Serialize};
use std::hint::black_box;

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

    /// A value from the mix above.
    fn value(&mut self) -> u64 {
        let class = self.next() % 100;
        let r = self.next();
        match class {
            0..60 => r & 0x7f,
            60..80 => r & 0x3fff,
            80..92 => r & 0xffff_ffff,
            92..97 => r & 0xffff_ffff_ffff,
            _ => r,
        }
    }
}

fn encode(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Number of varints in the handwritten buffer.
const N: usize = 300_000;

pub fn setup() -> Vec<u8> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut buf = Vec::with_capacity(N * 3);
    for _ in 0..N {
        encode(rng.value(), &mut buf);
    }
    buf
}

#[inline]
fn decode_shift(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let b = buf[*pos];
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return result;
        }
        shift += 7;
        if shift >= 70 {
            return result; // malformed: more than 10 bytes
        }
    }
}

#[inline]
fn decode_bounded(buf: &[u8]) -> Option<(u64, usize)> {
    let n = buf.len().min(10);
    let mut result = 0u64;
    for i in 0..n {
        let b = buf[i];
        result |= ((b & 0x7f) as u64) << (7 * i);
        if b < 0x80 {
            return Some((result, i + 1));
        }
    }
    None
}

#[inline]
fn decode_const(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut out = 0u64;
    for i in 0..10 {
        let &b = buf.get(*pos)?;
        *pos += 1;
        out |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some(out);
        }
    }
    None
}

#[inline(never)]
pub fn sum_shift(buf: &[u8]) -> u64 {
    let mut pos = 0;
    let mut sum = 0u64;
    while pos < buf.len() {
        sum = sum.wrapping_add(decode_shift(buf, &mut pos));
    }
    sum
}

#[inline(never)]
pub fn sum_bounded(buf: &[u8]) -> u64 {
    let mut rest = buf;
    let mut sum = 0u64;
    while let Some((v, n)) = decode_bounded(rest) {
        sum = sum.wrapping_add(v);
        rest = &rest[n..];
    }
    sum
}

#[inline(never)]
pub fn sum_const(buf: &[u8]) -> u64 {
    let mut pos = 0;
    let mut sum = 0u64;
    while let Some(v) = decode_const(buf, &mut pos) {
        sum = sum.wrapping_add(v);
    }
    sum
}

pub fn run(buf: &[u8]) -> u64 {
    let a = sum_shift(black_box(buf));
    let b = sum_bounded(black_box(buf));
    let c = sum_const(black_box(buf));
    a ^ b.rotate_left(1) ^ c.rotate_left(2)
}

/// A record of several integer fields. postcard encodes every one of them
/// as a varint (`delta` zigzagged).
#[derive(Serialize, Deserialize)]
pub struct Rec {
    pub id: u32,
    pub ts: u64,
    pub len: u16,
    pub delta: i64,
}

/// Number of records in the postcard buffer.
const NREC: usize = 200_000;

pub fn setup_postcard() -> Vec<u8> {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut buf = Vec::new();
    for _ in 0..NREC {
        let r = Rec {
            id: rng.value() as u32,
            ts: rng.value(),
            len: rng.value() as u16,
            delta: rng.value() as i64 >> (rng.next() % 64),
        };
        buf.extend_from_slice(&postcard::to_allocvec(&r).unwrap());
    }
    buf
}

#[inline(never)]
pub fn sum_postcard(buf: &[u8]) -> u64 {
    let mut rest = buf;
    let mut sum = 0u64;
    while !rest.is_empty() {
        let Ok((r, tail)) = postcard::take_from_bytes::<Rec>(rest) else {
            break;
        };
        sum = sum
            .wrapping_add(r.id as u64)
            .wrapping_add(r.ts)
            .wrapping_add(r.len as u64)
            .wrapping_add(r.delta as u64);
        rest = tail;
    }
    sum
}

pub fn run_postcard(buf: &[u8]) -> u64 {
    sum_postcard(black_box(buf))
}

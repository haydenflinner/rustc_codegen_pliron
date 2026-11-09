// aarch64 narrow-int cleanliness (`PLIRON_NARROWEXT`) and shared
// `br_table` split-edge blocks (`PLIRON_EDGEDEDUP`). The first skips the
// `uxtb`/`tst #0xff` on i8/i16 values whose register is already
// zero-extended (narrow constants, `and` with a clean operand, `orr`/`eor`
// of clean operands, compares, byte loads) — so a wrong "clean" answer
// shows up as junk above bit 7 in a `bool`/`u8` return or a byte test.
// The second points `br_table` entries that pass the same block call at one
// edge block. Each case is checked against wide-arithmetic references and a
// checksum pinned from stock rustc.
use std::hint::black_box;

#[inline(never)]
fn low_bit(x: u8) -> bool {
    x & 1 != 0
}

#[inline(never)]
fn masked(x: u8, k: u8) -> u8 {
    x & k
}

#[inline(never)]
fn mixed(x: u8, y: u8) -> u8 {
    // `(x & 15) | y`: clean | dirty-in-register — must keep its extension.
    (x & 15) | y
}

#[inline(never)]
fn sum8(x: u8, y: u8) -> u8 {
    x.wrapping_add(y)
}

#[inline(never)]
fn const_or(c: bool) -> u8 {
    if c { 0xf0 } else { 7 }
}

#[inline(never)]
fn not_bool(b: &bool) -> bool {
    !*b
}

#[inline(never)]
fn cmp_eq(x: u16, y: u16) -> bool {
    x == y
}

#[inline(never)]
fn widen(x: u8, k: u8) -> u64 {
    ((x & k) as u64) << 3
}

#[inline(never)]
fn widen16(x: u16, y: u16) -> u32 {
    ((x & 0x0ff0) ^ (y & 0x00ff)) as u32
}

#[inline(never)]
fn branch_on(bytes: &[bool]) -> u32 {
    let mut n = 0;
    for &b in bytes {
        if b {
            n += 1;
        }
    }
    n
}

#[inline(never)]
fn utf8(c: u32, out: &mut [u8; 4]) -> usize {
    if c < 0x80 {
        out[0] = c as u8;
        1
    } else if c < 0x800 {
        out[0] = (c >> 6) as u8 & 0x1f | 0xc0;
        out[1] = c as u8 & 0x3f | 0x80;
        2
    } else if c < 0x10000 {
        out[0] = (c >> 12) as u8 & 0x0f | 0xe0;
        out[1] = (c >> 6) as u8 & 0x3f | 0x80;
        out[2] = c as u8 & 0x3f | 0x80;
        3
    } else {
        out[0] = (c >> 18) as u8 & 0x07 | 0xf0;
        out[1] = (c >> 12) as u8 & 0x3f | 0x80;
        out[2] = (c >> 6) as u8 & 0x3f | 0x80;
        out[3] = c as u8 & 0x3f | 0x80;
        4
    }
}

#[inline(never)]
fn or_shr(x: u8, y: u8) -> u8 {
    // `x | (y >> k)` on a clean `y` fuses to `orr .., lsr #k` under bit 32.
    x | (y >> 3)
}

#[inline(never)]
fn or_shl16(x: u16, y: u16) -> u16 {
    // `ishl` operand fuses unconditionally — its low bits are exact.
    x | (y << 2)
}

#[inline(never)]
fn and_shr(x: u8, y: u8) -> u8 {
    x & (y >> 4)
}

#[inline(never)]
fn shr_load(b: &[u8], i: usize) -> u8 {
    // `uload8` is clean: `ushr` needs no `uxtb` first under bit 32.
    b[i] >> 3
}

#[inline(never)]
fn chain(v: u8) -> u8 {
    // Clean results chain: `band`/`bor` are clean, `ushr`'s result is
    // clean, so the whole tree shifts and ors without extensions.
    let t = (v & 0xf0) | (v >> 4);
    (t >> 2) | 0x80
}

#[inline(never)]
fn dirty_shift(x: u8, y: u8) -> u8 {
    // `y + 1` is NOT clean (iadd keeps carry in bit 8): fusing
    // `x | (t >> 3)` into `orr x, t, lsr #3` would shift the carry bit
    // into the result. Must keep the `uxtb` before the `lsr`.
    x | (y.wrapping_add(1) >> 3)
}

#[inline(never)]
fn dirty_shift16(x: u16, y: u16) -> u16 {
    x | (y.wrapping_add(1) >> 5)
}

#[derive(Clone, Copy)]
enum Op {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
}

#[inline(never)]
fn dispatch(op: Op, x: u64, y: u64) -> u64 {
    // Several arms forward the same value to the join: one shared edge
    // block per distinct (target, args) under `PLIRON_EDGEDEDUP`.
    let v = match op {
        Op::A | Op::C | Op::F => x,
        Op::B | Op::H => y,
        Op::D => x ^ y,
        Op::E | Op::G => x,
    };
    v.wrapping_mul(v | 1).rotate_left(7)
}

fn main() {
    let mut acc = 0u64;
    let mut mix = |v: u64| acc = acc.wrapping_mul(0x100000001b3).wrapping_add(v);
    let ops = [Op::A, Op::B, Op::C, Op::D, Op::E, Op::F, Op::G, Op::H];
    let mut flags = Vec::new();
    for i in 0..4096u32 {
        let a = black_box((i.wrapping_mul(2654435761) >> 7) as u8);
        let b = black_box((i.wrapping_mul(40503) ^ 0x5a) as u8);
        let w = black_box(i.wrapping_mul(0x9e37) as u16);

        let r = low_bit(a);
        assert_eq!(r as u8, a & 1);
        let m = masked(a, b);
        assert_eq!(m as u32, (a as u32) & (b as u32));
        let x = mixed(a, b);
        assert_eq!(x as u32, ((a as u32) & 15) | b as u32);
        let s = sum8(a, b);
        assert_eq!(s as u32, ((a as u32) + (b as u32)) & 0xff);
        let c = const_or(a > b);
        assert_eq!(c as u32, if a > b { 0xf0 } else { 7 });
        let nb = not_bool(&r);
        assert_eq!(nb as u32, (!r) as u32);
        let e = cmp_eq(w, w ^ (a as u16 & 1));
        assert_eq!(e as u32, (a & 1 == 0) as u32);
        assert_eq!(widen(a, b), (((a as u32) & (b as u32)) as u64) << 3);
        assert_eq!(widen16(w, a as u16), ((w as u32) & 0x0ff0) ^ (a as u32));
        assert_eq!(or_shr(a, b) as u32, (a as u32) | ((b as u32) >> 3));
        assert_eq!(or_shl16(w, a as u16) as u32, (w as u32) | (((a as u16 as u32) << 2) & 0xffff));
        assert_eq!(and_shr(a, b) as u32, (a as u32) & ((b as u32) >> 4));
        assert_eq!(shr_load(black_box(&[a, b, 7, 200]), (i & 3) as usize) as u32,
                   ([a, b, 7u8, 200][(i & 3) as usize] as u32) >> 3);
        assert_eq!(chain(a) as u32, ((((a as u32) & 0xf0) | ((a as u32) >> 4)) >> 2) | 0x80);
        assert_eq!(dirty_shift(a, b) as u32,
                   (a as u32) | ((((b as u32) + 1) & 0xff) >> 3));
        assert_eq!(dirty_shift16(w, a as u16) as u32,
                   (w as u32) | ((((a as u16 as u32) + 1) & 0xffff) >> 5));
        flags.push(r);

        let cp = black_box(i.wrapping_mul(0x2f1) % 0x110000);
        if !(0xd800..0xe000).contains(&cp) {
            let mut buf = [0u8; 4];
            let n = utf8(cp, &mut buf);
            let mut want = [0u8; 4];
            let want_s = char::from_u32(cp).unwrap().encode_utf8(&mut want);
            assert_eq!(&buf[..n], want_s.as_bytes(), "utf8({cp:#x})");
            mix(u32::from_le_bytes(buf) as u64);
        }

        let op = ops[(i & 7) as usize];
        mix(dispatch(op, a as u64 * 977, b as u64 + w as u64));
        mix(r as u64 + m as u64 + x as u64 + s as u64 + c as u64 + nb as u64 + e as u64);
    }
    let n = branch_on(black_box(&flags));
    assert_eq!(n as usize, flags.iter().filter(|&&f| f).count());
    mix(n as u64);
    println!("{acc}");
    // Stock rustc value.
    assert_eq!(acc, 13935495612886148059);
}

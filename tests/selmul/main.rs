// Late select-chain folds (PLIRON_SELCHAIN) and the madd carry
// (PLIRON_MULCARRY): compare chains decided by a sibling compare, selects
// over adds sharing an operand, and `acc * K + d` loops in the shapes
// rustc's parsers produce. Output must match stock rustc's (expected.out).
use std::hint::black_box;

fn f8(x: i8, k: i8) -> u64 {
    let mut a = 0u64;
    if x.wrapping_add(3) == k {
        a += 1;
    }
    if x.wrapping_sub(5) != k {
        a += 2;
    }
    if x.wrapping_add(-7) != 10 {
        a += 4;
    }
    a
}

fn f16(x: i16, k: i16) -> u64 {
    let mut a = 0u64;
    if x.wrapping_add(100) == k {
        a += 1;
    }
    // Same offset: the second compare is decided equal, not unequal.
    if x.wrapping_add(100) != k {
        a += 2;
    }
    if x.wrapping_add(300) == -50 {
        a += 4;
    }
    a
}

fn fu8(x: u8, k: u8) -> u64 {
    let mut a = 0u64;
    // 200 and 456 agree mod 256: decided equal at u8.
    if x.wrapping_add(200) == k {
        a += 1;
    }
    if x.wrapping_add(456u16 as u8) != k {
        a += 2;
    }
    a
}

fn compares() -> u64 {
    let mut acc = 0u64;
    for i in 0..70_000i64 {
        let x = black_box(i as i8);
        let k = black_box((i * 3) as i8);
        acc = acc.wrapping_mul(3).wrapping_add(f8(x, k));
        let x = black_box(i as i16);
        let k = black_box((i * 7) as i16);
        acc = acc.wrapping_mul(3).wrapping_add(f16(x, k));
        let x = black_box(i as u8);
        let k = black_box((i * 5) as u8);
        acc = acc.wrapping_mul(3).wrapping_add(fu8(x, k));
    }
    acc
}

// Both arms add to the same loop-carried value.
fn common_add(xs: &[i32]) -> i64 {
    let mut acc = 0i64;
    for &x in xs {
        acc = if x & 1 == 0 { acc + (x as i64) } else { acc + 7 };
    }
    acc
}

fn parses() -> (u64, u64) {
    let words = [
        "", "+", "-", "0", "7", "+42", "-7", "9x", "x9", "12345678", "99999999", "123456789",
        "4294967295", "4294967296", "00000000000000000001", "18446744073709551615",
        "18446744073709551616", "-0", "+0", "1_0",
    ];
    let (mut ok, mut err) = (0u64, 0u64);
    for r in 0..3u64 {
        for w in words {
            let w = black_box(w);
            match w.parse::<u32>() {
                Ok(v) => ok = ok.wrapping_mul(31).wrapping_add(v as u64 + r),
                Err(_) => err += 1,
            }
            match w.parse::<u64>() {
                Ok(v) => ok = ok.wrapping_mul(31).wrapping_add(v),
                Err(_) => err += 1,
            }
            match w.parse::<i32>() {
                Ok(v) => ok = ok.wrapping_mul(31).wrapping_add(v as u64),
                Err(_) => err += 1,
            }
            match w.parse::<u8>() {
                Ok(v) => ok = ok.wrapping_mul(31).wrapping_add(v as u64),
                Err(_) => err += 1,
            }
        }
    }
    (ok, err)
}

#[inline(never)]
fn mac_seeded(seed: u32, ds: &[u8]) -> u32 {
    // Entry value is seed*31 once rotated: an `imul` on the entry edge.
    let mut acc = seed;
    for &d in ds {
        acc = acc.wrapping_mul(31).wrapping_add(d as u32);
    }
    acc
}

#[inline(never)]
fn mac_digits(ds: &[u8]) -> Option<u32> {
    let mut acc = 0u32;
    for &c in ds {
        acc = acc.wrapping_mul(10);
        let d = (c as u32).wrapping_sub(b'0' as u32);
        if d >= 10 {
            return None;
        }
        acc = acc.wrapping_add(d);
    }
    Some(acc)
}

// bench/benches/eqbug.rs's loop: the decided compares and the
// `acc + select` shape selchain folds.
#[inline(never)]
fn eqbug(n: i64) -> u64 {
    let mut acc = 0u64;
    for i in 0..n {
        let x = black_box(i as i8);
        let k = black_box((i * 3) as i8);
        acc = acc.wrapping_add(f8(x, k));
        let x = black_box(i as i16);
        let k = black_box((i * 7) as i16);
        acc = acc.wrapping_add(f16(x, k));
        let x = black_box(i as u8);
        let k = black_box((i * 5) as u8);
        acc = acc.wrapping_add(fu8(x, k));
    }
    acc
}

// bench/benches/parse_only.rs's loop: core's short-string digit loop
// carries `acc * 10` (mulcarry's madd shape).
#[inline(never)]
fn parse_only(n: u64) -> u64 {
    let words = ["1234", "567", "89", "12345678", "42", "", "+7", "-3", "9z", "00000009"];
    let mut acc: u64 = 0;
    for i in 0..n {
        let w = words[(i % 10) as usize];
        if let Ok(v) = w.parse::<u32>() {
            acc = acc.wrapping_add(v as u64);
        }
        black_box(&mut acc);
    }
    acc
}

fn main() {
    println!("eqbug {}", eqbug(black_box(70_000)));
    println!("parse_only {}", parse_only(black_box(1000)));
    println!("compares {}", compares());
    let xs: Vec<i32> = (0..1000).map(|i| black_box(i * 37 % 101 - 50)).collect();
    println!("common_add {}", common_add(&xs));
    let (ok, err) = parses();
    println!("parses ok={ok} err={err}");
    let bytes: Vec<u8> = (0..200u32).map(|i| black_box((i * 13 % 251) as u8)).collect();
    for seed in [0u32, 1, 7, u32::MAX] {
        println!("mac_seeded {seed} {}", mac_seeded(black_box(seed), &bytes));
    }
    for s in ["", "0", "123", "4294967295", "4294967296", "12a", "999999999999"] {
        println!("mac_digits {s:?} {:?}", mac_digits(black_box(s.as_bytes())));
    }
}

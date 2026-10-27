// coldargs rebinds every int arg of a call in a cold block to a fresh
// value. Under PLIRON_COLDCOPY=1 that rebind is `iadd x, (umulhi 0, 1)`,
// which aarch64 lowers to a register copy (or nothing). The cold calls here
// pass hot-loop values of every int width, boundary constants, zeros,
// pointers, repeated args and stack-passed args (more than 8). The program
// prints a digest that must match stock rustc's.

use std::hint::black_box;

static mut ACC: u64 = 0xcbf2_9ce4_8422_2325;

#[inline(always)]
fn mix(v: u64) {
    unsafe {
        ACC = (ACC ^ v).wrapping_mul(0x100_0000_01b3);
    }
}

#[cold]
#[inline(never)]
fn sink_narrow(a: i8, b: u8, c: i16, d: u16, e: i32, f: u32, g: bool) {
    mix(a as u64);
    mix(b as u64);
    mix(c as u64);
    mix(d as u64);
    mix(e as u64);
    mix(f as u64);
    mix(g as u64);
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn sink_wide(a: i64, b: u64, c: isize, d: usize, e: i64, f: u64, g: i64, h: u64, i: i64, j: u64, k: u32, l: i8) {
    for v in [a as u64, b, c as u64, d as u64, e as u64, f, g as u64, h, i as u64, j, k as u64, l as u64] {
        mix(v);
    }
}

#[cold]
#[inline(never)]
fn sink_ptr(p: *const u32, len: usize, q: &[u8]) {
    let s = unsafe { std::slice::from_raw_parts(p, len) };
    for &x in s {
        mix(x as u64);
    }
    for &x in q {
        mix(x as u64);
    }
}

#[cold]
#[inline(never)]
fn sink_same(a: u64, b: u64, c: u64, d: u64) {
    mix(a);
    mix(b.rotate_left(7));
    mix(c.rotate_left(13));
    mix(d.rotate_left(29));
}

#[inline(never)]
fn narrow(n: u32) -> u32 {
    let mut x: u32 = black_box(0x9e37_79b9);
    let mut cnt = 0u32;
    for i in 0..n {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223 ^ i);
        if x % 61 == 0 {
            cnt += 1;
            sink_narrow(x as i8, (x >> 8) as u8, x as i16, (x >> 16) as u16, x as i32, x.rotate_left(5), x & 1 == 1);
        }
    }
    cnt
}

#[inline(never)]
fn wide(n: u64) -> u64 {
    let mut x: u64 = black_box(0x2545_f491_4f6c_dd1d);
    let mut y: i64 = black_box(-1);
    let mut cnt = 0;
    for i in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        y = y.wrapping_add(x as i64 >> 3).wrapping_sub(i as i64);
        if x % 97 == 3 {
            cnt += 1;
            sink_wide(
                y,
                x,
                y as isize,
                i as usize,
                i64::MIN,
                u64::MAX,
                0,
                0,
                -1,
                1,
                u32::MAX,
                i8::MIN,
            );
        }
        if x % 89 == 5 {
            sink_same(x, x, y as u64, x);
        }
    }
    cnt
}

#[inline(never)]
fn ptrs(v: &[u32], bytes: &[u8]) -> u32 {
    let mut s = 0u32;
    for (i, &x) in v.iter().enumerate() {
        s = s.wrapping_add(x);
        if s % 211 == 7 {
            sink_ptr(v.as_ptr(), i + 1, &bytes[..i % bytes.len()]);
        }
    }
    s
}

// Bounds-check panics: the cold panic call takes the hot index and length.
#[inline(never)]
fn index_panics(v: &[u64], idx: &[usize]) -> u64 {
    let mut s = 0u64;
    for &i in idx {
        let r = std::panic::catch_unwind(|| v[i]);
        match r {
            Ok(x) => s = s.wrapping_add(x),
            Err(_) => s = s.wrapping_mul(31).wrapping_add(i as u64),
        }
    }
    s
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let n = black_box(200_000u32);
    let a = narrow(n);
    let b = wide(n as u64);
    let v: Vec<u32> = (0..5000u32).map(|i| i.wrapping_mul(2_654_435_761)).collect();
    let bytes: Vec<u8> = (0..37u8).collect();
    let c = ptrs(&v, &bytes);
    let w: Vec<u64> = (0..64).map(|i| i * i + 1).collect();
    let idx: Vec<usize> = (0..200).map(|i| (i * 7919) % 97).collect();
    let d = index_panics(&w, &idx);
    // Boundary values through the narrow sink directly (black_boxed so the
    // call is a real cold call with materialized args).
    for &(x, y) in &[(0i32, 0u32), (-1, u32::MAX), (i32::MIN, 1), (i32::MAX, 0x8000_0000)] {
        if black_box(true) {
            sink_narrow(x as i8, y as u8, x as i16, y as u16, x, y, x < 0);
        }
    }
    let acc = unsafe { ACC };
    println!("{a} {b} {c} {d} {acc:016x}");
}

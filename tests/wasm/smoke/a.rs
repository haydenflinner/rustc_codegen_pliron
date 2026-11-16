//! wasm emit smoke, side A: recursion, floats, statics, an indirect call and
//! a cross-object call into b.rs. No_std so nothing is imported but memory.
#![no_std]
#![feature(core_intrinsics)]
#![allow(internal_features, unused_unsafe)]

#[panic_handler]
fn p(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

unsafe extern "C" {
    fn helper(x: u32) -> u32;
}

static BLOB: [u32; 8] = [1, 2, 3, 5, 8, 13, 21, 34];

#[unsafe(no_mangle)]
pub extern "C" fn fib(n: u32) -> u32 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

#[unsafe(no_mangle)]
pub extern "C" fn sum_sq(n: u32) -> u64 {
    (1..=n as u64).map(|x| x * x).sum()
}

/// All eight ordered compares of two 128-bit values, one bit each
/// (unsigned `< <= > >=` in bits 0..4, signed in bits 4..8). The wasm
/// emitter splits an i128 compare into a high-word test and a low-word
/// test; `>=` once used `>=` for the high words too, so it was true
/// whenever the high words were equal (loro's LEB128 writer for `i128`
/// never saw its last byte).
#[unsafe(no_mangle)]
pub extern "C" fn cmp128(alo: u64, ahi: u64, blo: u64, bhi: u64) -> u32 {
    let a = (ahi as u128) << 64 | alo as u128;
    let b = (bhi as u128) << 64 | blo as u128;
    let (sa, sb) = (a as i128, b as i128);
    (a < b) as u32
        | ((a <= b) as u32) << 1
        | ((a > b) as u32) << 2
        | ((a >= b) as u32) << 3
        | ((sa < sb) as u32) << 4
        | ((sa <= sb) as u32) << 5
        | ((sa > sb) as u32) << 6
        | ((sa >= sb) as u32) << 7
}

/// One 128-bit operation per `op`, the result's low or high word by
/// `half`. `run.mjs` checks each against BigInt arithmetic: the wasm
/// emitter lowers every i128 op to pairs of i64 by hand.
#[unsafe(no_mangle)]
pub extern "C" fn ops128(alo: u64, ahi: u64, blo: u64, bhi: u64, op: u32, half: u32) -> u64 {
    let a = (ahi as u128) << 64 | alo as u128;
    let b = (bhi as u128) << 64 | blo as u128;
    let sh = (blo & 127) as u32;
    let r: u128 = match op {
        0 => a.wrapping_add(b),
        1 => a.wrapping_sub(b),
        2 => a & b,
        3 => a | b,
        4 => a ^ b,
        5 => a << sh,
        6 => a >> sh,
        7 => ((a as i128) >> sh) as u128,
        8 => a.rotate_left(sh),
        9 => a.rotate_right(sh),
        10 => a.leading_zeros() as u128,
        11 => a.trailing_zeros() as u128,
        12 => a.count_ones() as u128,
        13 => a.wrapping_neg(),
        14 => !a,
        15 => a.min(b),
        16 => a.max(b),
        17 => (a as i128).min(b as i128) as u128,
        18 => (a as i128).max(b as i128) as u128,
        19 => a.swap_bytes(),
        20 => (a == b) as u128 | ((a != b) as u128) << 1,
        21 => a.wrapping_add(b).wrapping_add((alo & 1) as u128),
        22 => if a > b { a - b } else { b - a },
        23 => (a as i128).wrapping_abs() as u128,
        24 => (alo as i64 as i128) as u128 ^ (bhi as u128) << 64,
        25 => if (a as i128) <= (b as i128) { a ^ b } else { a.wrapping_sub(b) },
        _ => 0,
    };
    if half == 0 { r as u64 } else { (r >> 64) as u64 }
}

#[unsafe(no_mangle)]
pub extern "C" fn hypot(a: f64, b: f64) -> f64 {
    unsafe { core::intrinsics::sqrtf64(a * a + b * b) }
}

#[unsafe(no_mangle)]
pub extern "C" fn statics(i: u32) -> u32 {
    BLOB[(i & 7) as usize] * 3
}

fn inc(x: u32) -> u32 {
    x + 1
}
fn dec(x: u32) -> u32 {
    x - 1
}

/// Both an indirect table call (`f`) and a call into the other object.
#[unsafe(no_mangle)]
pub extern "C" fn indirect(x: u32) -> u32 {
    let f: fn(u32) -> u32 = if x & 1 == 1 { inc } else { dec };
    f(x) + unsafe { helper(x) }
}

/// Cross-object call, unaliased.
#[unsafe(no_mangle)]
pub extern "C" fn crosscall(x: u32) -> u32 {
    unsafe { helper(x).wrapping_mul(2) }
}

#[derive(Clone, Copy)]
pub struct Gene {
    pub down: Option<bool>,
}

static GENES: [Gene; 3] = [
    Gene { down: Some(true) },
    Gene { down: None },
    Gene { down: Some(false) },
];

/// The Option's byte read widens to `sload8.i64` (discriminant + payload in
/// one i64): the emit's i32 container must sign-extend into the i64 `band`
/// or the module fails validation (`i64.and` fed a local.get of type i32).
#[unsafe(no_mangle)]
pub fn tape_hash(t: &[Gene]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for g in t {
        for m in [g.down.map(|d| d as u64)] {
            h = (h ^ m.unwrap_or(u64::MAX)).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    h
}

#[unsafe(no_mangle)]
pub fn genes_hash() -> u64 {
    tape_hash(&GENES)
}

/// What the constructors wrote: one decimal digit per run, in run order.
static CTOR_LOG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Called by every constructor (b.rs's too) with its digit.
#[unsafe(no_mangle)]
pub extern "C" fn ctor_note(digit: u32) {
    use core::sync::atomic::Ordering::Relaxed;
    CTOR_LOG.store(CTOR_LOG.load(Relaxed) * 10 + digit, Relaxed);
}

extern "C" fn ctor_default() {
    ctor_note(3);
}
extern "C" fn ctor_200() {
    ctor_note(2);
}

// Static constructors (what `inventory` and `ctor` expand to on wasm): a
// function pointer in `.init_array`, or in `.init_array.N` for priority N
// (lower runs first; no suffix is 65535). b.rs, linked after this object,
// has the priority-100 one, so the wanted order 1, 2, 3 is not link order.
// Nothing references these statics: the linker keeps them, calls them from
// `__wasm_call_ctors`, and runs that once before the first export's body
// (in a `--no-entry` link nothing used to call it).
#[used]
#[unsafe(link_section = ".init_array")]
static CTOR_DEFAULT: extern "C" fn() = ctor_default;
#[used]
#[unsafe(link_section = ".init_array.00200")]
static CTOR_200: extern "C" fn() = ctor_200;

/// The constructor log: 123 when each ran once, in priority order.
#[unsafe(no_mangle)]
pub extern "C" fn ctors() -> u32 {
    CTOR_LOG.load(core::sync::atomic::Ordering::Relaxed)
}

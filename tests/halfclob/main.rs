// `PLIRON_A64_HALFCLOB=1`: aarch64's ABI preserves the low 64 bits of
// v8-v15 across calls, so f32/f64 values may live there across calls once
// no wider value is live across that call. Shapes:
//  - doubles and floats live across libm calls (they move into d8-d15);
//  - a 128-bit vector live across a call to a local function that itself
//    keeps a double across a libm call (its FASTCC summary must still say
//    it clobbers v8-v15 above 64 bits, or the caller's vector loses its
//    upper half);
//  - a loop with several floats live across calls;
//  - doubles live across invokes (`try_call`) and read by a destructor on
//    the unwind path, caught by `catch_unwind`: the unwinder must hand the
//    landing pad d8-d15 as they were at the call.
// Output pinned against stock rustc by test.sh.
use std::hint::black_box;

#[inline(never)]
fn narrow(x: f64) -> f64 {
    let a = black_box(x).sin();
    let b = black_box(x * 2.0).cos();
    let c = black_box(x + 0.5).exp();
    a + b * x + c * a
}

#[inline(never)]
fn floats(x: f32) -> f32 {
    let a = black_box(x).ln_1p();
    let b = black_box(x * 3.0).sqrt().tanh();
    let c = black_box(a + b).sinh();
    a * b + c - x
}

#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn wide(v: &[f32; 4], x: f64) -> f32 {
    use std::arch::aarch64::*;
    unsafe {
        // The vector depends on the first call and is read after the
        // second: it is live across `narrow`, whose own code keeps a double
        // in v8-v15 across libm calls.
        let r = narrow(black_box(x));
        let q = black_box(vmulq_f32(vld1q_f32(v.as_ptr()), vdupq_n_f32(r as f32)));
        let r2 = narrow(black_box(x + 1.0));
        let q2 = vaddq_f32(q, vdupq_n_f32(r2 as f32));
        vgetq_lane_f32(q2, 0) + 2.0 * vgetq_lane_f32(q2, 1) + 3.0 * vgetq_lane_f32(q2, 2) + 4.0 * vgetq_lane_f32(q2, 3)
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(never)]
fn wide(v: &[f32; 4], x: f64) -> f32 {
    let r = narrow(x);
    let r2 = narrow(x + 1.0);
    let q: Vec<f32> = v.iter().map(|e| e * r as f32 + r2 as f32).collect();
    q[0] + 2.0 * q[1] + 3.0 * q[2] + 4.0 * q[3]
}

#[inline(never)]
fn looped(n: usize) -> (f64, f32) {
    let (mut s, mut t, mut u) = (0.0f64, 1.0f64, 0.25f32);
    for i in 0..n {
        let x = black_box(i as f64 * 0.01);
        s += x.sin();
        t = t * 0.999 + x.cos();
        u = u * 0.5 + (x as f32).exp().fract();
    }
    (s + t, u)
}

static SEEN: std::sync::Mutex<Vec<f64>> = std::sync::Mutex::new(Vec::new());

struct Guard(f64, f64);
impl Drop for Guard {
    fn drop(&mut self) {
        SEEN.lock().unwrap().push(self.0 * 3.0 + self.1);
    }
}

#[inline(never)]
fn may_panic(x: f64) -> f64 {
    if black_box(x) > 5.0 {
        panic!("x = {x}");
    }
    x.sin()
}

#[inline(never)]
fn across_invokes(x: f64) -> f64 {
    let g = Guard(black_box(x * 2.0).cos(), black_box(x + 0.25).sqrt());
    let a = may_panic(x);
    let b = may_panic(x - 1.0);
    let c = may_panic(x + 0.5);
    g.0 + g.1 * (a + b + c)
}

fn main() {
    let mut acc = 0.0f64;
    for i in 0..200 {
        acc += narrow(i as f64 * 0.013);
    }
    let mut fa = 0.0f32;
    for i in 1..200 {
        fa += floats(i as f32 * 0.07);
    }
    let v = [1.0f32, -2.5, 3.25, 0.125];
    let mut w = 0.0f32;
    for i in 0..100 {
        w += wide(black_box(&v), i as f64 * 0.02);
    }
    let (l, m) = looped(black_box(5000));
    println!("{:.9e} {:.6e} {:.6e} {:.9e} {:.6e}", acc, fa, w, l, m);
    std::panic::set_hook(Box::new(|_| {}));
    let mut ok = 0.0;
    for i in 0..12 {
        match std::panic::catch_unwind(|| across_invokes(i as f64 * 0.5)) {
            Ok(v) => ok += v,
            Err(_) => ok -= 1.0,
        }
    }
    let seen: f64 = SEEN.lock().unwrap().iter().sum();
    println!("invokes {:.9e} drops {:.9e} n {}", ok, seen, SEEN.lock().unwrap().len());
}

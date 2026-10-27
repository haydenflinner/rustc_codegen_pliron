// PLIRON_NORETTRAP=1: the trap after a call that cannot return is dropped
// when code follows it. Every path here ends in such a call (panics,
// `-> !` helpers, uninhabited returns, `_Unwind_Resume` out of cleanups,
// invokes of noreturn callees with live drops) and must unwind into the
// right landing pad, or, built with `-Cpanic=abort`, reach the panic hook.
// Output is diffed against stock rustc's.
#![allow(unreachable_code)]
use std::convert::Infallible;
use std::hint::black_box;
use std::panic;

struct D(&'static str, u32);
impl Drop for D {
    fn drop(&mut self) {
        println!("drop {} {}", self.0, self.1);
    }
}

enum Void {}

#[inline(never)]
fn die(n: u32) -> ! {
    panic!("die {n}")
}

#[inline(never)]
fn void(n: u32) -> Void {
    panic!("void {n}")
}

#[inline(never)]
fn infallible(n: u32) -> Result<u32, Infallible> {
    if n > 100 {
        panic!("infallible {n}")
    }
    Ok(n * 3)
}

/// Several noreturn calls in one function, each next to more code, with
/// drops live across some of them (invokes of noreturn callees).
#[inline(never)]
fn pick(k: u32, v: &[u32]) -> u32 {
    let _a = D("pick", k);
    match k {
        0 => v[black_box(9)],
        1 => *v.get(black_box(9)).unwrap(),
        2 => die(k),
        3 => match void(k) {},
        4 => infallible(black_box(1000)).unwrap(),
        5 => {
            let _b = D("inner", k);
            die(k + 100)
        }
        6 => u32::try_from(black_box(-1i64)).expect("neg"),
        7 => black_box(u32::MAX).checked_add(black_box(1)).unwrap(),
        8 => {
            let s = black_box("héllo");
            s[black_box(1)..black_box(2)].len() as u32
        }
        _ => v.iter().sum::<u32>() + infallible(k).unwrap(),
    }
}

/// A recursive caller: the noreturn call is deep in the stack, the frames
/// above it have cleanups, and unwinding passes through each frame.
#[inline(never)]
fn deep(n: u32, at: u32) -> u32 {
    let _d = D("deep", n);
    if n == at {
        die(n)
    }
    if n == 0 {
        return 7;
    }
    deep(n - 1, at) + 1
}

fn run(name: &str, f: impl FnOnce() -> u32 + panic::UnwindSafe) {
    match panic::catch_unwind(f) {
        Ok(v) => println!("{name}: ok {v}"),
        Err(_) => println!("{name}: caught"),
    }
}

fn main() {
    panic::set_hook(Box::new(|i| {
        let msg = i
            .payload()
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| i.payload().downcast_ref::<&str>().copied())
            .unwrap_or("?");
        println!("hook: {msg}");
        if cfg!(panic = "abort") {
            // The hook runs before the abort; end the test cleanly here.
            std::process::exit(0);
        }
    }));
    let v = [1u32, 2, 3];
    // The non-panicking paths still return through code after the calls.
    for k in [9u32, 10] {
        run("pick", move || pick(black_box(k), &v));
    }
    println!("deep: {}", deep(black_box(5), black_box(99)));
    if cfg!(panic = "abort") {
        // One panic only: the hook exits.
        pick(black_box(5), &v);
        unreachable!();
    }
    for k in 0..9u32 {
        run("pick", move || pick(black_box(k), &v));
    }
    for at in [0u32, 3, 5] {
        run("deep", move || deep(black_box(5), black_box(at)));
    }
}

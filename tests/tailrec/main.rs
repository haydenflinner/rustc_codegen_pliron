// Accumulator tail-recursion elimination (`PLIRON_TAILREC`): the
// `return op(expr, f(args))` shape must accumulate `expr` in a loop-header
// param and jump back on the call's args — identity once at entry, params
// updated per edge.
use std::hint::black_box;

#[inline(never)]
fn fib(n: u32) -> u64 {
    if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
}

#[inline(never)]
fn sumdown(n: u64) -> u64 {
    if n == 0 { 0 } else { n + sumdown(n - 1) }
}

#[inline(never)]
fn fact(n: u64) -> u64 {
    if n == 0 { 1 } else { n * fact(n - 1) }
}

#[inline(never)]
fn xacc(n: u64) -> u64 {
    if n == 0 { 0 } else { n ^ xacc(n - 1) }
}

#[inline(never)]
fn maxacc(n: u64) -> u64 {
    if n == 0 { 7 } else { n.max(maxacc(n - 1)) }
}

// Multi-param back-edge: the looped call's args update both params.
#[inline(never)]
fn ackish(n: u64, m: u64) -> u64 {
    if n == 0 { m } else { m + ackish(n - 1, m + 1) }
}

// Not associative+commutative — must not fold, still must be right.
#[inline(never)]
fn subdown(n: u64) -> u64 {
    if n == 0 { 0 } else { subdown(n - 1).wrapping_sub(n) }
}

fn main() {
    assert_eq!(fib(black_box(20)), 6765);
    assert_eq!(sumdown(black_box(100)), 5050);
    assert_eq!(fact(black_box(20)), 2432902008176640000);
    assert_eq!(xacc(black_box(6)), 7);
    assert_eq!(maxacc(black_box(10)), 10);
    assert_eq!(maxacc(black_box(3)), 7);
    assert_eq!(ackish(black_box(3), black_box(10)), 46);
    assert_eq!(subdown(black_box(5)), 0u64.wrapping_sub(15));
    println!("tailrec ok");
}

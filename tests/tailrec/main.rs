// Accumulator tail-recursion elimination (`PLIRON_TAILREC`): the
// `return op(expr, f(args))` shape must accumulate `expr` in a loop-header
// param and jump back on the call's args — identity once at entry, params
// updated per edge.
use std::hint::black_box;

#[inline(never)]
fn fib(n: u32) -> u64 {
    if n < 2 {
        n as u64
    } else {
        fib(n - 1) + fib(n - 2)
    }
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
    if n == 0 {
        0
    } else {
        subdown(n - 1).wrapping_sub(n)
    }
}

// Pure inst between the call and the accumulate: `n & 7` materializes
// after `sumband(n - 1)` returns. Must still fold — that is what makes
// deep sums O(1) stack instead of a call per level.
#[inline(never)]
fn sumband(n: u64) -> u64 {
    if n == 0 { 0 } else { sumband(n - 1) + (n & 7) }
}

// Impure inst between the call and the accumulate: `black_box` is a real
// call, so this must NOT fold — but the result must still be right.
#[inline(never)]
fn sumbar(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        sumbar(n - 1) + black_box(n & 7)
    }
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
    assert_eq!(sumband(black_box(100)), (0..=100u64).map(|n| n & 7).sum::<u64>());
    // 10M deep: crashes with a stack overflow if the fold doesn't fire.
    assert_eq!(sumband(black_box(10_000_000)), 35_000_000);
    assert_eq!(sumbar(black_box(100)), (0..=100u64).map(|n| n & 7).sum::<u64>());
    println!("tailrec ok");
}

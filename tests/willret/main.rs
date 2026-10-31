// r[verify iropt.ipo.willreturn]
// willreturn must not assume termination: Rust defines an infinite loop with
// no side effects (rustc emits no `mustprogress`), so a call to a write-free
// function that may spin forever must not be erased (callgvn dead-call elim)
// or speculated.
//
// Runtime: the worker thread never returns from `wr_spin`; if the call is
// dropped, the worker sets LOOPED and the assert fires. main then exits with
// the worker still spinning.
// Facts (checked by test.sh with PLIRON_FACTS=wr_): a CFG loop and a
// call-graph cycle each deny willreturn; a straight-line leaf keeps it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static LOOPED: AtomicBool = AtomicBool::new(false);

/// Spins forever for odd `x`: the counter skips zero.
#[inline(never)]
fn wr_spin(x: u32) -> u32 {
    let mut i = x;
    while i != 0 {
        i = i.wrapping_add(2);
    }
    0
}

/// Mutual recursion: willreturn must not hold for either member.
#[inline(never)]
fn wr_ping(x: u32) -> u32 {
    if x == 0 { 0 } else { 1 + wr_pong(x - 1) }
}
#[inline(never)]
fn wr_pong(x: u32) -> u32 {
    if x == 0 { 0 } else { 1 + wr_ping(x - 1) }
}

/// No loop, no calls: still willreturn.
#[inline(never)]
fn wr_leaf(x: u32) -> u32 {
    x.wrapping_mul(3) ^ 5
}

fn main() {
    let n = std::env::args().count() as u32; // 1
    assert_eq!(wr_ping(n + 3), 4);
    assert_eq!(wr_leaf(n), 6);
    std::thread::spawn(move || {
        let _ = wr_spin(n | 1);
        LOOPED.store(true, Ordering::SeqCst);
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(!LOOPED.load(Ordering::SeqCst), "call to a looping fn was erased");
    std::process::exit(0);
}

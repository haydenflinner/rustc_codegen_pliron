// An invoke must end its block. Inlining `fill` at a callsite with a
// cleanup pad (the `Guard` drop) turns its body's calls into invokes to
// that pad; with ops after them in the same block, block-level dominance
// credited those ops to the pad and sroa-fwd forwarded a store into
// cleanup code (nu_command `copy_on_thread`, 2026-10-22). test.sh runs this
// with PLIRON_CHECK_DOM=1 and fails on any domcheck report.
use std::hint::black_box;

struct Guard(u32);
impl Drop for Guard {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

#[inline(never)]
fn may_panic(x: usize) -> usize {
    if x == 1000 {
        panic!("big");
    }
    x + 1
}

#[inline(always)]
fn fill(v: &mut Vec<usize>, n: usize) -> [usize; 4] {
    let a = may_panic(n);
    v.push(a);
    let b = may_panic(v.len());
    v.push(b);
    [a, b, v.len(), v[0]]
}

#[inline(never)]
fn run(n: usize) -> usize {
    let _g = Guard(n as u32);
    let mut v = Vec::new();
    let r = fill(&mut v, n);
    r.iter().sum()
}

fn main() {
    println!("{}", run(black_box(3)));
    let caught = std::panic::catch_unwind(|| run(black_box(1000)));
    println!("caught {}", caught.is_err());
}

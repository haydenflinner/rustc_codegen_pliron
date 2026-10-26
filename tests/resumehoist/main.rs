// An inlined callee's `_Unwind_Resume` becomes an invoke of the caller's
// own pad. Under PLIRON_INLINE_RESUME_DOM/HOIST the inliner ends the
// callee's locals there — moving a branch-local alloca to the clone's
// entry first when its block doesn't dominate the resume — while the
// caller's locals must stay live into its pad, whose drops read them.
// Stage-2 `check_expr_kind` recovered 2.9 KB of frame this way.
use std::hint::black_box;
use std::panic::{catch_unwind, AssertUnwindSafe};

struct Loud([u64; 8], &'static str);

impl Drop for Loud {
    fn drop(&mut self) {
        println!("drop {} sum={}", self.1, self.0.iter().sum::<u64>());
    }
}

#[inline(never)]
fn may_panic(x: u64) -> u64 {
    if black_box(x) % 5 == 0 {
        panic!("boom {}", x);
    }
    x * 3
}

// Branch-local escaped arrays, plus a droppable, so the callee has a pad
// and a resume that neither branch block dominates.
#[inline]
fn callee(x: u64) -> u64 {
    let _g = Loud([x; 8], "callee guard");
    if black_box(x) & 1 == 0 {
        let a = [x + 1; 16];
        black_box(&a);
        may_panic(a[3])
    } else {
        let b = [x + 2; 24];
        black_box(&b);
        may_panic(b[5])
    }
}

#[inline(never)]
fn caller(x: u64) -> u64 {
    let keep = Loud([x * 10; 8], "caller keep");
    black_box(&keep);
    let r = callee(x);
    let after = Loud([r; 8], "caller after");
    black_box(&after);
    r + callee(x + 1)
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    for x in [1u64, 3, 4, 8, 9] {
        match catch_unwind(AssertUnwindSafe(|| caller(x))) {
            Ok(v) => println!("caller({x}) = {v}"),
            Err(_) => println!("caller({x}) panicked"),
        }
    }
}

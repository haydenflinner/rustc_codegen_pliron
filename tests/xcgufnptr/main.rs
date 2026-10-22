// A function compared by address must keep one address: zlib-rs picks
// its free path with `self.zfree == Allocator::RUST.zfree`. xcgu copied
// the small `extern "C"` callbacks into other CGUs as internal duplicates,
// so the pointer stored in one CGU no longer matched the one compared in
// another and `deflateEnd` freed a pointer malloc never handed out (uiua's
// xlsx writer, 2026-10-22). Built with -C codegen-units=16; `inline(never)`
// keeps rustc itself from giving each CGU a local copy (as stock does for
// small fns), like zlib-rs's larger callbacks.
#![allow(unpredictable_function_pointer_comparisons)]
use std::hint::black_box;

pub struct Alloc {
    pub zalloc: unsafe extern "C" fn(usize) -> usize,
}

#[inline(never)]
unsafe extern "C" fn zalloc_rust(n: usize) -> usize {
    n + 1
}

pub const RUST: Alloc = Alloc { zalloc: zalloc_rust };

macro_rules! sites {
    ($($m:ident),*) => {$(
        mod $m {
            #[inline(never)]
            pub fn make() -> crate::Alloc {
                // A direct call makes zalloc_rust a copy candidate here.
                let _ = unsafe { crate::zalloc_rust(std::hint::black_box(1)) };
                crate::Alloc { zalloc: crate::RUST.zalloc }
            }
            #[inline(never)]
            pub fn is_rust(a: &crate::Alloc) -> bool {
                a.zalloc == crate::RUST.zalloc
            }
        }
    )*};
}
sites!(a, b, c, d, e, f);

fn main() {
    let made = [a::make(), b::make(), c::make(), d::make(), e::make(), f::make()];
    let checks = [a::is_rust, b::is_rust, c::is_rust, d::is_rust, e::is_rust, f::is_rust];
    let mut bad = 0;
    for m in &made {
        for c in &checks {
            if !c(black_box(m)) {
                bad += 1;
            }
        }
    }
    println!("mismatches {bad}");
    assert_eq!(bad, 0);
}

// xcgu copies a small callee from another CGU; its deps must have bodies
// here too. `needs_body` exempted every item *named* `rotate_left`, meant
// for the intrinsic the backend lowers inline, so the ordinary method
// `usize::rotate_left` was left out of a copy's closure and the link
// failed with an undefined symbol (nushell `nu-cmd-lang` tests,
// 2026-10-22). Built with -Copt-level=1 so the method stays a call.
use std::hint::black_box;

mod hash {
    #[derive(Default)]
    pub struct Fx(pub usize);
    impl Fx {
        #[inline]
        pub fn add(&mut self, w: usize) {
            self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
}

macro_rules! users {
    ($($m:ident),*) => {$(
        mod $m {
            #[inline(never)]
            pub fn run(xs: &[usize]) -> usize {
                let mut h = crate::hash::Fx::default();
                for &x in xs { h.add(x); }
                h.0
            }
        }
    )*};
}
users!(a, b, c, d, e, f, g, h);

fn main() {
    let xs = black_box([1usize, 2, 3, 4]);
    let r = [a::run(&xs), b::run(&xs), c::run(&xs), d::run(&xs),
             e::run(&xs), f::run(&xs), g::run(&xs), h::run(&xs)];
    assert!(r.iter().all(|&v| v == r[0]));
    println!("{}", r[0] % 1000);
}

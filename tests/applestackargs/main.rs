// Apple arm64 packs stack arguments at their natural size: after the 8
// integer argument registers, a `u16` sits at sp+8 and a `bool` at
// sp+0xa. Cranelift widened an extended (`uext`) narrow stack argument to
// the full word and stored/loaded 8 bytes, clobbering the next bytes of
// the caller's frame — here the start of a by-value array's copy — and
// reading neighbours' bytes as the extension (rustlantis seed 2598, -O0).
// Output must match stock rustc's (expected.out); built at -O0, where
// internal calls use the apple_aarch64 convention.
use std::hint::black_box;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ret {
    pub a: isize,
    pub b: isize,
    pub c: isize,
    pub d: isize,
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn callee(
    a: isize,
    b: char,
    c: char,
    d: isize,
    e: isize,
    f: [isize; 8],
    g: [isize; 8],
    h: char,
    i: isize,
    j: u16,
    k: bool,
) -> Ret {
    Ret {
        a: f.iter().sum::<isize>() ^ g[0],
        b: a + d + e + i + j as isize + k as isize,
        c: b as isize + c as isize + h as isize,
        d: g.iter().sum::<isize>() ^ f[0],
    }
}

fn main() {
    let x: [isize; 8] = black_box([-1, 2, 3, 4, 5, 6, 7, 8]);
    let r = callee(black_box(1), 'a', 'b', 4, 5, x, x, 'c', 9, black_box(4681), black_box(false));
    println!("{r:?}");
    let r = callee(black_box(-3), 'x', 'y', -4, -5, x, x, 'z', -9, black_box(0xffff), black_box(true));
    println!("{r:?}");
}

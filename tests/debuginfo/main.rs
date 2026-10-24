//! `-g` (src/debuginfo.rs, pliron-clif-lower/src/dwarf.rs): std's own
//! panic backtrace — which reads our DWARF — must show the inlined
//! chain leaf → mid → top with each frame's source line. The `// L:`
//! markers are what test.sh greps the expected lines from.

#[derive(Clone, Copy)]
struct Pair {
    a: u32,
    b: i64,
}

#[inline(always)]
fn leaf(v: &[u32], i: usize) -> u32 {
    v[i] * 2 // L:leaf
}

#[inline(always)]
fn mid(v: &[u32], i: usize) -> u32 {
    leaf(v, i + 1) + 1 // L:mid
}

#[inline(never)]
fn top(v: &[u32], p: Pair) -> u32 {
    let w = mid(v, p.a as usize); // L:top
    w * 3 + p.b as u32
}

fn main() {
    let v = vec![1, 2, 3];
    let p = Pair { a: std::env::args().count() as u32 + 4, b: -7 };
    println!("{}", top(&v, p)); // L:main
}

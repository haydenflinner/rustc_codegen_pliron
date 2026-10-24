//! `-g` (src/debuginfo.rs, pliron-clif-lower/src/dwarf.rs): std's own
//! panic backtrace — which reads our DWARF — must show the inlined
//! chain leaf → mid → top with each frame's source line. The `// L:`
//! markers are what test.sh greps the expected lines from.

/// A static: a CU-level variable lldb finds by its path.
static LIMIT: u32 = 77;

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

/// Optimized builds: variables live in registers, found through location
/// lists (pliron-clif-lower/src/dwarf.rs) — the loop-carried `acc` through
/// its loop header's block param.
#[inline(never)]
fn sum(v: &[u32], k: u32) -> u32 {
    let mut acc = 0;
    for &x in v {
        acc += x * k;
        if acc > 6 {
            sink(acc); // L:sum
        }
    }
    acc
}

/// Data-carrying enums (`DW_TAG_variant_part`) and a shadowed name in a
/// lexical block: lldb must show the inner `n`.
#[inline(never)]
fn shapes(n: u32) -> u32 {
    let o: Option<u32> = Some(n + 1);
    let r: Result<u8, i16> = Err(-9);
    {
        let n = n * 10;
        sink(n); // L:shapes
    }
    o.unwrap_or(0) + r.unwrap_or(0) as u32 + std::hint::black_box(&LIMIT)
}

/// Stepping (O0), rustc's basic-stepping: `next` from `let d` must land
/// on `let h` — the lines between have no code of their own, and a
/// constant created by an earlier statement must not start a row for it.
#[inline(never)]
#[allow(unused_assignments, unused_variables)]
fn steps() -> usize {
    let a = ();
    let b: [i32; 0] = [];
    let mut c = 27;
    let d = c = 99; // L:steps
    let e = "hi bob";
    let f = b"hi bob";
    let g = b'9';
    let h = ["whatever"; 8]; // L:steps2
    let i = [1, 2, 3, 4];
    let j = (23, "hi");
    let k = 2..3;
    let l = &i[k];
    let m: *const () = &a;
    l.len()
}

#[inline(never)]
fn sink(x: u32) {
    std::hint::black_box(x);
}

fn main() {
    let v = vec![1, 2, 3];
    let p = Pair { a: std::env::args().count() as u32 + 4, b: -7 };
    let s = String::from("hi");
    let w = vec![7u8, 8];
    println!("{} {} {} {} {}", s.len(), w.len(), sum(&v, p.a), shapes(p.a), steps()); // L:main2
    // Panics (index 6): the backtrace test.
    println!("{}", top(&v, p)); // L:main
}

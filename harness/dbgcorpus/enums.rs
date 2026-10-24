// Option, Result, data-carrying enums, match bindings.
use std::hint::black_box;

#[derive(Debug, Clone)]
enum Shape {
    Circle { r: f32 },
    Square(u32),
    Poly(Vec<(i32, i32)>),
    Empty,
}

#[inline(never)]
fn describe(s: &Shape) -> u64 {
    match s {
        Shape::Circle { r } => {
            let a = (r * r * 3.14) as u64;
            sink(a); // BREAK
            a
        }
        Shape::Square(side) => {
            let a = (*side as u64).pow(2);
            sink(a); // BREAK
            a
        }
        Shape::Poly(pts) => {
            let n = pts.len() as u64;
            sink(n); // BREAK
            n
        }
        Shape::Empty => 0,
    }
}

#[inline(never)]
fn parse_num(s: &str) -> Result<i32, String> {
    let t = s.trim();
    let r: Result<i32, _> = t.parse();
    let out = r.map_err(|e| format!("{e}"));
    sink(&out); // BREAK
    out
}

#[inline(never)]
fn pick(xs: &[u16], want: u16) -> Option<usize> {
    let found = xs.iter().position(|&x| x == want);
    let doubled = found.map(|i| i * 2);
    sink(doubled); // BREAK
    found
}

fn main() {
    let n = std::env::args().count() as u32;
    let shapes = vec![Shape::Circle { r: 1.5 }, Shape::Square(n + 2), Shape::Poly(vec![(1, 2), (3, 4)]), Shape::Empty];
    let mut t = 0;
    for s in &shapes {
        t += describe(s);
    }
    println!("{t}");
    println!("{:?} {:?}", parse_num(" 42 "), parse_num("x"));
    println!("{:?}", pick(&[3, 5, 7, 9], 7 + n as u16 - 1));
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}

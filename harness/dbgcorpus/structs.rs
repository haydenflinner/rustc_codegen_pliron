// Structs, methods, tuples, arrays.
use std::hint::black_box;

#[derive(Clone, Copy, Debug)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug)]
struct Rect {
    origin: Point,
    size: (u32, u32),
    tag: u8,
}

impl Rect {
    #[inline(never)]
    fn area(&self) -> u64 {
        let w = self.size.0 as u64;
        let h = self.size.1 as u64;
        let a = w * h;
        sink(self.tag); // BREAK
        a
    }

    #[inline(never)]
    fn grow(&mut self, by: u32) -> Point {
        let old = self.size;
        self.size.0 += by;
        self.size.1 += by * 2;
        let c = Point { x: self.origin.x + self.size.0 as f64 / 2.0, y: self.origin.y };
        sink(old); // BREAK
        c
    }
}

#[inline(never)]
fn dist(a: Point, b: Point) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let d2 = dx * dx + dy * dy;
    sink(d2); // BREAK
    d2.sqrt()
}

#[inline(never)]
fn table(seed: u32) -> u32 {
    let arr = [seed, seed + 1, seed * 3, 7];
    let pair = (arr[1], arr[2] as i16);
    let s: u32 = arr.iter().sum();
    sink(&arr); // BREAK
    s + pair.0 + pair.1 as u32
}

fn main() {
    let n = std::env::args().count() as u32;
    let mut r = Rect { origin: Point { x: 1.5, y: -2.0 }, size: (3 + n, 4), tag: 9 };
    println!("{}", r.area());
    let c = r.grow(n);
    println!("{:?} {}", c, dist(c, r.origin));
    println!("{}", table(n + 10));
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}

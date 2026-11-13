// Guarded devirtualization (`PLIRON_DEVIRT=1`). `Named` has two impls:
// its calls become `fnptr == Circle::name` / `fnptr == Square::name`
// guards with direct calls. `Shape` has three; `Odd::area` is
// `#[track_caller]`, so its vtable slot holds a shim that is never a
// candidate and `area` is guarded for the other two, with `Odd` taking
// the indirect fallback arm. The other `Shape` methods have three
// candidates: left alone at the default `PLIRON_DEVIRT_MAX=2` unless a
// profile ranks them (`PLIRON_PGO` / `PLIRON_PGO_USE`, see docs/pgo.md).
// Output must be identical with and without the knob.

use std::hint::black_box;

trait Shape {
    fn area(&self) -> u64;
    fn scale(&mut self, k: u64);
    // Provided method: resolves to the trait's body per impl type.
    fn double_area(&self) -> u64 {
        self.area() * 2
    }
    fn into_area(self: Box<Self>) -> u64;
}

struct Circle(u64);
struct Square(u64);
struct Odd(u64);

impl Shape for Circle {
    fn area(&self) -> u64 {
        3 * self.0 * self.0
    }
    fn scale(&mut self, k: u64) {
        self.0 *= k;
    }
    fn into_area(self: Box<Self>) -> u64 {
        self.area() + 1
    }
}

impl Shape for Square {
    fn area(&self) -> u64 {
        self.0 * self.0
    }
    fn scale(&mut self, k: u64) {
        self.0 += k;
    }
    fn double_area(&self) -> u64 {
        self.area() * 2 + 7
    }
    fn into_area(self: Box<Self>) -> u64 {
        self.area() + 2
    }
}

impl Shape for Odd {
    #[track_caller]
    fn area(&self) -> u64 {
        self.0 + std::panic::Location::caller().line() as u64 * 0
    }
    fn scale(&mut self, k: u64) {
        self.0 ^= k;
    }
    fn into_area(self: Box<Self>) -> u64 {
        self.0 + 3
    }
}

trait Named {
    fn name(&self) -> String;
    fn len(&self) -> usize;
}

impl Named for Circle {
    fn name(&self) -> String {
        format!("circle({})", self.0)
    }
    fn len(&self) -> usize {
        6
    }
}

impl Named for Square {
    fn name(&self) -> String {
        format!("square({})", self.0)
    }
    fn len(&self) -> usize {
        if self.0 == 13 {
            panic!("unlucky square");
        }
        6
    }
}

#[inline(never)]
fn sum_ref(shapes: &[&dyn Shape]) -> u64 {
    let mut s = 0u64;
    for sh in shapes {
        s = s.wrapping_add(sh.area()).wrapping_add(sh.double_area());
    }
    s
}

#[inline(never)]
fn sum_box(shapes: &mut [Box<dyn Shape>]) -> u64 {
    let mut s = 0u64;
    for (i, sh) in shapes.iter_mut().enumerate() {
        sh.scale(i as u64 + 1);
        s = s.wrapping_mul(31).wrapping_add(sh.area());
    }
    s
}

#[inline(never)]
fn names(v: &[Box<dyn Named>]) -> String {
    // `name` returns through an sret pointer; `len` can unwind with a live
    // `String` in the caller (an invoke with a cleanup edge).
    let mut out = String::new();
    for n in v {
        let s = n.name();
        out.push_str(&s);
        out.push_str(&n.len().to_string());
        out.push(';');
    }
    out
}

fn main() {
    let (c, q, o) = (Circle(black_box(2)), Square(black_box(5)), Odd(black_box(11)));
    let refs: [&dyn Shape; 5] = [&c, &q, &o, &q, &c];
    let mut total = 0u64;
    for _ in 0..1000 {
        total = total.wrapping_add(sum_ref(black_box(&refs)));
    }
    println!("ref: {total}");

    let mut boxes: Vec<Box<dyn Shape>> = vec![
        Box::new(Circle(1)),
        Box::new(Square(2)),
        Box::new(Odd(3)),
        Box::new(Circle(4)),
    ];
    let mut total = 0u64;
    for _ in 0..10 {
        total = total.wrapping_add(sum_box(black_box(&mut boxes)));
    }
    println!("box: {total}");
    let consumed: u64 = boxes.into_iter().map(|b| b.into_area()).fold(0, u64::wrapping_add);
    println!("into: {consumed}");

    let named: Vec<Box<dyn Named>> = vec![Box::new(Circle(7)), Box::new(Square(9))];
    println!("names: {}", names(&named));
    let bad: Vec<Box<dyn Named>> = vec![Box::new(Circle(1)), Box::new(Square(13))];
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| names(&bad)));
    println!("unwind: {}", r.is_err());
}

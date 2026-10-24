// Trait objects, Box<dyn>, recursion.
use std::hint::black_box;

trait Animal {
    fn name(&self) -> &str;
    fn legs(&self) -> u32;
}

struct Dog {
    name: String,
    age: u8,
}

struct Bird {
    wingspan: f32,
}

impl Animal for Dog {
    fn name(&self) -> &str {
        &self.name
    }
    #[inline(never)]
    fn legs(&self) -> u32 {
        let base = 4u32;
        sink(self.age); // BREAK
        base
    }
}

impl Animal for Bird {
    fn name(&self) -> &str {
        "bird"
    }
    fn legs(&self) -> u32 {
        2
    }
}

#[inline(never)]
fn census(zoo: &[Box<dyn Animal>]) -> u32 {
    let mut legs = 0;
    for a in zoo {
        let l = a.legs();
        legs += l;
    }
    let first = zoo[0].name();
    sink(first); // BREAK
    legs
}

#[inline(never)]
fn depth(n: u32, acc: u64) -> u64 {
    if n == 0 {
        sink(acc); // BREAK
        return acc;
    }
    depth(n - 1, acc * 3 + n as u64)
}

fn main() {
    let zoo: Vec<Box<dyn Animal>> = vec![
        Box::new(Dog { name: "rex".to_string(), age: 3 }),
        Box::new(Bird { wingspan: 0.4 }),
    ];
    println!("{}", census(&zoo));
    let n = std::env::args().count() as u32;
    println!("{}", depth(4 + n, 1));
    if let Some(b) = zoo.get(1) {
        black_box(b.legs());
    }
    let _ = Bird { wingspan: 1.0 }.wingspan;
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}

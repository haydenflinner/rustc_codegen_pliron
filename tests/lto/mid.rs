// Middle rlib: generics from `base` instantiated here, std collections,
// and a trait object handed across the crate boundary.
use std::collections::BTreeMap;

pub use base::Shape;

pub struct Square(pub f64);
impl base::Shape for Square {
    fn area(&self) -> f64 {
        self.0 * self.0
    }
}

pub fn shapes() -> Vec<Box<dyn base::Shape>> {
    vec![Box::new(base::Rect(2.0, 3.0)), Box::new(Square(1.5))]
}

pub fn histogram(words: &str) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for w in words.split_whitespace() {
        *m.entry(w.to_lowercase()).or_insert(0) += 1;
    }
    m
}

pub fn sums() -> (u64, u64) {
    (base::total(&[1u8, 2, 3, 250]), base::total(&[70000u32, 5]))
}

/// Generic over the caller's closure: the panic in `f` unwinds through
/// `base::with_depth`'s frame and this one.
pub fn nested<R>(f: impl FnOnce(u32) -> R) -> R {
    let _g = base::Guard("mid::nested");
    base::with_depth(|a| base::with_depth(|b| f(a * 10 + b)))
}

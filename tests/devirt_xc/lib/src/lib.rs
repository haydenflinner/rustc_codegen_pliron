//! The library half: it calls closures the binary defines (the target is
//! downstream) and hands out closures the binary calls (the vtable and
//! the target are upstream).

/// Not generic: compiled here, calls whatever closure the caller passes.
/// Its hot target lives in the downstream crate, so the guard is only
/// emitted with `PLIRON_DEVIRT_DOWNSTREAM=1`.
#[inline(never)]
pub fn run(f: &mut dyn FnMut(u32) -> u32, n: u32) -> u32 {
    let mut s = 0u32;
    for i in 0..n {
        s = s.wrapping_add(f(i));
    }
    s
}

/// Closures defined (and unsized) here; the binary calls them through
/// `dyn Fn`. Their bodies are private to this crate until promoted.
#[inline(never)]
pub fn make(k: u32) -> Box<dyn Fn(u32) -> u32> {
    if k % 2 == 0 {
        Box::new(move |x| x.wrapping_mul(k).wrapping_add(7))
    } else {
        Box::new(move |x| x ^ k.rotate_left(3))
    }
}

/// Generic: instantiated in the binary, where it calls a `dyn Fn` whose
/// vtable was built by `make` above.
pub fn fold<T: Copy + Into<u64>>(f: &dyn Fn(u32) -> u32, xs: &[T]) -> u64 {
    xs.iter().fold(0u64, |a, &x| a.wrapping_mul(31).wrapping_add(f(x.into() as u32) as u64))
}

pub trait Op {
    fn apply(&self, x: u64) -> u64;
}
pub struct Add(pub u64);
pub struct Mul(pub u64);
impl Op for Add {
    fn apply(&self, x: u64) -> u64 {
        x.wrapping_add(self.0)
    }
}
impl Op for Mul {
    fn apply(&self, x: u64) -> u64 {
        x.wrapping_mul(self.0)
    }
}

/// Upstream trait objects with public impl methods.
#[inline(never)]
pub fn ops() -> Vec<Box<dyn Op>> {
    vec![Box::new(Add(3)), Box::new(Mul(5)), Box::new(Add(11))]
}

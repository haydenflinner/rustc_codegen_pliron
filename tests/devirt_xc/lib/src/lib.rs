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

// What the closures of `make2` use, all private to this crate: a
// downstream copy of such a closure links only if these are exported
// with it (`devirt::provide` in the backend).
static TABLE: [u32; 4] = [3, 5, 7, 11];
static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
thread_local! {
    static SUM: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[inline(never)]
fn mix(x: u32, k: u32) -> u32 {
    (x ^ k).rotate_left(k & 31).wrapping_mul(0x9e37_79b9)
}

fn small(x: u32) -> u32 {
    x.wrapping_add(1)
}

/// Calls the closures of `make2` made so far through `COUNT`.
pub fn count() -> u32 {
    COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Like `make`, with closures whose bodies reach into this crate: a
/// private function that is not inlinable, a private static, a small
/// private function, a nested closure, and (not copyable downstream) a
/// thread local.
#[inline(never)]
pub fn make2(k: u32) -> Box<dyn Fn(u32) -> u32> {
    match k % 4 {
        0 => Box::new(move |x| mix(x, k).wrapping_add(TABLE[(x & 3) as usize])),
        1 => Box::new(move |x| {
            COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            small(x) ^ k
        }),
        2 => Box::new(move |x| {
            let g = |y: u32| mix(y, k ^ 5);
            g(x).wrapping_add(g(x.wrapping_add(1)))
        }),
        _ => Box::new(move |x| {
            SUM.with(|c| {
                c.set(c.get().wrapping_add(x ^ k));
                c.get()
            })
        }),
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

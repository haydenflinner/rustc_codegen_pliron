// Bottom rlib of the fat-LTO test (main -> mid -> base). Its objects only
// reach the final link through the backend's fat-LTO module: rustc drops
// upstream Rust objects from the link line under -Clto.
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

pub static CTOR_RAN: AtomicU32 = AtomicU32::new(0);

extern "C" fn ctor() {
    CTOR_RAN.fetch_add(1, Ordering::SeqCst);
}

// A static initializer nobody references: only `#[used]` keeps it, and it
// has to survive the merge into the LTO module and run before main.
#[used]
#[cfg_attr(target_vendor = "apple", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".init_array"))]
static CTOR: extern "C" fn() = ctor;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> String {
        "shape".to_string()
    }
}

pub struct Rect(pub f64, pub f64);
impl Shape for Rect {
    fn area(&self) -> f64 {
        self.0 * self.1
    }
    fn name(&self) -> String {
        format!("rect {}x{}", self.0, self.1)
    }
}

/// Generic: instantiated in the downstream crates, not here.
pub fn total<T: Copy + Into<u64>>(xs: &[T]) -> u64 {
    xs.iter().map(|&x| x.into()).sum()
}

pub struct Guard(pub &'static str);
impl Drop for Guard {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

/// Unwinds out of this crate into the caller's `catch_unwind`.
#[inline(never)]
pub fn checked(n: u32) -> u32 {
    let _g = Guard("base::checked");
    if n > 3 {
        panic!("base panic {n}");
    }
    n * 10
}

/// Calls back into the downstream crate; a panic there unwinds through
/// this frame (running its drop and TLS bookkeeping on the way out).
#[inline(never)]
pub fn with_depth<R>(f: impl FnOnce(u32) -> R) -> R {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get() - 1));
        }
    }
    let d = DEPTH.with(|d| {
        d.set(d.get() + 1);
        d.get()
    });
    let _p = Pop;
    f(d)
}

pub fn depth() -> u32 {
    DEPTH.with(|d| d.get())
}

#[unsafe(no_mangle)]
pub extern "C" fn lto_base_answer(x: u32) -> u32 {
    x.wrapping_mul(2654435761) >> 16
}

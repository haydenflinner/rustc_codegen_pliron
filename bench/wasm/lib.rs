//! wasm32 bench kernels: the same sources as `../benches` compiled as a
//! cdylib so the whole crate goes through whichever codegen backend built
//! it (LLVM or pliron — run.sh builds both). `run.mjs` instantiates both
//! modules in node and times each `k_<name>(iters) -> f64` export, like
//! criterion times `b.iter`.
//!
//! `k_<name>_prep()` restocks an input the kernel consumes (sortw, matmul,
//! nbody): run.mjs calls it before each timed call, outside the timing —
//! the same setup/run split criterion's `iter_batched` makes. Kernels that
//! only read their input keep it in a `Slot` initialized on first call,
//! like criterion hoisting `setup()` out of `b.iter`.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::hint::black_box;

#[path = "../benches/chars_alpha.rs"]
mod chars_alpha;
#[path = "../benches/chars_ascii.rs"]
mod chars_ascii;
#[path = "../benches/chars_mixed.rs"]
mod chars_mixed;
#[path = "../benches/contains_rng.rs"]
mod contains_rng;
#[path = "../benches/eqbug.rs"]
mod eqbug;
#[path = "../benches/fib.rs"]
mod fib;
#[path = "../benches/hashmap.rs"]
mod hashmap;
#[path = "../benches/hmget.rs"]
mod hmget;
#[path = "../benches/iterw.rs"]
mod iterw;
#[path = "../benches/matmul.rs"]
mod matmul;
#[path = "../benches/nbody.rs"]
mod nbody;
#[path = "../benches/parse_only.rs"]
mod parse_only;
#[path = "../benches/sieve.rs"]
mod sieve;
#[path = "../benches/sortw.rs"]
mod sortw;
#[path = "../benches/strwork.rs"]
mod strwork;
#[path = "../benches/tokenize.rs"]
mod tokenize;

/// Kernel input that outlives one call. The module is single-threaded, so
/// a bare UnsafeCell in a static is enough.
struct Slot<T>(UnsafeCell<Option<T>>);
unsafe impl<T> Sync for Slot<T> {}
impl<T> Slot<T> {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }
    #[expect(clippy::mut_from_ref)]
    fn get(&self) -> &mut Option<T> {
        unsafe { &mut *self.0.get() }
    }
}

fn repeat(iters: u32, mut f: impl FnMut() -> f64) -> f64 {
    let mut acc = 0.0;
    for _ in 0..iters {
        acc += f();
    }
    acc
}

// -- setup() -> String, run(&str) -> usize -------------------------------

static CHARS_ALPHA_S: Slot<String> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_chars_alpha(iters: u32) -> f64 {
    let s = CHARS_ALPHA_S.get().get_or_insert_with(chars_alpha::setup);
    repeat(iters, || chars_alpha::run(black_box(s)) as f64)
}

static CHARS_ASCII_S: Slot<String> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_chars_ascii(iters: u32) -> f64 {
    let s = CHARS_ASCII_S.get().get_or_insert_with(chars_ascii::setup);
    repeat(iters, || chars_ascii::run(black_box(s)) as f64)
}

static CHARS_MIXED_S: Slot<String> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_chars_mixed(iters: u32) -> f64 {
    let s = CHARS_MIXED_S.get().get_or_insert_with(chars_mixed::setup);
    repeat(iters, || chars_mixed::run(black_box(s)) as f64)
}

static TOKENIZE_S: Slot<String> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_tokenize(iters: u32) -> f64 {
    let s = TOKENIZE_S.get().get_or_insert_with(tokenize::setup);
    repeat(iters, || tokenize::run(black_box(s)) as f64)
}

// -- setup() once, run(&T) -----------------------------------------------

static CONTAINS_RNG_S: Slot<Vec<u32>> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_contains_rng(iters: u32) -> f64 {
    let v = CONTAINS_RNG_S.get().get_or_insert_with(contains_rng::setup);
    repeat(iters, || {
        contains_rng::run(black_box(v), black_box(100), black_box(60_000)) as f64
    })
}

static HMGET_S: Slot<HashMap<u64, u32>> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_hmget(iters: u32) -> f64 {
    let m = HMGET_S.get().get_or_insert_with(hmget::setup);
    repeat(iters, || hmget::run(black_box(m)) as f64)
}

static ITERW_S: Slot<Vec<i32>> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_iterw(iters: u32) -> f64 {
    let a = ITERW_S.get().get_or_insert_with(iterw::setup);
    repeat(iters, || iterw::run(black_box(a)) as f64)
}

// -- run() only -----------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn k_eqbug(iters: u32) -> f64 {
    repeat(iters, || eqbug::run() as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn k_fib(iters: u32) -> f64 {
    repeat(iters, || fib::run() as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn k_hashmap(iters: u32) -> f64 {
    repeat(iters, || hashmap::run() as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn k_parse_only(iters: u32) -> f64 {
    repeat(iters, || parse_only::run() as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn k_sieve(iters: u32) -> f64 {
    repeat(iters, || sieve::run() as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn k_strwork(iters: u32) -> f64 {
    repeat(iters, || strwork::run() as f64)
}

// -- prep() + run(&mut T): the input is consumed, so run.mjs calls prep()
//    before every timed call ------------------------------------------------

static MATMUL_S: Slot<(Vec<f64>, Vec<f64>, Vec<f64>, usize)> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_matmul_prep() {
    *MATMUL_S.get() = Some(matmul::setup());
}
#[unsafe(no_mangle)]
pub extern "C" fn k_matmul(iters: u32) -> f64 {
    let (a, b, c, n) = MATMUL_S.get().as_mut().unwrap();
    repeat(iters, || matmul::run(a, b, c, *n))
}

static NBODY_S: Slot<[nbody::Body; 5]> = Slot::new(); // nbody::N is private
#[unsafe(no_mangle)]
pub extern "C" fn k_nbody_prep() {
    *NBODY_S.get() = Some(nbody::setup());
}
#[unsafe(no_mangle)]
pub extern "C" fn k_nbody(iters: u32) -> f64 {
    let bodies = NBODY_S.get().as_mut().unwrap();
    repeat(iters, || nbody::run(bodies))
}

static SORTW_S: Slot<Vec<u64>> = Slot::new();
#[unsafe(no_mangle)]
pub extern "C" fn k_sortw_prep() {
    *SORTW_S.get() = Some(sortw::setup());
}
#[unsafe(no_mangle)]
pub extern "C" fn k_sortw(iters: u32) -> f64 {
    let v = SORTW_S.get().as_mut().unwrap();
    repeat(iters, || sortw::run(v) as f64)
}

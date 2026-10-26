// Built as an rlib with `-Clinker-plugin-lto` (what cargo passes to the
// dependencies of an `lto = true` build) under PLIRON_XFACTS_ALL. rustc
// skips the thin-local LTO step then, so modules held for the two-phase
// barrier reached `codegen` unfinished and every object was empty:
// "failed to build archive ... memory map must have a non-zero length"
// (prql, nushell in harness/crates.py). Several CGUs, one with no code.
pub mod a {
    pub fn f(x: u64) -> u64 {
        x.wrapping_mul(31) ^ (x >> 3)
    }
}
pub mod b {
    pub fn g(v: &[u64]) -> u64 {
        v.iter().map(|&x| crate::a::f(x)).sum()
    }
}
pub mod c {
    #[macro_export]
    macro_rules! twice {
        ($e:expr) => {
            ($e, $e)
        };
    }
}

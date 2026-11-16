//! The differential checker with 32-bit pointers (`set_ptr32`: wasm32).
//! Its own test binary, because the switch is process-wide.
//!
//! Before 2026-11-16 the sandbox only had addresses above `u32::MAX`, so
//! a wasm32 function that touched memory or took a frame address ended
//! every trial with `heap_oob` and came back inconclusive: the wasm-only
//! paths of the pipeline had no differential check at all.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check, set_ptr32};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn cfg() -> Config {
    Config { trials: 32, ..Config::default() }
}

/// `*p += x` through an i32 pointer, a frame slot round trip, and an
/// `uload32` (an i64 result behind a 32-bit address).
const BEFORE: &str = "function %f(i32, i32) -> i64 {
    ss0 = explicit_slot 8
block0(v0: i32, v1: i32):
    v2 = load.i32 notrap v0
    v3 = iadd v2, v1
    store notrap v3, v0
    v4 = stack_addr.i32 ss0
    store notrap v3, v4
    v5 = load.i32 notrap v4
    store notrap v5, v0+4
    v6 = uload32.i64 notrap v0+4
    return v6
}";

#[test]
fn i32_pointers_are_checked() {
    set_ptr32(true);
    // The same function: every trial runs to the end and compares.
    let a = parse(BEFORE);
    let s = check(&a, &a.clone(), &Facts::default(), &cfg()).unwrap();
    assert!(s.compared >= 24, "only {} of 32 trials compared", s.compared);

    // A store dropped: the bytes at `p + 4` differ.
    let b = parse(&BEFORE.replace("    store notrap v5, v0+4\n", ""));
    assert!(check(&a, &b, &Facts::default(), &cfg()).is_err(), "a dropped store went unnoticed");

    // A wrong value through the frame slot: the result differs.
    let c = parse(&BEFORE.replace("store notrap v3, v4", "store notrap v1, v4"));
    assert!(check(&a, &c, &Facts::default(), &cfg()).is_err(), "a wrong slot value went unnoticed");
}

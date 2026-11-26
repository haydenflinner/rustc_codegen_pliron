//! `frozen_loads` peeled a single `iadd` level to reach a frozen
//! (`noalias readonly`) param: `load (iadd x, k)`. Nested member access —
//! `x.data.heap.cap`, smallvec's `capacity()`, the decode num-bigint-dig's
//! `montgomery` recomputes every outer iteration — lowers as
//! `load (iadd (iadd x, 8), 8)` and a one-level peel missed it, so the
//! `*8>>3`/`min` chain stayed in-loop. Under `PLIRON_FPEEL=1` the peel follows
//! the `iadd` chain (bounded at 8 links) and the load goes `readonly
//! can_move`, letting Cranelift's egraph hoist it.

use cranelift_codegen::ir::Function;
use cranelift_reader::parse_functions;
use rustc_hash::FxHashMap;

fn parse(src: &str) -> Function {
    parse_functions(src).unwrap().remove(0)
}

/// `load (iadd (iadd x, 8), 8)` on a 48-byte frozen param: marks
/// `readonly can_move` under `PLIRON_FPEEL=1`.
#[test]
fn nested_iadd_peel_marks_frozen() {
    unsafe { std::env::set_var("PLIRON_FPEEL", "1") };
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 8
    v2 = iadd v0, v1
    v3 = iadd v2, v1
    v4 = load.i64 notrap v3
    return v4
}";
    let mut f = parse(src);
    let mut frozen = FxHashMap::default();
    // `v0` is a rustc `noalias readonly` param with 48 dereferenceable
    // bytes, so `v0+16` (the SmallVec `heap.cap` union field) qualifies.
    frozen.insert(cranelift_codegen::ir::Value::from_u32(0), 48);
    assert_eq!(pliron_clif_opt::clifpeep::frozen_loads(&mut f, &frozen), 1);
    let text = f.display().to_string();
    assert!(
        text.lines().any(|l| l.contains("readonly can_move")),
        "nested iadd+16 load did not freeze:\n{text}"
    );
}

/// Beyond the param's dereferenceable size the load stays unfrozen even
/// with the nested peel.
#[test]
fn nested_iadd_past_deref_stays_unfrozen() {
    unsafe { std::env::set_var("PLIRON_FPEEL", "1") };
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 24
    v2 = iadd v0, v1
    v3 = iadd v2, v1
    v4 = load.i64 notrap v3
    return v4
}";
    let mut f = parse(src);
    let mut frozen = FxHashMap::default();
    frozen.insert(cranelift_codegen::ir::Value::from_u32(0), 48);
    assert_eq!(
        pliron_clif_opt::clifpeep::frozen_loads(&mut f, &frozen),
        0,
        "load at +48 on a 48-byte frozen param must not freeze"
    );
}

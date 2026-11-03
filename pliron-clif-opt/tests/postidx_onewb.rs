//! aarch64 post-index sweep, one writeback per chain
//! (default, `PLIRON_POSTIDX_ONEWB=0` restores the chain; patch 100): a run of stores on one base
//! followed by `add base, base, #C` takes the bump on its last offset-0
//! access and re-addresses the accesses after it to `[base + (o - C)]`
//! (`stur`), instead of a telescoping writeback per access whose
//! addresses serialize through the base (brotli's 20-byte
//! `BrotliInitZopfliNodes` fill: `str [b], #4; str [b], #4; str [b],
//! #4; str [b], #8`).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    // Read once by the sweep's `OnceLock`; every test here wants it on.
    unsafe { std::env::set_var("PLIRON_POSTIDX_ONEWB", "1") };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    b.set("is_pic", "true").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

/// Fill `n` 20-byte records `{0, 1, 0, 1, 0}` through a bumped pointer.
const FILL: &str = "function %f(i64, i64) apple_aarch64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i32 0
    v3 = iconst.i32 1
    v10 = iconst.i64 20
    v11 = iconst.i64 -1
    jump block1(v0, v1)
block1(v4: i64, v5: i64):
    store notrap v2, v4
    store notrap v3, v4+4
    store notrap v2, v4+8
    store notrap v3, v4+12
    store notrap v2, v4+16
    v6 = iadd v4, v10
    v7 = iadd v5, v11
    brif v7, block1(v6, v7), block2
block2:
    return
}";

fn count(a: &str, f: impl Fn(&str) -> bool) -> usize {
    a.lines().filter(|l| f(l.trim_start())).count()
}

#[test]
fn record_fill_has_one_writeback_and_negative_offsets() {
    let a = asm(FILL);
    assert_eq!(count(&a, |l| l.contains("], #")), 1, "one post-index writeback:\n{a}");
    assert!(count(&a, |l| l.contains("], #20") || l.contains("], #0x14")) == 1, "writeback is the whole bump:\n{a}");
    assert_eq!(count(&a, |l| l.starts_with("stur ")), 4, "the later stores re-addressed below the bumped base:\n{a}");
    assert_eq!(count(&a, |l| l.starts_with("add ")), 0, "the pointer bump folded away:\n{a}");
}

/// A load and a store at the base before the bump (patch 90's shape):
/// the writeback goes on the store, the last offset-0 access, and the
/// load stays a plain `ldr [b]`.
const RMW: &str = "function %g(i64, i64) apple_aarch64 {
block0(v0: i64, v1: i64):
    v10 = iconst.i64 8
    v11 = iconst.i64 -1
    v12 = iconst.i64 1
    jump block1(v0, v1)
block1(v4: i64, v5: i64):
    v8 = load.i64 notrap v4
    v9 = iadd v8, v12
    store notrap v9, v4
    v6 = iadd v4, v10
    v7 = iadd v5, v11
    brif v7, block1(v6, v7), block2
block2:
    return
}";

#[test]
fn load_store_at_base_keeps_the_writeback_on_the_store() {
    let a = asm(RMW);
    assert_eq!(count(&a, |l| l.contains("], #")), 1, "one writeback:\n{a}");
    assert!(count(&a, |l| l.starts_with("str ") && l.contains("], #8")) == 1, "writeback on the store:\n{a}");
    assert_eq!(count(&a, |l| l.starts_with("stur ") || l.starts_with("ldur ")), 0, "no re-addressing needed:\n{a}");
    assert_eq!(count(&a, |l| l.starts_with("add ") && l.contains("#8")), 0, "the bump folded away:\n{a}");
}

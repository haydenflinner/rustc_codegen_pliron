//! Shrink-wrapping (`PLIRON_SHRINKWRAP=1`, aarch64): the callee saves and
//! frame allocation run only on the path that needs them. An early return
//! before any call keeps just the frame record; a value live across the
//! slow path's calls is split at the save block so the entry block does
//! not hold it in a callee-saved register; a return block shared by both
//! paths is duplicated.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    // The knob is read once per process; this test binary sets it first.
    unsafe { std::env::set_var("PLIRON_SHRINKWRAP", "1") };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    b.set("is_pic", "true").unwrap();
    b.set("preserve_frame_pointers", "true").unwrap();
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

/// `skip_to_escape`'s shape: two loads, an early return when they are
/// equal, otherwise calls with both values live across them; the return
/// block is shared by both paths.
const EARLY: &str = "function %f(i64, i64) -> i64 apple_aarch64 {
    sig0 = (i64) -> i64 apple_aarch64
    fn0 = %g sig0
block0(v0: i64, v1: i64):
    v2 = load.i64 notrap v0
    v3 = load.i64 notrap v0+8
    v4 = icmp eq v2, v3
    brif v4, block2(v1), block1
block1:
    v5 = call fn0(v2)
    v6 = call fn0(v5)
    v7 = iadd v6, v3
    v8 = iadd v7, v1
    jump block2(v8)
block2(v9: i64):
    return v9
}";

#[test]
fn early_return_skips_callee_saves() {
    let a = asm(EARLY);
    let lines: Vec<&str> = a.lines().map(str::trim).filter(|l| !l.starts_with("unwind")).collect();
    // Everything up to the first branch: frame record, the two loads, the
    // compare. No callee-saved register is saved or used there.
    let head: Vec<&&str> = lines.iter().take_while(|l| !l.starts_with('b')).collect();
    for l in &head {
        assert!(
            !["x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28"].iter().any(|r| l.contains(r)),
            "callee-saved register before the early branch: {l}\n{a}"
        );
    }
    assert!(a.contains("stp x19") || a.contains("str x19") || a.contains("stp x2"), "no callee saves at all?\n{a}");
    // Some return runs only the frame-record pop.
    let frame_only = lines.windows(2).any(|w| w[0].starts_with("ldp fp, lr") && w[1] == "ret")
        && lines.iter().filter(|l| **l == "ret").count() >= 2;
    assert!(frame_only, "no frame-only return:\n{a}");
}

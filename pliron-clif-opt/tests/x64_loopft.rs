//! x64 loop-side fallthrough placement (`PLIRON_LOOPFT=1`, opt-in).
//!
//! When a brif's two arms are a leaf/return block and a loop entry,
//! the loop arm should be the fallthrough: keeping loop work on the
//! straight-line fetch path avoids a taken front-end redirect per
//! non-leaf call (recursive `fib` measured ~1.4x slower with the
//! leaf inline, even though the leaf is the *more* frequent arm).
//! Exercises both mechanisms: `BCHAIN`'s raw chain order
//! (`PLIRON_PLACE=0`) and the default `PLACE` chain builder.

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str, loopft: &str, place: &str) -> String {
    unsafe {
        std::env::set_var("PLIRON_LOOPFT", loopft);
        std::env::set_var("PLIRON_PLACE", place);
    };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("x86_64")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

/// fib's shape: entry `brif n<2, leaf, loop-entry`; the leaf arm is a
/// two-inst return path, the loop arm contains the recursive call and
/// the back-edge.
const FIB: &str = "
function %f(i32) -> i64 windows_fastcall {
    sig0 = (i32) -> i64 windows_fastcall
    fn0 = colocated %f sig0
block0(v0: i32):
    v3 = iconst.i32 2
    v20 = icmp ult v0, v3
    brif v20, block5, block6
block6:
    v16 = iconst.i64 0
    jump block1(v0, v16)
block5:
    v33 = uextend.i64 v0
    jump block2(v33)
block1(v27: i32, v28: i64):
    v39 = iconst.i32 -1
    v40 = iadd v27, v39
    v7 = call fn0(v40)
    v41 = iconst.i32 -2
    v42 = iadd v27, v41
    v43 = iconst.i32 2
    v44 = icmp uge v42, v43
    v18 = iadd v28, v7
    brif v44, block1(v42, v18), block7
block7:
    v36 = uextend.i64 v42
    v37 = iadd v36, v18
    jump block2(v37)
block2(v38: i64):
    return v38
}";

/// Position of the leaf's `movl %ecx, %eax` (the `uextend`) versus
/// the loop's self-call in the emitted text. LOOPFT=1 puts the call
/// first; LOOPFT=0 puts the leaf first.
fn check(on: &str, off: &str) {
    let pos = |hay: &str, n: &str| hay.find(n).unwrap_or(usize::MAX);
    let call_on = pos(on, "call");
    let leaf_on = pos(on, "movl %ecx, %eax");
    assert!(
        call_on < leaf_on,
        "loop arm not the fallthrough with LOOPFT=1:\n{on}"
    );
    let call_off = pos(off, "call");
    let leaf_off = pos(off, "movl %ecx, %eax");
    assert!(
        leaf_off < call_off,
        "leaf arm unexpectedly deferred with LOOPFT=0:\n{off}"
    );
}

#[test]
fn loop_arm_fallthrough_bchain() {
    check(&asm(FIB, "1", "0"), &asm(FIB, "0", "0"));
}

#[test]
fn loop_arm_fallthrough_place() {
    check(&asm(FIB, "1", "1"), &asm(FIB, "0", "1"));
}

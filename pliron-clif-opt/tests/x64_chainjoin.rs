//! x64 block-order diamond placement (`PLIRON_CHAINJOIN=1`, opt-in).
//!
//! When a brif's two successors form a diamond — the join's other
//! predecessor is the sibling arm — `BCHAIN`'s fallthrough pick should
//! be the *arm*, not the join. Choosing the join (it has more
//! predecessors, and inside a loop it also dominates the latch, so it
//! scores well by default) defers the arm to the far end of the
//! function: every trip through it then pays two taken branches (the
//! `jcc` out and the `jmp` back). Placing the arm between the test and
//! the join makes both edges fallthrough — `iter().filter`'s
//! keep-block in `iterw` went from `ja`-out+`jmp`-back per kept
//! element to LLVM's inline shape (~1.4x -> ~1.0x).

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str, cj: &str) -> String {
    unsafe {
        std::env::set_var("PLIRON_CHAINJOIN", cj);
        // BCHAIN's raw order is the mechanism under test; PLACE would
        // already repair this particular CFG.
        std::env::set_var("PLIRON_PLACE", "0");
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

/// Two filter diamonds in a loop (`brif c, arm, join`, arm jumps to
/// join). The first join is mid-loop: it dominates the latch, so the
/// default heuristic prefers it (rank 2, plus more preds); the second
/// join *is* the latch and stays preferred either way.
const DIAMONDS: &str = "
function %f(i32, i64) -> i64 system_v {
block0(v0: i32, v1: i64):
    v100 = iconst.i64 0
    jump block1(v0, v100)
block1(v10: i32, v11: i64):
    v19 = iconst.i32 5
    v20 = icmp ne v10, v19
    brif.i8 v20, block2, block3(v11)
block2:
    v21 = sextend.i64 v10
    v22 = imul v21, v21
    v23 = iadd v11, v22
    jump block3(v23)
block3(v12: i64):
    v30 = iconst.i32 7
    v31 = icmp ne v10, v30
    brif.i8 v31, block4, block5(v12)
block4:
    v32 = sextend.i64 v10
    v33 = iadd v12, v32
    jump block5(v33)
block5(v13: i64):
    v40 = iconst.i32 1
    v41 = isub v10, v40
    v42 = iconst.i32 2
    v43 = icmp uge v41, v42
    brif v43, block1(v41, v13), block6(v13)
block6(v14: i64):
    return v14
}";

#[test]
fn diamond_arm_before_join() {
    let off = asm(DIAMONDS, "0");
    let on = asm(DIAMONDS, "1");
    // ON: the square (`imulq`) is emitted between the `cmpl $0x5` test
    // and the `cmpl $0x7` of the next diamond — the arm is inline.
    let pos = |hay: &str, n: &str| hay.find(n).unwrap_or(usize::MAX);
    let t1 = pos(&on, "cmpl $0x5");
    let sq = pos(&on, "imulq");
    let t2 = pos(&on, "cmpl $0x7");
    assert!(t1 < sq && sq < t2, "arm not inline:\n{on}");
    // OFF: the arm is deferred past `retq`.
    let ret = pos(&off, "retq");
    let sq_off = pos(&off, "imulq");
    assert!(ret < sq_off, "arm unexpectedly inline:\n{off}");
}

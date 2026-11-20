//! Vendored Cranelift's `bump_hoist` with `PLIRON_BUMPHOIST_INLOOP` (default
//! on, `=0` off): a loop cursor is fused into a post-indexed access unless
//! the loop reads it again in more every-trip blocks than an emptied latch
//! saves (each such block would pay a `sub` to rebuild the old value).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
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

/// pulldown-cmark's byte scan: the cursor's one other read is the end test,
/// and the latch is only the bump. Fused, the end test pays one `sub` and the
/// latch disappears (the back edge is the conditional branch itself).
#[test]
fn single_reader_with_empty_latch_fuses() {
    let a = asm("
function %f(i64, i64) -> i32 apple_aarch64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i32 0
    jump block1(v0, v2)
block1(v3: i64, v4: i32):
    v5 = uload8.i32 notrap v3
    v6 = iadd v4, v5
    brif v5, block2, block3(v6)
block2:
    v8 = icmp eq v3, v1
    brif v8, block3(v6), block4
block4:
    v30 = iconst.i64 1
    v9 = iadd v3, v30
    jump block1(v9, v6)
block3(v10: i32):
    return v10
}");
    assert!(a.contains("], #1"), "cursor not post-indexed:\n{a}");
}

/// brotli's `nodes[i + 1]` pointer IV: read in two blocks every trip runs,
/// the latch has work of its own. Fused, both blocks would rebuild the old
/// pointer with a `sub`; the bump stays an `add`.
#[test]
fn two_readers_stay_unfused() {
    let a = asm("
function %f(i64, i64) -> i32 apple_aarch64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i32 0
    jump block1(v0, v2)
block1(v3: i64, v4: i32):
    v5 = load.i32 notrap v3
    brif v5, block5, block2(v4)
block5:
    v20 = iadd v4, v5
    jump block2(v20)
block2(v21: i32):
    v6 = load.i32 notrap v3+8
    v7 = iadd v21, v6
    v31 = iconst.i32 100000
    v8 = icmp ult v7, v31
    brif v8, block6, block3(v7)
block6:
    v9 = load.i32 notrap v3+12
    store notrap v7, v3+16
    v10 = iadd v7, v9
    v32 = iconst.i64 20
    v11 = iadd v3, v32
    v12 = icmp ne v11, v1
    brif v12, block1(v11, v10), block3(v10)
block3(v13: i32):
    return v13
}");
    assert!(!a.contains("], #20"), "pointer fused with every-trip readers:\n{a}");
    assert!(!a.lines().any(|l| l.trim_start().starts_with("sub ") && l.trim_end().ends_with("#20")), "remat sub:\n{a}");
}

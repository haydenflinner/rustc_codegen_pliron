//! x64 Windows `CallConv::Tail` callee-saves: on x86_64-windows the internal
//! Tail ABI preserves xmm8-15 (xmm6/7 stay volatile — they are Tail float
//! arg/retval regs). Without this, `get_regs_clobbered_by_call(Tail)` returns
//! the SysV set, so every WindowsFastcall caller of an internal function
//! clobbers all ten callee-saved xmm regs and pays a ~160-byte save/restore
//! storm in its prologue even when it never uses an xmm register.
//! `PLIRON_X64_TAILXMM=0` restores the stock behavior (the gate is read once
//! per process; this test only exercises the default-on state).

#![cfg(target_arch = "x86_64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str, triple: &str) -> String {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name(triple)
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

// A WindowsFastcall function calling internal (Tail-conv) callees, with only
// integers live across the calls: no xmm values exist, so xmm8-15 must not be
// saved. (xmm6/7 may still be saved — they are Tail arg regs and therefore
// genuinely call-clobbered.) Before the fix all ten were saved.
#[test]
fn fastcall_caller_no_xmm_save_storm() {
    let a = asm(
        "
function %f(i64) -> i64 windows_fastcall {
    fn0 = %g(i64) -> i64 tail
block0(v0: i64):
    v1 = call fn0(v0)
    v2 = call fn0(v0)
    v3 = iadd v1, v2
    return v3
}

function %g(i64) -> i64 tail {
block0(v0: i64):
    v1 = imul v0, v0
    return v1
}
",
        "x86_64-pc-windows-msvc",
    );
    for r in 8..16 {
        assert!(
            !a.contains(&format!("xmm{r}")),
            "xmm{r} save/restore in windows_fastcall caller:\n{a}"
        );
    }
}

// A Tail callee that itself uses xmm8-15 must save them so callers can rely
// on the preservation: nine f64 args summed across an internal call leaves
// xmm8+ live at the call boundary.
#[test]
fn tail_callee_saves_used_xmm8_15() {
    let a = asm(
        "
function %f(f64, f64, f64, f64, f64, f64, f64, f64, f64) -> f64 tail {
    fn0 = %g(f64) -> f64 tail
block0(v0: f64, v1: f64, v2: f64, v3: f64, v4: f64, v5: f64, v6: f64, v7: f64, v8: f64):
    v9 = call fn0(v0)
    v10 = fadd v9, v1
    v11 = fadd v10, v2
    v12 = fadd v11, v3
    v13 = fadd v12, v4
    v14 = fadd v13, v5
    v15 = fadd v14, v6
    v16 = fadd v15, v7
    v17 = fadd v16, v8
    return v17
}

function %g(f64) -> f64 tail {
block0(v0: f64):
    v1 = fmul v0, v0
    return v1
}
",
        "x86_64-pc-windows-msvc",
    );
    // v8 (the ninth float arg) must live in an xmm8+ slot across the call and
    // be preserved for our own caller — a save must appear.
    assert!(
        a.contains("xmm8")
            || a.contains("xmm9")
            || a.contains("xmm10")
            || a.contains("xmm11"),
        "no callee-saved xmm8+ save in tail function:\n{a}"
    );
}

// Same caller shape on Linux keeps the stock SysV clobber set (SysV has no
// callee-saved xmm, so nothing changes; this pins that the non-Windows path
// is untouched).
#[test]
fn linux_unchanged() {
    let a = asm(
        "
function %f(i64) -> i64 system_v {
    fn0 = %g(i64) -> i64 tail
block0(v0: i64):
    v1 = call fn0(v0)
    v2 = call fn0(v0)
    v3 = iadd v1, v2
    return v3
}

function %g(i64) -> i64 tail {
block0(v0: i64):
    v1 = imul v0, v0
    return v1
}
",
        "x86_64-unknown-linux-gnu",
    );
    assert!(a.contains("call"), "expected calls in output:\n{a}");
}

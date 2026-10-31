//! aarch64 codegen of a table-scan loop (pulldown-cmark's `parse_block`):
//! - the exit path's constant branch arguments are materialized on the
//!   exit edge, not in the latch every iteration (`PLIRON_EXITCONST`);
//! - the byte load folds its single-use `base + index` into a
//!   register-offset address although the loaded value has two uses
//!   (`PLIRON_AMODESHARE_DIRECT`).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    // Without the compile-path egraph the constants stay in the latch as
    // written (the egraph would hoist these to the entry), which is the
    // shape the pliron pipeline hands over in `parse_block`. Read once per
    // process; this test binary sets it first.
    unsafe { std::env::set_var("PLIRON_COMPILE_EGRAPH", "0") };
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

/// Scan `s[i..n]` until `table[s[i]] != 0`; on running off the end return
/// `(255, 0)`-style constants through the exit block's params.
const SCAN: &str = "function %f(i64, i64, i64, i64) -> i64, i64 apple_aarch64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    jump block1(v3)
block1(v4: i64):
    v5 = iadd v0, v4
    v6 = uload8.i64 notrap v5
    v7 = iadd v2, v6
    v8 = load.i8 notrap v7
    brif v8, block3(v4, v6), block2
block2:
    v9 = iconst.i64 1
    v10 = iadd v4, v9
    v11 = icmp ult v10, v1
    v12 = iconst.i64 255
    v13 = iconst.i64 0
    brif v11, block1(v10), block3(v12, v13)
block3(v14: i64, v15: i64):
    return v14, v15
}";

/// Lines of the vcode blocks that lower CLIF `block` (`blockN: ; block2`).
fn lowered<'a>(a: &'a str, block: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut on = false;
    for l in a.lines() {
        let t = l.trim();
        if t.starts_with("block") && t.contains(':') {
            on = t.split("; ").nth(1).is_some_and(|c| c.trim() == block);
            continue;
        }
        if on {
            out.push(t);
        }
    }
    out
}

#[test]
fn exit_constants_not_in_latch() {
    let a = asm(SCAN);
    let latch = lowered(&a, "block2");
    assert!(!latch.is_empty(), "no latch found:\n{a}");
    assert!(
        !latch.iter().any(|l| l.contains("#255")),
        "exit constant materialized in the latch:\n{a}"
    );
}

#[test]
fn byte_load_uses_register_offset() {
    let a = asm(SCAN);
    let body = lowered(&a, "block1");
    // `ldrb wN, [xA, xB]`: register + register, no separate `add`.
    let folded = |l: &&str| {
        l.starts_with("ldrb") && l.split_once('[').is_some_and(|(_, m)| {
            let m = m.trim_end_matches(']');
            let parts: Vec<&str> = m.split(", ").collect();
            parts.len() == 2 && parts.iter().all(|p| p.starts_with('x'))
        })
    };
    assert!(body.iter().take(3).any(folded), "byte load not folded into [base, index]:\n{a}");
}

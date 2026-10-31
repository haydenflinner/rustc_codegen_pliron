//! aarch64 register allocation around calls:
//! - `PLIRON_ARGCOPY`: a value that is both a call argument and used in a
//!   loop is copied into a fresh vreg for the call, so the ABI's
//!   fixed-register use does not pin the value's whole bundle to that
//!   (call-clobbered) register; the loop then keeps it in a register.
//! - `PLIRON_REMAT`: a spilled symbol address is re-materialized at its
//!   reload (`load_ext_name_*`) instead of loaded from the spill slot.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

/// A vcode block label: `block3:`, optionally followed by `; <clif block>`.
fn is_label(l: &str) -> bool {
    let t = l.trim();
    t.starts_with("block") && t.split(';').next().is_some_and(|h| h.trim_end().ends_with(':'))
}

fn asm(clif: &str) -> String {
    asm_at(clif, "speed")
}

fn asm_at(clif: &str, opt: &str) -> String {
    let mut b = settings::builder();
    b.set("opt_level", opt).unwrap();
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

/// The real case: serde_json's `serialize_str` after the pipeline. Its
/// escape loop (the block with two byte loads and a `cbnz`) must not touch
/// the stack: without ARGCOPY the string base is stored and reloaded every
/// byte (it is also `memcpy`'s x1); without REMAT the escape table's
/// address is reloaded from its spill slot every byte.
#[test]
fn serde_escape_loop_has_no_stack_traffic() {
    let a = asm(include_str!("data/serde_serialize_str.clif"));
    let lines: Vec<&str> = a.lines().collect();
    let mut found = false;
    for (i, l) in lines.iter().enumerate() {
        if !is_label(l) {
            continue;
        }
        let body: Vec<&str> = lines[i + 1..].iter().take_while(|x| !is_label(x)).copied().collect();
        let loads = body.iter().filter(|x| x.contains("ldurb") || x.contains("ldrb")).count();
        if loads == 2 && body.iter().any(|x| x.contains("cbnz")) && body.len() <= 10 {
            found = true;
            assert!(!body.iter().any(|x| x.contains("[sp")), "stack traffic in the escape loop:\n{}", body.join("\n"));
        }
    }
    assert!(found, "escape loop not found:\n{a}");
}

/// More constants than callee-saved registers live across a call in a
/// loop: some spill, every reload of them is rematerialized, so their spill
/// slots are never read and the stores into them are dead
/// (`PLIRON_REMAT_STORE`).
const CONSTS_ACROSS_CALL: &str = "
function %f(i64, i64) -> i64 apple_aarch64 {
    sig0 = (i64) -> i64 apple_aarch64
    fn0 = %g sig0
block0(v0: i64, v1: i64):
    v10 = iconst.i64 63
    v11 = iconst.i64 65535
    v12 = iconst.i64 1023
    v13 = iconst.i64 4095
    v14 = iconst.i64 127
    v15 = iconst.i64 2047
    v16 = iconst.i64 16777215
    v17 = iconst.i64 1073741823
    v18 = iconst.i64 55296
    v19 = iconst.i64 57344
    v20 = iconst.i64 262143
    v21 = iconst.i64 55295
    v22 = iconst.i64 4096
    v23 = iconst.i64 1024
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v5 = call fn0(v3)
    v40 = imul v5, v10
    v60 = iadd v40, v10
    v41 = imul v60, v11
    v61 = iadd v41, v11
    v42 = imul v61, v12
    v62 = iadd v42, v12
    v43 = imul v62, v13
    v63 = iadd v43, v13
    v44 = imul v63, v14
    v64 = iadd v44, v14
    v45 = imul v64, v15
    v65 = iadd v45, v15
    v46 = imul v65, v16
    v66 = iadd v46, v16
    v47 = imul v66, v17
    v67 = iadd v47, v17
    v48 = imul v67, v18
    v68 = iadd v48, v18
    v49 = imul v68, v19
    v69 = iadd v49, v19
    v50 = imul v69, v20
    v70 = iadd v50, v20
    v51 = imul v70, v21
    v71 = iadd v51, v21
    v52 = imul v71, v22
    v72 = iadd v52, v22
    v53 = imul v72, v23
    v73 = iadd v53, v23
    v6 = iadd v4, v73
    v7 = iconst.i64 1
    v8 = iadd v3, v7
    v9 = icmp ult v8, v1
    brif v9, block1(v8, v6), block2(v6)
block2(v100: i64):
    return v100
}";

#[test]
fn no_stores_into_remat_only_slots() {
    // `none`: no egraph, which would re-place the constants in the loop;
    // pliron's licm hoists them like this.
    let a = asm_at(CONSTS_ACROSS_CALL, "none");
    let stores = a
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            (t.starts_with("str ") || t.starts_with("stur ")) && t.contains("[sp") && !t.ends_with("]!")
        })
        .count();
    // Only the loop bound (a parameter, not rematerializable) spills: its
    // store in the entry and on the back edge. With the constants' stores
    // kept (`PLIRON_REMAT_STORE=0`) there are 14.
    assert!(stores <= 2, "{stores} spill stores; constants' stores are dead:\n{a}");
}

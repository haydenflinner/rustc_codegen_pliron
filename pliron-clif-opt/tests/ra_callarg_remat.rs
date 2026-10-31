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

fn asm(clif: &str) -> String {
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
        if !l.trim_end().ends_with(':') {
            continue;
        }
        let body: Vec<&str> = lines[i + 1..].iter().take_while(|x| !x.trim_end().ends_with(':')).copied().collect();
        let loads = body.iter().filter(|x| x.contains("ldurb") || x.contains("ldrb")).count();
        if loads == 2 && body.iter().any(|x| x.contains("cbnz")) && body.len() <= 10 {
            found = true;
            assert!(!body.iter().any(|x| x.contains("[sp")), "stack traffic in the escape loop:\n{}", body.join("\n"));
        }
    }
    assert!(found, "escape loop not found:\n{a}");
}

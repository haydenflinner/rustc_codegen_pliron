//! aarch64 reload PRE (`reload_pre` in vcode.rs, `PLIRON_RELOADPRE`):
//! regalloc2 reloads a spilled value right before each use. serde_json's
//! escape table address is spilled across the escape path's calls, so the
//! byte loop rebuilt it (`load_ext_name` = `adrp` + `add`) every byte
//! although its register survives the loop's hot path. The reload moves to
//! the loop's other in-edges; the allocation checker vets the rewrite.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn is_label(l: &str) -> bool {
    let t = l.trim();
    t.starts_with("block") && t.split(';').next().is_some_and(|h| h.trim_end().ends_with(':'))
}

fn asm(clif: &str) -> String {
    // SAFETY: tests in this binary only read the variable.
    unsafe { std::env::set_var("PLIRON_RELOADPRE_CHECK", "1") };
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

#[test]
fn serde_escape_loop_keeps_table_address() {
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
            assert!(
                !body.iter().any(|x| x.contains("load_ext_name") || x.contains("[sp")),
                "escape table address rebuilt or reloaded in the loop:\n{}",
                body.join("\n")
            );
        }
    }
    assert!(found, "escape loop not found:\n{a}");
}

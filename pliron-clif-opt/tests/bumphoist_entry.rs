//! Vendored `bump_hoist` (post-index fusion for a loop's stepped cursor)
//! takes only parameters of blocks inside loops (`PLIRON_BUMPHOIST_LOOPONLY`,
//! default on). An entry parameter used as a load base qualified too:
//! lol_html's `handle_end_tag_hint` kept `self + 481` across a call and
//! rebuilt `self` as `x21 - 481` before each later use.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_BUMPHOIST_LOOPONLY", "1") };
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

/// lol_html's `handle_end_tag_hint` after our pipeline (`self` is `v1`,
/// `v13 = self + 481` is kept for a later store).
#[test]
fn entry_base_is_not_rebuilt_from_its_bump() {
    let a = asm(include_str!("data/bumphoist_entry.clif"));
    let subs = a.lines().filter(|l| l.contains("#481") || l.contains("#0x1e1")).count();
    assert_eq!(subs, 1, "only the bump itself: {a}");
}

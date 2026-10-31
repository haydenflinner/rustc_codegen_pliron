//! aarch64: a `symbol_value` defined before a loop stays there. On x86-64
//! the post-egraph `remat_sink` clones it into the loop (one RIP-relative
//! `lea`); on aarch64 the clone is `adrp` + `add` per iteration, which
//! LLVM never pays (pulldown-cmark's escape-table scan).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

#[test]
fn table_address_stays_out_of_loop() {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(
        "
function %f(i64, i64) -> i64 apple_aarch64 {
    gv0 = symbol colocated %tab
block0(v0: i64, v1: i64):
    v2 = symbol_value.i64 gv0
    jump block1(v0)
block1(v3: i64):
    v4 = uload8.i64 notrap v3
    v5 = iadd v2, v4
    v6 = load.i8 notrap v5
    v7 = iconst.i8 0
    v8 = icmp ne v6, v7
    brif v8, block2(v3), block3
block3:
    v9 = iconst.i64 1
    v10 = iadd v3, v9
    v11 = icmp ne v10, v1
    brif v11, block1(v10), block2(v10)
block2(v12: i64):
    return v12
}",
    )
    .unwrap()
    .remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    let a = ctx.compiled_code().unwrap().vcode.clone().unwrap();
    let pre = a.split("block1:").next().unwrap();
    assert!(pre.contains("load_ext_name"), "address not computed before the loop:\n{a}");
    assert_eq!(a.matches("load_ext_name").count(), 1, "{a}");
}

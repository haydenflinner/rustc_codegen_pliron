//! `PLIRON_LUCARRY=1` (vendored Cranelift, `Context::lastuse_schedule`):
//! last-use scheduling does not compute a block parameter's next value
//! ahead of an op that still reads the parameter. regex's varint writer
//! (`data.push((n as u8) | 0x80); n >>= 7`) with the byte computed wide:
//! the `lsr` was moved above the `orr` (to make the `orr` the last use of
//! the shared constant), `n` and `n >> 7` overlapped, and the loop paid a
//! `mov` on its back edge. Its own test binary: the knob is read once per
//! process. Run with `PLIRON_LUCARRY=0` to see it fail.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

#[test]
fn varint_loop_needs_no_back_edge_copy() {
    if std::env::var_os("PLIRON_LUCARRY").is_none() {
        unsafe { std::env::set_var("PLIRON_LUCARRY", "1") };
    }
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(
        "function %f(i64, i64, i32) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64, v2: i32):
    jump block1(v2, v1)

block1(v3: i32, v4: i64):
    v5 = iconst.i32 128
    v6 = bor v3, v5
    v7 = iadd v0, v4
    istore8 notrap v6, v7
    v8 = iconst.i32 7
    v9 = ushr v3, v8
    v10 = icmp uge v9, v5
    v11 = iconst.i64 1
    v12 = iadd v4, v11
    brif v10, block1(v9, v12), block2(v9, v12)

block2(v13: i32, v14: i64):
    v15 = uextend.i64 v13
    v16 = iadd v15, v14
    return v16
}",
    )
    .unwrap()
    .remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    let asm = ctx.compiled_code().unwrap().vcode.clone().unwrap();
    // The loop body: from `block1:` up to the next block label.
    let body: Vec<&str> = asm
        .lines()
        .skip_while(|l| !l.starts_with("block1:"))
        .skip(1)
        .take_while(|l| !l.starts_with("block"))
        .collect();
    assert!(!body.is_empty(), "{asm}");
    assert!(!body.iter().any(|l| l.trim_start().starts_with("mov ")), "{asm}");
}

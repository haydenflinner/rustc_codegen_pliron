//! aarch64 `fvdemote_f16` to a 64-bit `f16x4` leaf (CubeCL's 4-lane f16
//! lines under `types::NATIVE_F16`): one `fcvtn v.4h, v.4s`. Without the
//! vendored ISLE rule Cranelift rejects the instruction ("should be
//! implemented in ISLE"), which made every `f16 x 4` CubeCL kernel panic
//! on the pliron->Cranelift JIT on Apple Silicon.

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
    ctx.compile(&*isa, &mut ControlPlane::default())
        .unwrap_or_else(|e| panic!("cranelift rejected f16x4 demote: {e:?}"));
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

#[test]
fn demote_f32x4_to_f16x4() {
    let a = asm("
function %f(f32x4) -> f16x4 apple_aarch64 {
block0(v0: f32x4):
    v1 = fvdemote_f16.f16x4 v0
    return v1
}");
    assert!(a.contains("fcvtn"), "expected fcvtn in:\n{a}");
}

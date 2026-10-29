//! aarch64 scalar `fpromote.f32` of an f16 / `fdemote.f16` of an f32 must
//! encode as `fcvt s,h` / `fcvt h,s`. The vendored `FPUOp1::Cvt16To32` /
//! `Cvt32To16` once had bits 20:15 = 000000 / 000011, which are `fmov h,h`
//! and `fsqrt s,s`: every f16<->f32 scalar conversion silently produced
//! garbage (CubeCL's f16 kernels on the pliron->Cranelift JIT, 70+ failing
//! tests). The vcode text printed `fcvt` either way, so check the bytes.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn words(clif: &str) -> Vec<u32> {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code()
        .unwrap()
        .buffer
        .data()
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[test]
fn scalar_f16_f32_converts_encode_as_fcvt() {
    let w = words(
        "
function %f(f16) -> f16 apple_aarch64 {
block0(v0: f16):
    v1 = fpromote.f32 v0
    v2 = fadd v1, v1
    v3 = fdemote.f16 v2
    return v3
}",
    );
    // Rd/Rn live in bits 9:0.
    let fcvt_s_h = w.iter().any(|x| x & 0xFFFF_FC00 == 0x1EE2_4000);
    let fcvt_h_s = w.iter().any(|x| x & 0xFFFF_FC00 == 0x1E23_C000);
    assert!(fcvt_s_h, "no `fcvt s,h` in {w:08x?}");
    assert!(fcvt_h_s, "no `fcvt h,s` in {w:08x?}");
}

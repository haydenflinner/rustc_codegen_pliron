//! aarch64 packed f16 demotes: `fvdemote_f16` to a 64-bit `f16x4` leaf
//! (CubeCL's 4-lane f16 lines under `types::NATIVE_F16`) is one
//! `fcvtn v.4h, v.4s`, and `fvdemote_f16_pair` to `f16x8` is
//! `fcvtn` + `fcvtn2`. Without the vendored ISLE rule Cranelift rejects the
//! f16x4 form ("should be implemented in ISLE"); the pair rule once passed
//! the *source* lane size to `fcvtn`, which encoded `fcvtn v.2s, v.2d` (f64
//! -> f32) and gave garbage. The vcode text hides that, so check the
//! emitted words.

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
    ctx.compile(&*isa, &mut ControlPlane::default())
        .unwrap_or_else(|e| panic!("cranelift rejected the f16 demote: {e:?}"));
    ctx.compiled_code()
        .unwrap()
        .buffer
        .data()
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// `fcvtn v.4h, v.4s` (Q=0) / `fcvtn2 v.8h, v.4s` (Q=1); Rd/Rn in bits 9:0.
const FCVTN_4H: u32 = 0x0E21_6800;
const FCVTN2_8H: u32 = 0x4E21_6800;

#[test]
fn demote_f32x4_to_f16x4() {
    let w = words(
        "
function %f(f32x4) -> f16x4 apple_aarch64 {
block0(v0: f32x4):
    v1 = fvdemote_f16.f16x4 v0
    return v1
}",
    );
    assert!(w.iter().any(|x| x & 0xFFFF_FC00 == FCVTN_4H), "no fcvtn v.4h in {w:08x?}");
}

#[test]
fn demote_pair_f32x4_to_f16x8() {
    let w = words(
        "
function %f(f32x4, f32x4) -> f16x8 apple_aarch64 {
block0(v0: f32x4, v1: f32x4):
    v2 = fvdemote_f16_pair.f32x4 v0, v1
    return v2
}",
    );
    assert!(w.iter().any(|x| x & 0xFFFF_FC00 == FCVTN_4H), "no fcvtn v.4h in {w:08x?}");
    assert!(w.iter().any(|x| x & 0xFFFF_FC00 == FCVTN2_8H), "no fcvtn2 v.8h in {w:08x?}");
}

//! Wide-vector (256/512-bit) boolean lowering tests: compile small CLIF
//! functions on x64 with `WIDE_VECTOR_BITS` set and check the emitted bytes
//! contain VEX-256/EVEX-512 encodings of the boolean ops — and no xmm-width
//! `pand`/`por`/`pxor` that would silently compute only the low 128 bits.

use core::sync::atomic::Ordering;

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::x64::WIDE_VECTOR_BITS;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::isa::TargetIsa;

fn isa() -> std::sync::Arc<dyn TargetIsa> {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(b);
    let mut isa_b = cranelift_codegen::isa::lookup_by_name("x86_64").unwrap();
    isa_b.enable("has_avx").unwrap();
    isa_b.enable("has_avx2").unwrap();
    isa_b.enable("has_fma").unwrap();
    isa_b.enable("has_avx512f").unwrap();
    isa_b.finish(flags).unwrap()
}

fn parse(clif: &str) -> Function {
    cranelift_reader::parse_functions(clif)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

fn compile(bits: u32, clif: &str) -> Vec<u8> {
    let isa = isa();
    WIDE_VECTOR_BITS.store(bits, Ordering::Relaxed);
    let mut ctx = Context::for_function(parse(clif));
    ctx.compile(&*isa, &mut ControlPlane::default())
        .unwrap();
    WIDE_VECTOR_BITS.store(128, Ordering::Relaxed);
    ctx.compiled_code().unwrap().code_buffer().to_vec()
}

/// Does `code` contain a VEX.256 (`C5`/`C4`) or EVEX.512 (`62`) encoding of
/// one of `ops`? VEX.256: L=1 in vvvvL byte. EVEX: L'L = 10 in P2[6:5].
fn has_wide_op(code: &[u8], ops: &[u8]) -> bool {
    code.windows(3).any(|w| {
        w[0] == 0xC5 && (w[1] & 0x04) != 0 && ops.contains(&w[2])
    }) || code.windows(4).any(|w| {
        w[0] == 0xC4 && (w[2] & 0x04) != 0 && ops.contains(&w[3])
    }) || code.windows(5).any(|w| {
        w[0] == 0x62 && (w[3] & 0x60) == 0x40 && ops.contains(&w[4])
    })
}

/// Is there a legacy (non-VEX) encoding of `ops` (`66 0F xx`)? That would mean
/// an xmm-width instruction computed only the low half of a wide vector.
fn has_legacy_op(code: &[u8], ops: &[u8]) -> bool {
    code.windows(3)
        .any(|w| w[0] == 0x66 && w[1] == 0x0F && ops.contains(&w[2]))
}

const BITSELECT: &str = "
function %bs(i32x8, i32x8, i32x8) -> i32x8 {
block0(v0: i32x8, v1: i32x8, v2: i32x8):
    v3 = bitselect v2, v0, v1
    return v3
}";

const BNOT: &str = "
function %bn(i32x8) -> i32x8 {
block0(v0: i32x8):
    v1 = bnot v0
    return v1
}";

const BAND_BOR_BXOR: &str = "
function %bbb(i32x8, i32x8, i32x8, i32x8) -> i32x8 {
block0(v0: i32x8, v1: i32x8, v2: i32x8, v3: i32x8):
    v4 = band v0, v1
    v5 = bor v4, v2
    v6 = bxor v5, v3
    return v6
}";

const FLOAT_OPS: &str = "
function %fops(i64, f32x8, f32x8, f32x8) -> f32x8 {
block0(v0: i64, v1: f32x8, v2: f32x8, v3: f32x8):
    v4 = load.f32x8 v0
    v5 = fadd v1, v4
    v6 = fmul v5, v4
    v7 = fma v6, v2, v3
    return v7
}";

const FLOAT_OPS_512: &str = "
function %fops(i64, f32x16, f32x16, f32x16) -> f32x16 {
block0(v0: i64, v1: f32x16, v2: f32x16, v3: f32x16):
    v4 = load.f32x16 v0
    v5 = fadd v1, v4
    v6 = fmul v5, v4
    v7 = fma v6, v2, v3
    return v7
}";

const FOLD_LOAD: &str = "
function %fold(i64, i64) -> f32x8 {
block0(v0: i64, v1: i64):
    v2 = load.f32x8 v0
    v3 = load.f32x8 v1
    v4 = fadd v2, v3
    return v4
}";

const BITSELECT_512: &str = "
function %bs(i32x16, i32x16, i32x16) -> i32x16 {
block0(v0: i32x16, v1: i32x16, v2: i32x16):
    v3 = bitselect v2, v0, v1
    return v3
}";

const BNOT_512: &str = "
function %bn(i32x16) -> i32x16 {
block0(v0: i32x16):
    v1 = bnot v0
    return v1
}";

#[test]
fn bitselect_256() {
    let code = compile(256, BITSELECT);
    // vpand 0xDB, vpandn 0xDF, vpor 0xEB — all must appear at L256.
    assert!(has_wide_op(&code, &[0xDB]), "no vpand ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xDF]), "no vpandn ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xEB]), "no vpor ymm: {code:02x?}");
    assert!(
        !has_legacy_op(&code, &[0xDB, 0xDF, 0xEB]),
        "xmm-width boolean op: {code:02x?}"
    );
}

#[test]
fn bnot_256() {
    let code = compile(256, BNOT);
    // vpcmpeqb 0x74 (all-ones) + vpxor 0xEF.
    assert!(has_wide_op(&code, &[0x74]), "no vpcmpeqb ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xEF]), "no vpxor ymm: {code:02x?}");
    assert!(!has_legacy_op(&code, &[0xEF]), "xmm pxor: {code:02x?}");
}

#[test]
fn boolean_ops_256() {
    let code = compile(256, BAND_BOR_BXOR);
    assert!(has_wide_op(&code, &[0xDB]), "no vpand ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xEB]), "no vpor ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xEF]), "no vpxor ymm: {code:02x?}");
    assert!(
        !has_legacy_op(&code, &[0xDB, 0xEB, 0xEF]),
        "xmm-width boolean op: {code:02x?}"
    );
}

#[test]
fn float_ops_256() {
    // load (vmovups 0x10), vaddps 0x58, vmulps 0x59, vfmadd213ps 0xA8 — all
    // operand types went through the XmmMem256 retag.
    let code = compile(256, FLOAT_OPS);
    assert!(has_wide_op(&code, &[0x10]), "no vmovups ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0x58]), "no vaddps ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0x59]), "no vmulps ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xA8]), "no vfmadd ymm: {code:02x?}");
}

#[test]
fn float_ops_512() {
    let code = compile(512, FLOAT_OPS_512);
    assert!(has_wide_op(&code, &[0x10]), "no vmovups zmm: {code:02x?}");
    assert!(has_wide_op(&code, &[0x58]), "no vaddps zmm: {code:02x?}");
    assert!(has_wide_op(&code, &[0x59]), "no vmulps zmm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xA8]), "no vfmadd zmm: {code:02x?}");
}

#[test]
fn fold_load_256() {
    // v2 folds into vaddps's memory operand — exactly one vmovups load.
    let code = compile(256, FOLD_LOAD);
    let loads = code
        .windows(3)
        .filter(|w| w[0] == 0xC5 && (w[1] & 0x04) != 0 && w[2] == 0x10)
        .count()
        + code
            .windows(4)
            .filter(|w| w[0] == 0xC4 && (w[2] & 0x04) != 0 && w[3] == 0x10)
            .count();
    assert!(has_wide_op(&code, &[0x58]), "no vaddps ymm: {code:02x?}");
    assert_eq!(loads, 1, "load did not fold into vaddps: {code:02x?}");
}

#[test]
fn bitselect_512() {
    let code = compile(512, BITSELECT_512);
    // vpandd/vpandnd/vpord — same opcode bytes, EVEX.512 form.
    assert!(has_wide_op(&code, &[0xDB]), "no vpandd zmm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xDF]), "no vpandnd zmm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xEB]), "no vpord zmm: {code:02x?}");
    assert!(
        !has_legacy_op(&code, &[0xDB, 0xDF, 0xEB]),
        "xmm-width boolean op: {code:02x?}"
    );
}

#[test]
fn bnot_512() {
    let code = compile(512, BNOT_512);
    // vpternlogd 0x25 (all-ones LUT) + vpxord 0xEF.
    assert!(has_wide_op(&code, &[0x25]), "no vpternlogd zmm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xEF]), "no vpxord zmm: {code:02x?}");
}

//! Wide-vector (256/512-bit) boolean lowering tests: compile small CLIF
//! functions on x64 with `WIDE_VECTOR_BITS` set and check the emitted bytes
//! contain VEX-256/EVEX-512 encodings of the boolean ops — and no xmm-width
//! `pand`/`por`/`pxor` that would silently compute only the low 128 bits.

// x86 codegen is only compiled when the `x86` cranelift feature is active —
// `host-arch` provides it on x86_64 hosts. Everything below is a no-op
// elsewhere (`lookup_by_name("x86_64")` wouldn't find an ISA anyway).
#![cfg(target_arch = "x86_64")]

use core::sync::atomic::Ordering;

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::isa::x64::WIDE_VECTOR_BITS;
use cranelift_codegen::settings::{self, Configurable};

fn isa() -> std::sync::Arc<dyn TargetIsa> {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(b);
    let mut isa_b = cranelift_codegen::isa::lookup_by_name("x86_64").unwrap();
    isa_b.enable("has_avx").unwrap();
    isa_b.enable("has_avx2").unwrap();
    isa_b.enable("has_fma").unwrap();
    isa_b.enable("has_avx512f").unwrap();
    isa_b.enable("has_avx512bw").unwrap();
    isa_b.enable("has_avx512vl").unwrap();
    isa_b.enable("has_avx_vnni").unwrap();
    isa_b.enable("has_avx512vnni").unwrap();
    isa_b.finish(flags).unwrap()
}

fn parse(clif: &str) -> Function {
    cranelift_reader::parse_functions(clif)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

/// `WIDE_VECTOR_BITS` is process-global — tests run on threads, so a 512
/// compile must hold the ceiling for its whole compile/reset window.
static COMPILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn compile(bits: u32, clif: &str) -> Vec<u8> {
    let isa = isa();
    let _g = COMPILE_LOCK.lock().unwrap();
    WIDE_VECTOR_BITS.store(bits, Ordering::Relaxed);
    let mut ctx = Context::for_function(parse(clif));
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    WIDE_VECTOR_BITS.store(128, Ordering::Relaxed);
    ctx.compiled_code().unwrap().code_buffer().to_vec()
}

/// Does `code` contain a VEX.256 (`C5`/`C4`) or EVEX.512 (`62`) encoding of
/// one of `ops`? VEX.256: L=1 in vvvvL byte. EVEX: L'L = 10 in P2[6:5].
fn has_wide_op(code: &[u8], ops: &[u8]) -> bool {
    code.windows(3)
        .any(|w| w[0] == 0xC5 && (w[1] & 0x04) != 0 && ops.contains(&w[2]))
        || code
            .windows(4)
            .any(|w| w[0] == 0xC4 && (w[2] & 0x04) != 0 && ops.contains(&w[3]))
        || code
            .windows(5)
            .any(|w| w[0] == 0x62 && (w[3] & 0x60) == 0x40 && ops.contains(&w[4]))
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
    assert!(
        has_wide_op(&code, &[0x25]),
        "no vpternlogd zmm: {code:02x?}"
    );
    assert!(has_wide_op(&code, &[0xEF]), "no vpxord zmm: {code:02x?}");
}

const INT_OPS: &str = "
function %iops(i32x8, i32x8) -> i32x8 {
block0(v0: i32x8, v1: i32x8):
    v2 = iadd v0, v1
    v3 = imul v2, v1
    v4 = isub v3, v0
    v5 = smin v4, v2
    return v5
}";

const INT_OPS_512: &str = "
function %iops(i32x16, i32x16) -> i32x16 {
block0(v0: i32x16, v1: i32x16):
    v2 = iadd v0, v1
    v3 = imul v2, v1
    v4 = isub v3, v0
    v5 = smax v4, v2
    return v5
}";

const ICMP_256: &str = "
function %cmp(i32x8, i32x8) -> i32x8 {
block0(v0: i32x8, v1: i32x8):
    v2 = icmp eq v0, v1
    v3 = icmp ugt v0, v1
    v4 = band v2, v3
    return v4
}";

const SPLAT_INT: &str = "
function %splat(i32) -> i32x8 {
block0(v0: i32):
    v1 = splat.i32x8 v0
    return v1
}";

const SPLAT_512: &str = "
function %splat(i64) -> i64x8 {
block0(v0: i64):
    v1 = splat.i64x8 v0
    return v1
}";

const F64_OPS: &str = "
function %dops(f64x4, f64x4) -> f64x4 {
block0(v0: f64x4, v1: f64x4):
    v2 = fadd v0, v1
    v3 = fmin v2, v1
    v4 = fneg v3
    v5 = fma v4, v0, v1
    return v5
}";

const SHIFT_256: &str = "
function %sh(i32x8, i32) -> i32x8 {
block0(v0: i32x8, v1: i32):
    v2 = ishl v0, v1
    v3 = ushr v2, v1
    return v3
}";

const SHIFT_IMM: &str = "
function %sh(i32x8) -> i32x8 {
block0(v0: i32x8):
    v9 = iconst.i32 3
    v1 = ishl v0, v9
    v10 = iconst.i32 2
    v2 = ushr v1, v10
    return v2
}";

#[test]
fn int_ops_256() {
    let code = compile(256, INT_OPS);
    // vpaddd 0xFE, vpsubd 0xFA, vpmulld 0x40 (0F38), vpminsd 0x39 (0F38).
    assert!(has_wide_op(&code, &[0xFE]), "no vpaddd ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xFA]), "no vpsubd ymm: {code:02x?}");
    assert!(
        has_wide_op(&code, &[0x40, 0x39]),
        "no vpmulld/vpminsd ymm: {code:02x?}"
    );
    assert!(
        !has_legacy_op(&code, &[0xFE, 0xFA]),
        "xmm-width int op: {code:02x?}"
    );
}

#[test]
fn int_ops_512() {
    let code = compile(512, INT_OPS_512);
    assert!(has_wide_op(&code, &[0xFE]), "no vpaddd zmm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xFA]), "no vpsubd zmm: {code:02x?}");
    assert!(
        has_wide_op(&code, &[0x40, 0x3D]),
        "no vpmulld/vpmaxsd zmm: {code:02x?}"
    );
}

#[test]
fn icmp_256() {
    let code = compile(256, ICMP_256);
    // eq: vpcmpeqd 0x76; ugt: two vpxor (flip mask) + vpcmpgtd 0x66.
    assert!(has_wide_op(&code, &[0x76]), "no vpcmpeqd ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0x66]), "no vpcmpgtd ymm: {code:02x?}");
    assert!(
        has_wide_op(&code, &[0xEF]),
        "no vpxor ymm (flip mask): {code:02x?}"
    );
    assert!(
        !has_legacy_op(&code, &[0x76, 0x66]),
        "xmm-width compare: {code:02x?}"
    );
}

#[test]
fn splat_256() {
    let code = compile(256, SPLAT_INT);
    // vpbroadcastd 0x7C is a 0F38-map op -> 3-byte VEX.
    assert!(
        has_wide_op(&code, &[0x7C]),
        "no vpbroadcastd ymm: {code:02x?}"
    );
}

#[test]
fn splat_512() {
    let code = compile(512, SPLAT_512);
    // vpbroadcastq zmm: EVEX 0F38 0x7D W=1.
    assert!(
        has_wide_op(&code, &[0x7D]),
        "no vpbroadcastq zmm: {code:02x?}"
    );
}

#[test]
fn f64_ops_256() {
    let code = compile(256, F64_OPS);
    // vaddpd 0x58, vminpd 0x5D, vfmadd213pd 0xA8, vpxor 0xEF (fneg mask).
    assert!(has_wide_op(&code, &[0x58]), "no vaddpd ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0x5D]), "no vminpd ymm: {code:02x?}");
    assert!(
        has_wide_op(&code, &[0xA8]),
        "no vfmadd213pd ymm: {code:02x?}"
    );
    assert!(
        has_wide_op(&code, &[0xEF]),
        "no vpxor (fneg) ymm: {code:02x?}"
    );
}

#[test]
fn shifts_256() {
    let code = compile(256, SHIFT_256);
    // vpslld 0xF2, vpsrld 0xD2 with xmm count.
    assert!(has_wide_op(&code, &[0xF2]), "no vpslld ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0xD2]), "no vpsrld ymm: {code:02x?}");
}

#[test]
fn shift_imm_256() {
    let code = compile(256, SHIFT_IMM);
    // imm forms: vpslld.yi 0x72 digit6, vpsrld.yi 0x72 digit2.
    assert!(has_wide_op(&code, &[0x72]), "no imm-shift ymm: {code:02x?}");
}

const EXTRACT_INT_256: &str = "
function %ex(i32x8) -> i32 {
block0(v0: i32x8):
    v1 = extractlane v0, 5
    v2 = extractlane v0, 1
    v3 = iadd v1, v2
    return v3
}";

const EXTRACT_INT_512: &str = "
function %ex(i64x8) -> i64 {
block0(v0: i64x8):
    v1 = extractlane v0, 6
    v2 = extractlane v0, 2
    v3 = iadd v1, v2
    return v3
}";

const INSERT_INT_256: &str = "
function %ins(i32x8, i32) -> i32x8 {
block0(v0: i32x8, v1: i32):
    v2 = insertlane v0, v1, 6
    v3 = insertlane v2, v1, 2
    return v3
}";

const EXTRACT_F64_256: &str = "
function %exf(f64x4) -> f64 {
block0(v0: f64x4):
    v1 = extractlane v0, 3
    v2 = extractlane v0, 0
    v3 = fadd v1, v2
    return v3
}";

const INSERT_F64_256: &str = "
function %insf(f64x4, f64) -> f64x4 {
block0(v0: f64x4, v1: f64):
    v2 = insertlane v0, v1, 3
    v3 = insertlane v2, v1, 0
    return v3
}";

const WIDEN_256: &str = "
function %wid(i16x16) -> i32x8 {
block0(v0: i16x16):
    v2 = uwiden_low.i16x16 v0
    v3 = swiden_high.i16x16 v0
    v4 = iadd v2, v3
    return v4
}";

#[test]
fn extractlane_int_256() {
    let code = compile(256, EXTRACT_INT_256);
    // vextractf128 0x19 (0F3A) + vpextrd 0x16 (0F3A).
    assert!(has_wide_op(&code, &[0x19]), "no vextractf128: {code:02x?}");
    assert!(
        code.windows(4)
            .any(|w| w[0] == 0xC4 && w[3] == 0x16 || w[0] == 0xC5 && w[2] == 0x16),
        "no vpextrd: {code:02x?}"
    );
}

#[test]
fn extractlane_int_512() {
    let code = compile(512, EXTRACT_INT_512);
    // vextractf32x4 0x19 EVEX + vpextrq.
    assert!(has_wide_op(&code, &[0x19]), "no vextractf32x4: {code:02x?}");
}

#[test]
fn insertlane_int_256() {
    let code = compile(256, INSERT_INT_256);
    // vinsertf128 0x18 + vpinsrd 0x22 (0F3A).
    assert!(has_wide_op(&code, &[0x18]), "no vinsertf128: {code:02x?}");
    assert!(
        code.windows(4).any(|w| w[0] == 0xC4 && w[3] == 0x22),
        "no vpinsrd: {code:02x?}"
    );
}

#[test]
fn f64_lanes_256() {
    let code = compile(256, EXTRACT_F64_256);
    assert!(has_wide_op(&code, &[0x19]), "no vextractf128: {code:02x?}");
    let code = compile(256, INSERT_F64_256);
    assert!(has_wide_op(&code, &[0x18]), "no vinsertf128: {code:02x?}");
    // vshufpd 0xC6 (VEX.128 is correct — the in-chunk merge is xmm-width
    // by design).
    assert!(
        code.windows(3).any(|w| w[0] == 0xC5 && w[2] == 0xC6),
        "no vshufpd xmm: {code:02x?}"
    );
}

#[test]
fn widen_256() {
    let code = compile(256, WIDEN_256);
    // vpmovzxwd 0x33, vpmovsxwd 0x23 (0F38) + vextractf128 0x19 (high).
    assert!(has_wide_op(&code, &[0x33]), "no vpmovzxwd ymm: {code:02x?}");
    assert!(has_wide_op(&code, &[0x23]), "no vpmovsxwd ymm: {code:02x?}");
    assert!(
        has_wide_op(&code, &[0x19]),
        "no vextractf128 (high half): {code:02x?}"
    );
}

/// 0F38-map variant of `has_wide_op`: VEX.256 is `C4` with mm=0F38 and L=1
/// (C5 can't encode 0F38); EVEX.512 is `62` with mm=2 in P1 and L'L=10.
fn has_wide_op38(code: &[u8], ops: &[u8]) -> bool {
    code.windows(4)
        .any(|w| w[0] == 0xC4 && (w[1] & 0x1F) == 0x02 && (w[2] & 0x04) != 0 && ops.contains(&w[3]))
        || code.windows(5).any(|w| {
            w[0] == 0x62 && (w[1] & 0x03) == 0x02 && (w[3] & 0x60) == 0x40 && ops.contains(&w[4])
        })
}

/// Legacy 0F38-map encoding check (`66 0F38 xx`).
fn has_legacy_op38(code: &[u8], ops: &[u8]) -> bool {
    code.windows(4)
        .any(|w| w[0] == 0x66 && w[1] == 0x0F && w[2] == 0x38 && ops.contains(&w[3]))
}

const SDOT: &str = "
function %sdot(i16x16, i16x16) -> i32x8 {
block0(v0: i16x16, v1: i16x16):
    v2 = sdot.i16x16 v0, v1
    return v2
}";

const UDOT: &str = "
function %udot(i8x32, i8x32) -> i16x16 {
block0(v0: i8x32, v1: i8x32):
    v2 = udot.i8x32 v0, v1
    return v2
}";

const UDOT_ACC: &str = "
function %udot_acc(i32x8, i8x32, i8x32) -> i32x8 {
block0(v0: i32x8, v1: i8x32, v2: i8x32):
    v3 = udot_acc.i32x8 v0, v1, v2
    return v3
}";

#[test]
fn sdot_256() {
    let code = compile(256, SDOT);
    assert!(has_wide_op(&code, &[0xF5]), "no vpmaddwd ymm: {code:02x?}");
    assert!(!has_legacy_op(&code, &[0xF5]), "xmm pmaddwd: {code:02x?}");
}

#[test]
fn sdot_512() {
    let code = compile(512, SDOT);
    assert!(has_wide_op(&code, &[0xF5]), "no vpmaddwd zmm: {code:02x?}");
    assert!(!has_legacy_op(&code, &[0xF5]), "xmm pmaddwd: {code:02x?}");
}

#[test]
fn udot_256() {
    let code = compile(256, UDOT);
    assert!(
        has_wide_op38(&code, &[0x04]),
        "no vpmaddubsw ymm: {code:02x?}"
    );
    assert!(
        !has_legacy_op38(&code, &[0x04]),
        "xmm pmaddubsw: {code:02x?}"
    );
}

#[test]
fn udot_512() {
    let code = compile(512, UDOT);
    assert!(
        has_wide_op38(&code, &[0x04]),
        "no vpmaddubsw zmm: {code:02x?}"
    );
    assert!(
        !has_legacy_op38(&code, &[0x04]),
        "xmm pmaddubsw: {code:02x?}"
    );
}

#[test]
fn udot_acc_256() {
    let code = compile(256, UDOT_ACC);
    assert!(
        has_wide_op38(&code, &[0x50]),
        "no vpdpbusd ymm: {code:02x?}"
    );
}

#[test]
fn udot_acc_512() {
    let code = compile(512, UDOT_ACC);
    assert!(
        has_wide_op38(&code, &[0x50]),
        "no vpdpbusd zmm: {code:02x?}"
    );
}

const VSWIZZLE: &str = "
function %vsw(i32x8, i32x8) -> i32x8 {
block0(v0: i32x8, v1: i32x8):
    v2 = vswizzle.i32x8 v0, v1
    return v2
}";

const VSWIZZLE_F64: &str = "
function %vswf(f64x8, i64x8) -> f64x8 {
block0(v0: f64x8, v1: i64x8):
    v2 = vswizzle.f64x8 v0, v1
    return v2
}";

const VSWIZZLE2: &str = "
function %vsw2(i32x16, i32x16, i32x16) -> i32x16 {
block0(v0: i32x16, v1: i32x16, v2: i32x16):
    v3 = vswizzle2.i32x16 v0, v1, v2
    return v3
}";

#[test]
fn vswizzle_256() {
    let code = compile(256, VSWIZZLE);
    assert!(has_wide_op38(&code, &[0x36]), "no vpermd ymm: {code:02x?}");
}

#[test]
fn vswizzle_512() {
    let code = compile(512, VSWIZZLE);
    assert!(has_wide_op38(&code, &[0x36]), "no vpermd zmm: {code:02x?}");
}

#[test]
fn vswizzle_f64_512() {
    let code = compile(512, VSWIZZLE_F64);
    assert!(has_wide_op38(&code, &[0x16]), "no vpermpd zmm: {code:02x?}");
}

#[test]
fn vswizzle2_512() {
    let code = compile(512, VSWIZZLE2);
    assert!(
        has_wide_op38(&code, &[0x7E]),
        "no vpermt2d zmm: {code:02x?}"
    );
}

/// EVEX 0F38-map check for a specific L'L (index width): `62`, mm=2 in P1,
/// L'L = `ll` in P2[6:5]. Gathers/scatters encode the index register width
/// in L'L (1 = ymm, 2 = zmm).
fn has_evex_op38(code: &[u8], ops: &[u8], ll: u8) -> bool {
    code.windows(5).any(|w| {
        w[0] == 0x62 && (w[1] & 0x03) == 0x02 && (w[3] & 0x60) == (ll << 5) && ops.contains(&w[4])
    })
}

const GATHER_256: &str = "
function %g(i64, i32x8) -> i32x8 {
block0(v0: i64, v1: i32x8):
    v2 = vgather.i32x8 notrap aligned v0, v1+0
    return v2
}";

const GATHER_512: &str = "
function %g(i64, i32x16) -> i32x16 {
block0(v0: i64, v1: i32x16):
    v2 = vgather.i32x16 notrap aligned v0, v1+0
    return v2
}";

const SCATTER_256: &str = "
function %s(i32x8, i64, i32x8) {
block0(v0: i32x8, v1: i64, v2: i32x8):
    vscatter notrap aligned v0, v1, v2+0
    return
}";

const SCATTER_512: &str = "
function %s(i32x16, i64, i32x16) {
block0(v0: i32x16, v1: i64, v2: i32x16):
    vscatter notrap aligned v0, v1, v2+0
    return
}";

#[test]
fn gather_256() {
    let code = compile(256, GATHER_256);
    // vpgatherdd ymm = VEX.256 0F38 0x90 (all-ones mask + zeroed dst regs are
    // also emitted; only the gather opcode is checked).
    assert!(
        has_wide_op38(&code, &[0x90]),
        "no vpgatherdd ymm: {code:02x?}"
    );
}

#[test]
fn gather_512() {
    let code = compile(512, GATHER_512);
    // vpgatherdd zmm-idx = EVEX 0F38 0x90, L'L=10
    assert!(
        has_evex_op38(&code, &[0x90], 2),
        "no vpgatherdd zmm: {code:02x?}"
    );
}

#[test]
fn scatter_256() {
    let code = compile(256, SCATTER_256);
    // vpscatterdd ymm-idx = EVEX 0F38 0xA0, L'L=01
    assert!(
        has_evex_op38(&code, &[0xA0], 1),
        "no vpscatterdd (ymm idx): {code:02x?}"
    );
}

#[test]
fn scatter_512() {
    let code = compile(512, SCATTER_512);
    // vpscatterdd zmm-idx = EVEX 0F38 0xA0, L'L=10
    assert!(
        has_evex_op38(&code, &[0xA0], 2),
        "no vpscatterdd zmm: {code:02x?}"
    );
}

// ---------------------------------------------------------------------------
// loopvec end-to-end: a loop vectorized at vbits=256 lowers to ymm ops.

use cranelift_codegen::ir::types;
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use rustc_hash::{FxHashMap, FxHashSet};
use target_lexicon::PointerWidth;

/// `acc += a[i] * w[n-1-i]` — two streams, one descending, reduction.
const DESC_LOOP: &str = "
function u0:0() -> f32 {
block0:
    v0 = iconst.i64 0
    v1 = iconst.i64 64
    v2 = iconst.i64 4
    v3 = imul v1, v2
    v4 = iadd v0, v3
    v5 = iconst.i64 252
    v6 = f32const 0x0.0
    jump block1(v0, v6)
block1(v7: i64, v8: f32):
    v9 = icmp eq v7, v4
    brif v9, block3(v8), block2
block2:
    v10 = load.f32 notrap v7
    v11 = isub v5, v7
    v12 = load.f32 notrap v11
    v13 = fmul v10, v12
    v14 = fadd v8, v13
    v17 = iconst.i64 4
    v15 = iadd v7, v17
    jump block1(v15, v14)
block3(v16: f32):
    return v16
}";

#[test]
fn loopvec_desc_emits_ymm_end_to_end() {
    let mut func = parse(DESC_LOOP);
    let tcfg = TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    };
    let n = pliron_clif_opt::loopvec::run(
        &mut func,
        &mut pliron_clif_opt::pipeline::Analysis::default(),
        "desc",
        &FxHashSet::default(),
        tcfg,
        true,
        true,
        true,
        &FxHashMap::default(),
        pliron_clif_opt::loopvec::WideCaps {
            vbits: 256,
            fma: true,
            avx512bw: false,
        },
    );
    assert_eq!(n, 1, "loop did not vectorize");
    let has_x8 = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .any(|i| func.dfg.ctrl_typevar(i) == types::F32X8);
    assert!(has_x8, "no f32x8 insts in vectorized loop");
    let has_swizzle = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .any(|i| func.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Vswizzle);
    assert!(has_swizzle, "descending stream did not emit vswizzle");

    let isa = isa();
    let _g = COMPILE_LOCK.lock().unwrap();
    WIDE_VECTOR_BITS.store(256, Ordering::Relaxed);
    let mut ctx = Context::for_function(func);
    ctx.compile(&*isa, &mut ControlPlane::default())
        .expect("wide loopvec output must lower");
    WIDE_VECTOR_BITS.store(128, Ordering::Relaxed);
    let code = ctx.compiled_code().unwrap().code_buffer().to_vec();
    // vmovups ymm (0x10/0x11), vaddps ymm (0x58), vmulps ymm (0x59).
    assert!(
        has_wide_op(&code, &[0x10, 0x11, 0x58, 0x59]),
        "no ymm encodings in lowered loop: {code:02x?}"
    );
}

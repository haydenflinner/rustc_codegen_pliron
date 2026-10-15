//! Wide-vector (>128-bit) safety gate for `bnot`/`bitselect`/boolean ops.
//!
//! A `bnot`/`bitselect`/`band`/`bor`/`bxor` on `i8x32`/`i64x8`-style types
//! must either compile with a full-width lowering (x64 with
//! `WIDE_VECTOR_BITS` raised — the emitted bytes are checked in
//! `widevec_lower.rs`) or fail loudly. What must never happen is an xmm-
//! width path computing — or an xmm-width register class preserving — only
//! the low 128 bits while the upper lanes read garbage.
//!
//! Two layers are exercised, host-independently:
//!
//! - Non-x86 hosts: the ISA has no wide vector support at all, so
//!   compilation must return `Err` (never `Ok` with silently wrong code).
//! - x86_64 hosts: `rc_for_type` rejects a vector wider than the
//!   `WIDE_VECTOR_BITS` ceiling. Without that gate a wide op compiled at
//!   the 128 default emitted ymm/zmm insns into an xmm-canonical Float
//!   class — `gen_move`/spill slots keep only the low 128 bits.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};

fn host_isa() -> Arc<dyn TargetIsa> {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(b);
    cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
        .expect("host ISA")
        .finish(flags)
        .expect("ISA flags")
}

fn parse(clif: &str) -> Function {
    cranelift_reader::parse_functions(clif)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

/// Compile `clif`, returning Ok(bytes) or Err(codegen error or panic
/// message) — a panic is still a loud failure, not a silent miscompile.
fn compile(isa: &dyn TargetIsa, clif: &str) -> Result<Vec<u8>, String> {
    let mut ctx = Context::for_function(parse(clif));
    match catch_unwind(AssertUnwindSafe(|| {
        match ctx.compile(isa, &mut ControlPlane::default()) {
            Ok(_) => Ok(ctx.compiled_code().unwrap().code_buffer().to_vec()),
            Err(e) => Err(format!("{e:?}")),
        }
    })) {
        Ok(r) => r,
        Err(p) => Err(format!(
            "panic: {}",
            p.downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        )),
    }
}

/// name, CLIF, vector width in bits.
const CASES: &[(&str, &str, u32)] = &[
    (
        "bnot.i8x32",
        "function %f(i8x32) -> i8x32 {
            block0(v0: i8x32):
                v1 = bnot v0
                return v1
        }",
        256,
    ),
    (
        "bitselect.i8x32",
        "function %f(i8x32, i8x32, i8x32) -> i8x32 {
            block0(v0: i8x32, v1: i8x32, v2: i8x32):
                v3 = bitselect v2, v0, v1
                return v3
        }",
        256,
    ),
    (
        "band.i8x32",
        "function %f(i8x32, i8x32) -> i8x32 {
            block0(v0: i8x32, v1: i8x32):
                v2 = band v0, v1
                return v2
        }",
        256,
    ),
    (
        "bnot.i64x8",
        "function %f(i64x8) -> i64x8 {
            block0(v0: i64x8):
                v1 = bnot v0
                return v1
        }",
        512,
    ),
    (
        "bitselect.i64x8",
        "function %f(i64x8, i64x8, i64x8) -> i64x8 {
            block0(v0: i64x8, v1: i64x8, v2: i64x8):
                v3 = bitselect v2, v0, v1
                return v3
        }",
        512,
    ),
];

/// Hosts without x64 codegen (aarch64, riscv64, ...) must reject every
/// wide vector op — the alternative to a correct lowering is a loud
/// `CodegenError`, never silently truncated machine code.
#[cfg(not(target_arch = "x86_64"))]
#[test]
fn wide_ops_fail_loudly() {
    let isa = host_isa();
    for &(name, clif, _) in CASES {
        match compile(&*isa, clif) {
            Ok(code) => panic!("{name} compiled on a non-x86 host: {code:02x?}"),
            Err(_) => {}
        }
    }
}

/// On x86_64 the ceiling is explicit: below it the op must error (an
/// xmm-width Float class would spill/move only the low 128 bits); at or
/// above it the full-width lowering applies (`widevec_lower.rs` checks
/// the emitted opcodes).
#[cfg(target_arch = "x86_64")]
#[test]
fn wide_ops_respect_ceiling() {
    use core::sync::atomic::Ordering;
    use cranelift_codegen::isa::x64::WIDE_VECTOR_BITS;

    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(b);
    let mut isa_b = cranelift_codegen::isa::lookup_by_name("x86_64").unwrap();
    isa_b.enable("has_avx").unwrap();
    isa_b.enable("has_avx2").unwrap();
    isa_b.enable("has_avx512f").unwrap();
    isa_b.enable("has_avx512bw").unwrap();
    isa_b.enable("has_avx512vl").unwrap();
    let isa = isa_b.finish(flags).unwrap();

    // Ceiling at its 128-bit default: every wide type must fail loudly.
    // (The error is `rc_for_type`'s "exceeds the x64 wide-vector ceiling";
    // any earlier rejection — e.g. the ABI signature check that calls the
    // same helper — is an equally loud failure.)
    WIDE_VECTOR_BITS.store(128, Ordering::Relaxed);
    for &(name, clif, _) in CASES {
        match compile(&*isa, clif) {
            Ok(code) => panic!("{name} compiled with WIDE_VECTOR_BITS=128: {code:02x?}"),
            Err(_) => {}
        }
    }
    // Raised ceiling: only the covered widths compile.
    for bits in [256, 512] {
        WIDE_VECTOR_BITS.store(bits, Ordering::Relaxed);
        for &(name, clif, width) in CASES {
            match compile(&*isa, clif) {
                Ok(_) => assert!(width <= bits, "{name} ({width}-bit) compiled at {bits} ceiling"),
                Err(e) => assert!(
                    width > bits,
                    "{name} ({width}-bit) failed at {bits} ceiling: {e}"
                ),
            }
        }
    }
    WIDE_VECTOR_BITS.store(128, Ordering::Relaxed);
}

//! aarch64 narrow-int cleanliness (`PLIRON_NARROWEXT=1`) and `br_table`
//! split-edge sharing (`PLIRON_EDGEDEDUP=1`): compile small CLIF functions
//! and check the emitted assembly drops the redundant `uxtb`/`tst #0xff`
//! and the repeated `mov; b` edge blocks, while keeping the extensions a
//! dirty value still needs.
//!
//! Both knobs are read once per process (`OnceLock`), so every test here
//! sets them before its first compile; all tests in this binary run with
//! them on.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};

fn isa() -> std::sync::Arc<dyn TargetIsa> {
    // SAFETY: tests set the same values from every thread before any
    // compile reads them; nothing else in this binary reads the env.
    unsafe {
        std::env::set_var("PLIRON_NARROWEXT", "1");
        std::env::set_var("PLIRON_EDGEDEDUP", "1");
    }
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(b);
    cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(flags)
        .unwrap()
}

/// Compile `clif` and return its disassembly (vcode text), one inst per line.
fn asm(clif: &str) -> String {
    let isa = isa();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.set_disasm(true);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.compiled_code().unwrap().vcode.clone().unwrap()
}

fn count(asm: &str, mnem: &str) -> usize {
    asm.lines()
        .filter(|l| l.split_whitespace().next() == Some(mnem))
        .count()
}

/// `band x, 1` returned as `i8 uext`: the `and` already zero-extends, so
/// the ABI's return `uxtb` is dead.
#[test]
fn ret_band_imm_no_uxtb() {
    let a = asm("
function %f(i8) -> i8 uext apple_aarch64 {
block0(v0: i8):
    v1 = iconst.i8 1
    v2 = band v0, v1
    return v2
}");
    assert_eq!(count(&a, "uxtb"), 0, "{a}");
}

/// A canonical narrow constant returned as `uext` needs no `uxtb`.
#[test]
fn ret_iconst_no_uxtb() {
    let a = asm("
function %f(i64) -> i8 uext apple_aarch64 {
block0(v0: i64):
    v1 = iconst.i8 0
    v2 = iconst.i8 255
    v4 = iconst.i64 7
    v3 = icmp eq v0, v4
    brif v3, block1, block2
block1:
    return v1
block2:
    return v2
}");
    assert_eq!(count(&a, "uxtb"), 0, "{a}");
}

/// A dirty narrow value (an `iadd` can carry into bit 8) still gets its
/// return extension.
#[test]
fn ret_dirty_keeps_uxtb() {
    let a = asm("
function %f(i8, i8) -> i8 uext apple_aarch64 {
block0(v0: i8, v1: i8):
    v2 = iadd v0, v1
    return v2
}");
    assert_eq!(count(&a, "uxtb"), 1, "{a}");
}

/// `bor` of a clean value and a dirty one is dirty: the `uxtb` stays.
#[test]
fn ret_bor_dirty_keeps_uxtb() {
    let a = asm("
function %f(i8, i8) -> i8 uext apple_aarch64 {
block0(v0: i8, v1: i8):
    v2 = iconst.i8 15
    v3 = band v0, v2
    v4 = bor v3, v1
    return v4
}");
    assert_eq!(count(&a, "uxtb"), 1, "{a}");
}

/// `brif` on a loaded byte (`ldrb` zero-extends) is a single `cbnz`/`cbz`,
/// no `tst #0xff`.
#[test]
fn brif_loaded_byte_cbnz() {
    let a = asm("
function %f(i64) -> i64 apple_aarch64 {
block0(v0: i64):
    v1 = load.i8 notrap v0
    brif v1, block1, block2
block1:
    v2 = iconst.i64 1
    return v2
block2:
    v3 = iconst.i64 2
    return v3
}");
    assert_eq!(count(&a, "tst"), 0, "{a}");
    assert_eq!(count(&a, "cbnz") + count(&a, "cbz"), 1, "{a}");
}

/// `band (ireduce (ushr x 6)) 63` masks the wide shift result directly:
/// no `uxtb` for the `ireduce`. The UTF-8 continuation-byte shape.
#[test]
fn band_ireduce_no_uxtb() {
    let a = asm("
function %f(i32, i64) apple_aarch64 {
block0(v0: i32, v1: i64):
    v2 = iconst.i32 6
    v3 = ushr v0, v2
    v4 = ireduce.i8 v3
    v5 = iconst.i8 63
    v6 = band v4, v5
    v7 = iconst.i8 128
    v8 = bor v6, v7
    store notrap v8, v1
    return
}");
    assert_eq!(count(&a, "uxtb"), 0, "{a}");
}

/// `band (ireduce (ishl.i64 y 34)) 63` is 0: the `ireduce`'s wide source
/// must not be folded as a shifted operand of the narrow `and`, which masks
/// the shift amount by the i8 width (`and w, w, w, LSL 2`) instead of 64. Rustlantis
/// seed 12309.
#[test]
fn band_ireduce_of_wide_shift() {
    let a = asm("
function %f(i64, i64) -> i8 uext apple_aarch64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 34
    v3 = ishl v0, v2
    store notrap v3, v1
    v4 = ireduce.i8 v3
    v5 = iconst.i8 63
    v6 = band v4, v5
    return v6
}");
    assert!(
        !a.lines().any(|l| l.trim_start().starts_with("and ") && l.contains("LSL")),
        "shift folded into the narrow and with its amount masked to i8:\n{a}"
    );
}

/// `uextend` of a clean narrow value is a passthrough.
#[test]
fn uextend_clean_passthrough() {
    let a = asm("
function %f(i8) -> i64 apple_aarch64 {
block0(v0: i8):
    v1 = iconst.i8 7
    v2 = band v0, v1
    v3 = uextend.i64 v2
    return v3
}");
    assert_eq!(count(&a, "uxtb"), 0, "{a}");
}

/// `br_table` entries passing the same block call share one split-edge
/// block: the four entries forwarding `v1` to `block1` land on one label
/// (and so one `mov; b` at most), not four identical edge blocks.
#[test]
fn br_table_shared_edge_block() {
    let a = asm("
function %f(i32, i64, i64) -> i64 apple_aarch64 {
block0(v0: i32, v1: i64, v2: i64):
    br_table v0, block2, [block1(v1), block1(v1), block3, block1(v1), block1(v1)]
block3:
    jump block1(v2)
block2:
    v3 = iconst.i64 9
    jump block1(v3)
block1(v4: i64):
    v7 = iconst.i64 1
    v5 = iadd v4, v7
    v6 = imul v5, v4
    return v6
}");
    let line = a.lines().find(|l| l.contains("jt_entries")).expect(&a);
    let entries: Vec<&str> = line
        .split("jt_entries [")
        .nth(1)
        .unwrap()
        .trim_end_matches(']')
        .split(", ")
        .collect();
    assert_eq!(entries.len(), 5, "{a}");
    assert!(
        entries[0] == entries[1] && entries[1] == entries[3] && entries[3] == entries[4],
        "same-call entries not shared: {entries:?}\n{a}"
    );
    assert_ne!(entries[0], entries[2], "{a}");
}

/// A two-successor table whose default and only entry pass the same call
/// share one edge block, which must reach regalloc2 as a single CFG edge
/// (a repeated successor reads as a critical edge and fails allocation).
#[test]
fn br_table_default_shared_with_entry() {
    let a = asm("
function %f(i32, i64, i64) -> i64 apple_aarch64 {
block0(v0: i32, v1: i64, v2: i64):
    v3 = iconst.i32 100
    v4 = icmp ult v0, v3
    brif v4, block2, block1(v2)
block2:
    br_table v0, block1(v1), [block1(v1)]
block1(v5: i64):
    v6 = imul v5, v5
    return v6
}");
    assert!(a.contains("jt_entries"), "{a}");
}

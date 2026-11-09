//! aarch64 bitfield fusion (`PLIRON_BFUSE`, default on): `(x>>k)&m`
//! lowers to one `ubfx`, `(x&m)<<k` and `(x<<k)&m'` to one `ubfiz`,
//! and `y | ((x&m)<<k)` to `bfi` when `y`'s field bits are provably
//! zero (bounded known-bits walk; a miss keeps the `orr`). LLVM emits
//! these single-insn forms for `is_mostly_utf8`-style codepoint
//! assembly where we previously spent `lsl`+`and`+`orr`.

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    b.set("is_pic", "true").unwrap();
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

/// `(v0 >> 8) & 0xffff` — plain extract.
const UBFX: &str = "function %f(i32) -> i32 {
block0(v0: i32):
    v10 = iconst.i64 8
    v1 = ushr v0, v10
    v11 = iconst.i32 0xffff
    v2 = band v1, v11
    return v2
}";

/// `(v0 & 0x3f) << 6` — low-mask then shift.
const UBFIZ_MASK_FIRST: &str = "function %f(i32) -> i32 {
block0(v0: i32):
    v10 = iconst.i32 0x3f
    v1 = band v0, v10
    v11 = iconst.i64 6
    v2 = ishl v1, v11
    return v2
}";

/// `(v0 << 8) & 0xff00` — shift then field mask.
const UBFIZ_SHIFT_FIRST: &str = "function %f(i32) -> i32 {
block0(v0: i32):
    v10 = iconst.i64 8
    v1 = ishl v0, v10
    v11 = iconst.i32 0xff00
    v2 = band v1, v11
    return v2
}";

/// `(v0 << 8) & ((1<<50)-1)<<8` on i64 — a real field mask (`w = 50`,
/// non-redundant); `imms = k+w-1 = 57 >= immr = 56` flips a bad
/// encoding all the way to `ubfx`.
const UBFIZ_SHIFT_FIRST_I64: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v10 = iconst.i64 8
    v1 = ishl v0, v10
    v11 = iconst.i64 0x03ffffffffffff00
    v2 = band v1, v11
    return v2
}";

/// `(v0 & 0xff00) >> 8` — field mask then shift back down.
const UBFX_FIELD: &str = "function %f(i32) -> i32 {
block0(v0: i32):
    v10 = iconst.i32 0xff00
    v1 = band v0, v10
    v11 = iconst.i64 8
    v2 = ushr v1, v11
    return v2
}";

/// `(v0 & 0xff) | ((v1 & 0x3f) << 8)` — disjoint field assembly: `bfi`.
const BFI: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v10 = iconst.i64 0xff
    v2 = band v0, v10
    v11 = iconst.i64 0x3f
    v3 = band v1, v11
    v12 = iconst.i64 8
    v4 = ishl v3, v12
    v5 = bor v2, v4
    return v5
}";

/// Same shape but `v0 >> 1` is unproven in the field — must stay `orr`.
const BFI_NEG: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v10 = iconst.i64 1
    v2 = ushr v0, v10
    v11 = iconst.i64 0x3f
    v3 = band v1, v11
    v12 = iconst.i64 8
    v4 = ishl v3, v12
    v5 = bor v2, v4
    return v5
}";

/// Narrow type: `(v0.i8 >> 2) & 0x3f` → `ubfx` on the W register.
const UBFX_I8: &str = "function %f(i8) -> i8 {
block0(v0: i8):
    v10 = iconst.i64 2
    v1 = ushr v0, v10
    v11 = iconst.i8 0x3f
    v2 = band v1, v11
    return v2
}";

#[test]
fn ushr_then_band_is_ubfx() {
    let a = asm(UBFX);
    assert!(
        a.lines().any(|l| {
            let l = l.trim_start();
            (l.starts_with("ubfx ") || l.starts_with("ubfm "))
                && l.contains("#8")
                && l.contains("#16")
        }),
        "want `ubfx w?, w?, #8, #16`:\n{a}"
    );
    assert!(
        !a.lines().any(|l| l.trim_start().starts_with("and ")),
        "no separate `and` left:\n{a}"
    );
}

#[test]
fn band_then_ishl_is_ubfiz() {
    let a = asm(UBFIZ_MASK_FIRST);
    assert!(
        a.lines().any(|l| {
            let l = l.trim_start();
            (l.starts_with("ubfiz ") || l.starts_with("ubfm "))
                && l.contains("#6")
        }),
        "want `ubfiz w?, w?, #6, #6`:\n{a}"
    );
}

#[test]
fn ishl_then_band_is_ubfiz() {
    let a = asm(UBFIZ_SHIFT_FIRST);
    // `ubfiz` immediates are (lsb, width): `x<<8` masked to bits 8..15
    // is `ubfiz w?, w?, #8, #8`. The field-width bug encoded the
    // extract form's `k+w-1` as `imms` and printed `#8, #16`.
    assert!(
        a.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("ubfiz ") && l.contains("#8, #8")
        }),
        "want `ubfiz w?, w?, #8, #8`:\n{a}"
    );
}

#[test]
fn ishl_then_band_i64_is_ubfiz() {
    let a = asm(UBFIZ_SHIFT_FIRST_I64);
    // `(x<<8) & ((1<<50)-1)<<8` = `ubfiz x?, x?, #8, #50`. The
    // field-width bug encoded `imms = k+w-1 = 57 >= immr` and produced
    // `ubfx x, x, #56, #2` (`lsr x, x, #56`) — the top 2 bits instead.
    assert!(
        a.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("ubfiz ") && l.contains("#8, #50")
        }),
        "want `ubfiz x?, x?, #8, #50`:\n{a}"
    );
    assert!(
        !a.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("ubfx ") || l.starts_with("lsr ")
        }),
        "no `ubfx`/`lsr` — that extracts instead of inserts:\n{a}"
    );
}

#[test]
fn band_then_ushr_is_ubfx() {
    let a = asm(UBFX_FIELD);
    assert!(
        a.lines().any(|l| {
            let l = l.trim_start();
            (l.starts_with("ubfx ") || l.starts_with("ubfm "))
                && l.contains("#8")
        }),
        "want `ubfx w?, w?, #8, #8`:\n{a}"
    );
}

#[test]
fn disjoint_bor_is_bfi() {
    let a = asm(BFI);
    assert!(
        a.lines()
            .any(|l| { let l = l.trim_start(); l.starts_with("bfi ") || l.starts_with("bfm ") }),
        "want `bfi x?, x?, #8, #6`:\n{a}"
    );
    assert!(
        !a.lines().any(|l| l.trim_start().starts_with("orr ")),
        "no `orr` left:\n{a}"
    );
}

#[test]
fn unproven_bor_keeps_orr() {
    let a = asm(BFI_NEG);
    assert!(
        !a.lines()
            .any(|l| { let l = l.trim_start(); l.starts_with("bfi ") || l.starts_with("bfm ") }),
        "field bits unproven — `bfm` would clobber live bits:\n{a}"
    );
}

#[test]
fn narrow_ubfx_on_w_register() {
    let a = asm(UBFX_I8);
    assert!(
        a.lines().any(|l| {
            let l = l.trim_start();
            (l.starts_with("ubfx w") || l.starts_with("ubfm w")) && l.contains("#2")
        }),
        "want `ubfx w?, w?, #2, #6`:\n{a}"
    );
}

/// `x <s -64` — the signed-byte continuation bound `sextload` produces:
/// `cmn x, #64` in place of `movn`+`cmp`.
const CMN_I64: &str = "function %f(i64) -> i32 {
block0(v0: i64):
    v10 = iconst.i64 -64
    v1 = icmp slt v0, v10
    v20 = iconst.i32 1
    v21 = iconst.i32 0
    v2 = select v1, v20, v21
    return v2
}";

/// Same on a W register — the i32 iconst stores `0xffffffc0`; the
/// negated-immediate helper sign-extends by type before negating.
const CMN_I32: &str = "function %f(i32) -> i32 {
block0(v0: i32):
    v10 = iconst.i32 -64
    v1 = icmp slt v0, v10
    v20 = iconst.i32 1
    v21 = iconst.i32 0
    v2 = select v1, v20, v21
    return v2
}";

/// `x == 0` must stay on the `is_nonzero` path, not `cmn x, #0`.
const CMP_ZERO: &str = "function %f(i64) -> i32 {
block0(v0: i64):
    v10 = iconst.i64 0
    v1 = icmp eq v0, v10
    v20 = iconst.i32 1
    v21 = iconst.i32 0
    v2 = select v1, v20, v21
    return v2
}";

#[test]
fn negative_imm_compare_is_cmn() {
    for (clif, reg) in [(CMN_I64, "x"), (CMN_I32, "w")] {
        let a = asm(clif);
        assert!(
            a.lines()
                .any(|l| l.trim_start().starts_with(&format!("cmn {reg}"))),
            "want `cmn {reg}?, #64`:\n{a}"
        );
        assert!(
            !a.lines().any(|l| l.trim_start().starts_with("movn ")),
            "no `movn` materialization:\n{a}"
        );
    }
}

#[test]
fn zero_compare_stays_nonzero_path() {
    let a = asm(CMP_ZERO);
    assert!(
        !a.lines().any(|l| l.trim_start().starts_with("cmn ")),
        "`cmp x, 0` must not become `cmn x, #0`:\n{a}"
    );
}

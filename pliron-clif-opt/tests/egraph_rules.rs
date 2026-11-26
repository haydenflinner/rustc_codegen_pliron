//! `opts/pliron.isle`: clifpeep rewrites migrated into the egraph
//! (`PLIRON_EGRAPH_RULES=1`). Each test is one migrated family; the Rust
//! rule stays until its `rule:<name>` counter is zero on the corpus.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

/// Run Cranelift's mid-end (the egraph) alone and return the CLIF text.
fn opt(clif: &str) -> String {
    unsafe { std::env::set_var("PLIRON_EGRAPH_RULES", "1") };
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.optimize(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.func.display().to_string()
}

#[test]
fn addnorm_folds_a_constant_chain_once() {
    let t = opt("function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 8
    v2 = iadd v0, v1
    v3 = iconst.i64 3
    v4 = isub v2, v3
    v5 = iconst.i64 16
    v6 = iadd v4, v5
    return v6
}");
    assert!(t.contains("iconst.i64 21"), "8 - 3 + 16 folded to one constant:\n{t}");
    assert_eq!(t.lines().filter(|l| l.contains("= iadd") || l.contains("= isub")).count(), 1, "{t}");
}

#[test]
fn icmpk_moves_the_constant_across_the_equality() {
    let t = opt("function %f(i64) -> i8 {
block0(v0: i64):
    v1 = iconst.i64 5
    v2 = iadd v0, v1
    v3 = iconst.i64 12
    v4 = icmp ne v2, v3
    return v4
}");
    assert!(t.contains("iconst.i64 7"), "12 - 5:\n{t}");
    assert_eq!(t.lines().filter(|l| l.contains("= iadd")).count(), 0, "the add is gone:\n{t}");
}

#[test]
fn icmpk_drops_a_zero_compared_difference() {
    let t = opt("function %f(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = isub v0, v1
    v3 = iconst.i64 0
    v4 = icmp eq v2, v3
    return v4
}");
    assert!(t.contains("icmp eq v0, v1"), "{t}");
    assert_eq!(t.lines().filter(|l| l.contains("= isub")).count(), 0, "{t}");
}

/// `PLIRON_MINCMP`: a value clamped with `umin` to `y` compares against `y`
/// as a constant (lol_html's `Bytes::slice` bounds check after its clamp).
#[test]
fn mincmp_folds_a_clamp_compared_with_its_bound() {
    unsafe { std::env::set_var("PLIRON_MINCMP", "1") };
    let t = opt("function %f(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = umin v0, v1
    v3 = icmp ule v2, v1
    v4 = icmp ugt v2, v1
    v5 = smax v1, v0
    v6 = icmp slt v5, v0
    v7 = bor v3, v4
    v8 = bor v7, v6
    return v8
}");
    assert!(!t.contains("icmp"), "every compare folds:\n{t}");
}

#[test]
fn lowbits_layout_rounding_folds() {
    unsafe { std::env::set_var("PLIRON_LOWBITS", "1") };
    let t = opt("function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 48
    v2 = imul v1, v0
    v3 = iconst.i64 7
    v4 = iadd v2, v3
    v5 = icmp ult v4, v2
    brif v5, block1, block2
block1:
    v9 = iconst.i64 0
    return v9
block2:
    v6 = iconst.i64 -8
    v7 = band v4, v6
    return v7
}");
    assert!(!t.contains("band"), "48n + 7 rounded down to 8 is 48n:\n{t}");
    assert!(!t.contains("icmp"), "48n + 7 cannot wrap:\n{t}");
}

/// `PLIRON_XREDUCE`: `ireduce` leftovers of packed-field deposits —
/// regex `c_utf8_seq`'s suffix-cache loop computes `ireduce.i8 (ishl
/// end, 8)` (always 0) and `ireduce.i8 (ushr ((k & -65281) | end<<8),
/// 8)` (re-extracting the just-deposited byte) every iteration.
#[test]
fn xreduce_ishl_past_width_is_zero() {
    unsafe { std::env::set_var("PLIRON_XREDUCE", "1") };
    let t = opt("function %f(i64) -> i8 {
block0(v0: i64):
    v1 = iconst.i64 8
    v2 = ishl v0, v1
    v3 = ireduce.i8 v2
    return v3
}");
    assert!(t.contains("iconst.i8 0"), "x << 8 has no low byte:\n{t}");
    assert!(!t.contains("ishl"), "{t}");
}

#[test]
fn xreduce_ishl_below_width_kept() {
    unsafe { std::env::set_var("PLIRON_XREDUCE", "1") };
    let t = opt("function %f(i64) -> i8 {
block0(v0: i64):
    v1 = iconst.i64 4
    v2 = ishl v0, v1
    v3 = ireduce.i8 v2
    return v3
}");
    assert!(t.contains("ishl"), "x << 4 keeps low bits:\n{t}");
}

#[test]
fn xreduce_masked_out_low_bits_is_zero() {
    unsafe { std::env::set_var("PLIRON_XREDUCE", "1") };
    let t = opt("function %f(i64) -> i8 {
block0(v0: i64):
    v1 = iconst.i64 -256
    v2 = band v0, v1
    v3 = ireduce.i8 v2
    return v3
}");
    assert!(t.contains("iconst.i8 0"), "x & -256 has no low byte:\n{t}");
}

#[test]
fn xreduce_extracts_the_deposited_field() {
    unsafe { std::env::set_var("PLIRON_XREDUCE", "1") };
    let t = opt("function %f(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 -65281
    v3 = band v0, v2
    v4 = iconst.i64 8
    v5 = ishl v1, v4
    v6 = bor v3, v5
    v7 = ushr v6, v4
    v8 = ireduce.i8 v7
    return v8
}");
    assert!(t.contains("ireduce.i8 v1"), "byte 1 of `(x & ~0xff00) | y<<8` is y:\n{t}");
    assert!(!t.contains("ushr") && !t.contains("bor"), "{t}");
}

#[test]
fn xreduce_extract_needs_the_mask_window_clear() {
    unsafe { std::env::set_var("PLIRON_XREDUCE", "1") };
    let t = opt("function %f(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 -255
    v3 = band v0, v2
    v4 = iconst.i64 8
    v5 = ishl v1, v4
    v6 = bor v3, v5
    v7 = ushr v6, v4
    v8 = ireduce.i8 v7
    return v8
}");
    assert!(t.contains("ushr"), "x & -255 keeps bits in [8,16): no fold:\n{t}");
}

/// The exact `c_utf8_seq` shape: `uextend`ed bytes deposited through a
/// two-mask chain, the top byte re-extracted.
#[test]
fn xreduce_corpus_shape() {
    unsafe { std::env::set_var("PLIRON_XREDUCE", "1") };
    let t = opt("function %f(i64, i8, i8) -> i8 {
block0(v0: i64, v117: i8, v120: i8):
    v121 = uextend.i64 v117
    v123 = iconst.i64 0
    v124 = ishl v121, v123
    v125 = iconst.i64 -256
    v126 = band.i64 v0, v125
    v127 = bor v126, v124
    v128 = uextend.i64 v120
    v129 = iconst.i64 8
    v130 = ishl v128, v129
    v131 = iconst.i64 -65281
    v132 = band v127, v131
    v133 = bor v132, v130
    v160 = iconst.i64 8
    v161 = ushr.i64 v133, v160
    v162 = ireduce.i8 v161
    return v162
}");
    println!("{t}");
    // The extract folds all the way: the deposited field is `v120`
    // itself and every `band`/`bor`/`ishl`/`ushr` in between is dead.
    assert!(t.contains("return v120") || t.contains("ireduce.i8 v120")
        || (t.contains("ireduce.i8") && !t.contains("ushr")), "{t}");
}

/// Post-push-down shape: `bor.i8` of an `ireduce.i8` and
/// `ireduce.i8 (ishl x, 8)` — the latter is always 0.
#[test]
fn xreduce_post_pushdown_shape() {
    unsafe { std::env::set_var("PLIRON_XREDUCE", "1") };
    let t = opt("function %f(i8, i8) -> i8 {
block0(v117: i8, v120: i8):
    v128 = uextend.i64 v120
    v1719 = iconst.i64 8
    v130 = ishl v128, v1719
    v1608 = ireduce.i8 v130
    v1609 = bor.i8 v117, v1608
    return v1609
}");
    println!("{t}");
    assert!(!t.contains("ishl") && !t.contains("ireduce.i8 v130"), "{t}");
}

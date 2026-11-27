//! aarch64 constants and copies around branches (each knob opt-in, set
//! for this whole test binary): `PLIRON_EDGECONST` (constant branch
//! arguments materialized on their edges), `PLIRON_DUPPARAM` (duplicate
//! block parameters), `PLIRON_AMODENEG` (negative amode offsets as `sub`),
//! `PLIRON_MINMAXIMM` (min/max against an immediate), `PLIRON_ARGPARAM`
//! (entry parameters passed straight on to a call are not copied first).

#![cfg(target_arch = "aarch64")]

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};

fn asm(clif: &str) -> String {
    // The knobs are read once per process; every test here wants them on.
    // SAFETY: set before any compile in this binary, by every test alike.
    unsafe {
        for k in ["PLIRON_EDGECONST", "PLIRON_DUPPARAM", "PLIRON_AMODENEG", "PLIRON_MINMAXIMM", "PLIRON_ARGPARAM"] {
            std::env::set_var(k, "1");
        }
    }
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

/// A `br_table` passing a different tag to one join per arm: the tags
/// are materialized in the arms, not all of them ahead of the table.
const TAGS: &str = "
function %f(i32) -> i32 apple_aarch64 {
block0(v0: i32):
    v1 = iconst.i32 11
    v2 = iconst.i32 22
    v3 = iconst.i32 33
    v4 = iconst.i32 44
    br_table v0, block2(v4), [block2(v1), block2(v2), block2(v3)]
block2(v5: i32):
    v6 = iadd v5, v0
    return v6
}
";

#[test]
fn br_table_tags_on_edges() {
    let a = asm(TAGS);
    let head: Vec<&str> = a.lines().take_while(|l| !l.contains("br ") && !l.contains("b.hs")).collect();
    assert!(
        !head.iter().any(|l| l.contains("#22") || l.contains("#33") || l.contains("#44")),
        "tags materialized ahead of the jump table:\n{a}"
    );
}

/// The regression: a `br_table` with two edges into `block2` (one
/// predecessor) and one into `block3` (two predecessors), all passing
/// `v1`. Counting predecessors instead of edges moved `v1` onto the edge
/// into `block3`, left the two into `block2` alone and removed `v1` anyway
/// (`regs.is_valid()` in the lowering, unicase's `lookup`).
const SHARED: &str = "
function %f(i32) -> i32 apple_aarch64 {
block0(v0: i32):
    v1 = iconst.i32 105
    v2 = iconst.i32 7
    br_table v0, block1, [block2(v1), block2(v2), block3(v1)]
block1:
    v9 = iconst.i32 1
    jump block3(v9)
block2(v3: i32):
    v4 = iadd v3, v0
    return v4
block3(v5: i32):
    v6 = isub v5, v0
    return v6
}
";

#[test]
fn constant_kept_for_an_edge_left_alone() {
    let a = asm(SHARED);
    assert!(a.contains("#105"), "the constant is gone:\n{a}");
}

/// Two parameters fed the same value on every edge are one register.
const DUP: &str = "
function %f(i64, i64, i8) -> i64 apple_aarch64 {
block0(v0: i64, v1: i64, v2: i8):
    brif v2, block1(v0, v0), block1(v1, v1)
block1(v3: i64, v4: i64):
    v5 = imul v3, v4
    return v5
}
";

#[test]
fn duplicate_params_merge() {
    let a = asm(DUP);
    let mul = a.lines().find(|l| l.trim_start().starts_with("mul") || l.trim_start().starts_with("madd")).unwrap_or("");
    let ops: Vec<&str> = mul.split(|c| c == ' ' || c == ',').filter(|s| s.starts_with('x')).collect();
    assert!(ops.len() >= 3 && ops[1] == ops[2], "parameters not merged: {mul}\n{a}");
}

/// `base - 4 + index`: one `sub` of the immediate, no materialized `-4`.
const NEG: &str = "
function %f(i64, i64) -> i32 apple_aarch64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 -4
    v3 = iadd v0, v2
    v4 = iadd v3, v1
    v5 = load.i32 notrap v4
    return v5
}
";

#[test]
fn negative_offset_is_sub() {
    let a = asm(NEG);
    assert!(!a.contains("movn") && !a.contains("#-4"), "offset materialized:\n{a}");
}

/// `smax x, 0` compares against the immediate and selects `xzr`.
const SMAX: &str = "
function %f(i64) -> i64 apple_aarch64 {
block0(v0: i64):
    v1 = iconst.i64 0
    v2 = smax v0, v1
    return v2
}
";

#[test]
fn smax_zero_uses_xzr() {
    let a = asm(SMAX);
    assert!(a.contains("xzr") && !a.contains("movz"), "zero materialized:\n{a}");
}

/// A wrapper passing `(0, a, b, c, d)` on: one parallel move, no scratch
/// registers, `x0` written once (it was 9 moves with a dead `mov x0`).
const SHIFT: &str = "
function %f(i64, i64, i64, i64) -> i64, i64 apple_aarch64 {
    sig0 = (i64, i64, i64, i64, i64) -> i64, i64 apple_aarch64
    fn0 = colocated %g sig0
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v5 = iconst.i64 0
    v6, v7 = call fn0(v5, v0, v1, v2, v3)
    return v6, v7
}
";

#[test]
fn forwarded_params_one_parallel_move() {
    let a = asm(SHIFT);
    // `orr xN, xzr, xzr` is PLIRON_ZRMOV's zero materialization — a `mov`
    // for the purposes of this count.
    let movs: Vec<&str> = a
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.starts_with("mov x") || l.starts_with("movz x") || l.starts_with("orr x")
        })
        .collect();
    let x0 = movs
        .iter()
        .filter(|l| l.starts_with("mov x0,") || l.starts_with("movz x0,") || l.starts_with("orr x0,"))
        .count();
    assert!(movs.len() <= 6 && x0 == 1, "argument shuffle:\n{a}");
}

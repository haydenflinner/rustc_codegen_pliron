//! divconst: `sdiv`/`udiv`/`srem`/`urem` by constants → magic-multiply +
//! shift sequences, differentially checked against the interpreter on
//! random inputs (biased to 0/±1/min/max — exactly the edges that break
//! magic-division fixups).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Function, InstructionData, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn no_divs(f: &Function) -> bool {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .all(|i| {
            !matches!(
                f.dfg.insts[i].opcode(),
                Opcode::Sdiv | Opcode::Srem | Opcode::Udiv | Opcode::Urem
            )
        })
}

fn check_one(op: &str, ty: &str, d: i64, trials: u32) {
    let src = format!(
        "function %f({ty}) -> {ty} {{\nblock0(v0: {ty}):\n    v1 = iconst.{ty} {d}\n    v2 = {op}.{ty} v0, v1\n    return v2\n}}"
    );
    let before = parse(&src);
    let mut after = before.clone();
    let k = pliron_clif_opt::divconst::run(&mut after);
    assert!(k > 0, "no expansion: {op}.{ty} by {d}");
    assert!(no_divs(&after), "div left: {op}.{ty} by {d}");
    let cfg = Config {
        trials,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{op}.{ty} by {d}: {m}\n---- after\n{}", after.display());
    }
}

/// The vector `mulhi` emission loopvec uses for widened lanes: widen +
/// imul + odd-chunk shuffle. (Div/rem themselves are scalar-only in CLIF.)
fn check_vec_mulhi(op: &str, ty: &str, signed: bool, trials: u32) {
    let src = format!(
        "function %f({ty}, {ty}) -> {ty} {{\nblock0(v0: {ty}, v1: {ty}):\n    v2 = {op}.{ty} v0, v1\n    return v2\n}}"
    );
    let before = parse(&src);
    let mut after = before.clone();
    let mut hits = Vec::new();
    for b in after.layout.blocks() {
        for i in after.layout.block_insts(b) {
            if let InstructionData::Binary { opcode, args } = after.dfg.insts[i] {
                if matches!(opcode, Opcode::Smulhi | Opcode::Umulhi) {
                    hits.push((i, args[0], args[1]));
                }
            }
        }
    }
    assert!(!hits.is_empty());
    let mut pos = FuncCursor::new(&mut after);
    for (i, a, b) in hits {
        pos.goto_inst(i);
        let res = pos.func.dfg.first_result(i);
        let ty = pos.func.dfg.value_type(res);
        let v = pliron_clif_opt::divconst::mulhi(&mut pos, ty, signed, a, b);
        pos.func.dfg.clear_results(i);
        pos.func.layout.remove_inst(i);
        pos.func.dfg.change_to_alias(res, v);
    }
    let cfg = Config {
        trials,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{op}.{ty}: {m}\n---- after\n{}", after.display());
    }
}

#[test]
fn sdiv_i8_all() {
    for d in -128i64..=127 {
        if d != 0 {
            check_one("sdiv", "i8", d, 128);
        }
    }
}

#[test]
fn srem_i8_all() {
    for d in -128i64..=127 {
        if d != 0 {
            check_one("srem", "i8", d, 128);
        }
    }
}

#[test]
fn unsigned_i8_all() {
    for d in 1u32..=255 {
        check_one("udiv", "i8", d as i64, 128);
        check_one("urem", "i8", d as i64, 128);
    }
}

#[test]
fn mixed_widths() {
    // pow2, magic, add-indicator (7 is the canonical u32 is_add case),
    // sign-mismatched magic (25), negatives, INT_MIN/INT_MAX edges.
    let ds = [
        -128, -100, -27, -16, -9, -7, -5, -4, -3, -2, -1, 2, 3, 4, 5, 6, 7, 8, 10, 14, 16, 25, 37,
        63, 64, 100, 101, 127, 128,
    ];
    for ty in ["i16", "i32", "i64"] {
        for &d in &ds {
            for op in ["sdiv", "srem", "udiv", "urem"] {
                check_one(op, ty, d, 64);
            }
        }
    }
    // Width-specific extremes: i64 divisors needing the 65-bit magic.
    for d in [i32::MAX as i64, i32::MIN as i64, i64::MAX, i64::MIN] {
        for op in ["sdiv", "srem"] {
            check_one(op, "i64", d, 64);
        }
    }
    for d in [
        u32::MAX as i64,
        0x8000_0000u32 as i64,
        u64::MAX as i64,
        i64::MIN + 1,
    ] {
        for op in ["udiv", "urem"] {
            check_one(op, "i64", d, 64);
        }
    }
    for d in [i16::MAX as i64, i16::MIN as i64] {
        check_one("sdiv", "i16", d, 64);
        check_one("urem", "i16", d, 64);
    }
}

#[test]
fn vector_mulhi() {
    check_vec_mulhi("smulhi", "i32x4", true, 128);
    check_vec_mulhi("umulhi", "i32x4", false, 128);
    check_vec_mulhi("smulhi", "i16x8", true, 128);
    check_vec_mulhi("umulhi", "i8x16", false, 128);
}

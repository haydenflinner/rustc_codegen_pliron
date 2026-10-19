//! divconst: `sdiv`/`udiv`/`srem`/`urem` by constants → magic-multiply +
//! shift sequences, differentially checked against the interpreter on
//! random inputs (biased to 0/±1/min/max — exactly the edges that break
//! magic-division fixups).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Function, InstBuilder, InstructionData, Opcode, types};
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

/// i128 `udiv`/`urem` by a constant — the libcall (`__udivti3`) case. The
/// divisor goes in as `iconcat`d i64 limbs: the form lowering actually
/// emits (`iconst.i128` has no isel rule and the reader rejects it).
fn check_one_128(op: &str, d: u128, trials: u32) {
    use cranelift_codegen::ir::{AbiParam, Signature, UserFuncName, types};
    use cranelift_codegen::isa::CallConv;
    let mut f = Function::with_name_signature(
        UserFuncName::user(0, 0),
        Signature::new(CallConv::SystemV),
    );
    f.signature.params.push(AbiParam::new(types::I128));
    f.signature.returns.push(AbiParam::new(types::I128));
    let b = f.dfg.make_block();
    f.layout.append_block(b);
    let v0 = f.dfg.append_block_param(b, types::I128);
    {
        let mut pos = FuncCursor::new(&mut f).at_bottom(b);
        let lo = pos.ins().iconst(types::I64, d as u64 as i64);
        let hi = pos.ins().iconst(types::I64, (d >> 64) as u64 as i64);
        let dc = pos.ins().iconcat(lo, hi);
        let v = match op {
            "udiv" => pos.ins().udiv(v0, dc),
            "urem" => pos.ins().urem(v0, dc),
            _ => unreachable!(),
        };
        pos.ins().return_(&[v]);
    }
    let before = f.clone();
    let k = pliron_clif_opt::divconst::run(&mut f);
    assert_eq!(k, 1, "no expansion: {op}.i128 by {d:#x}");
    assert!(no_divs(&f), "div left: {op}.i128 by {d:#x}");
    let cfg = Config {
        trials,
        ..Config::default()
    };
    if let Err(m) = check(&before, &f, &Facts::default(), &cfg) {
        panic!("{op}.i128 by {d:#x}: {m}\n---- after\n{}", f.display());
    }
}

#[test]
fn unsigned_i128() {
    // pow2 edges, small magics, the 2^64 limb boundary, and extremes whose
    // magic fills both limbs.
    for d in [
        1u128,
        2,
        3,
        5,
        7,
        8,
        97,
        100,
        10u128.pow(18),
        u64::MAX as u128,
        1u128 << 64,
        (1u128 << 64) + 1,
        u64::MAX as u128 + 2,
        (1u128 << 127) - 1,
        1u128 << 127,
        u128::MAX,
    ] {
        check_one_128("udiv", d, 64);
        check_one_128("urem", d, 64);
    }
}

/// `PLIRON_DIVNARROW`: an edge-fact-bounded i64 dividend (`brif
/// (uge i, 1e8)` → `i <= 1e8 - 1` on the else edge) takes the i32
/// expansion — the emitter's `umulhi.i64` synthesis is ~4x the i32
/// path's size on targets without a native mulhi (wasm).
#[test]
fn narrow_bounded_i64_dividend() {
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 100000000
    v2 = icmp.i64 uge v0, v1
    brif v2, block3, block2
block2:
    v3 = iconst.i64 5
    v4 = urem.i64 v0, v3
    return v4
block3:
    v5 = iconst.i64 0
    return v5
}";
    let before = parse(src);
    let mut f = before.clone();
    let k = pliron_clif_opt::divconst::run(&mut f);
    assert_eq!(k, 1, "no expansion:\n{}", f.display());
    assert!(no_divs(&f), "rem left: {}", f.display());
    let (mut hi32, mut hi64) = (0, 0);
    let (mut red, mut ext) = (0, 0);
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            match f.dfg.insts[i] {
                InstructionData::Binary {
                    opcode: Opcode::Umulhi,
                    ..
                } => {
                    if f.dfg.value_type(f.dfg.first_result(i)) == types::I32 {
                        hi32 += 1;
                    } else {
                        hi64 += 1;
                    }
                }
                InstructionData::Unary { opcode, .. } => match opcode {
                    Opcode::Ireduce => red += 1,
                    Opcode::Uextend => ext += 1,
                    _ => {}
                },
                _ => {}
            }
        }
    }
    assert_eq!(hi64, 0, "umulhi.i64 left: {}", f.display());
    assert_eq!(hi32, 1, "no umulhi.i32: {}", f.display());
    assert_eq!(red, 1, "no ireduce: {}", f.display());
    assert_eq!(ext, 1, "no uextend: {}", f.display());
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &f, &Facts::default(), &cfg) {
        panic!("narrow: {m}\n---- after\n{}", f.display());
    }
}

/// Unbounded dividends keep the i64 expansion — the bound must be
/// proven, not assumed.
#[test]
fn narrow_keeps_unbounded_i64() {
    let src = "function %f(i64) -> i64 {
block0(v0: i64):
    v3 = iconst.i64 5
    v4 = urem.i64 v0, v3
    return v4
}";
    let before = parse(src);
    let mut f = before.clone();
    let k = pliron_clif_opt::divconst::run(&mut f);
    assert_eq!(k, 1, "no expansion:\n{}", f.display());
    assert!(no_divs(&f));
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if let InstructionData::Binary {
                opcode: Opcode::Umulhi,
                ..
            } = f.dfg.insts[i]
            {
                assert_eq!(
                    f.dfg.value_type(f.dfg.first_result(i)),
                    types::I64,
                    "narrowed without a bound: {}",
                    f.display()
                );
            }
        }
    }
    if let Err(m) = check(&before, &f, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", f.display());
    }
}

/// Regression: `isub a, b` in the dividend chain wraps to ~2^64 when
/// `b > a`, so `a`'s edge-fact bound does not carry to the difference.
/// `bound_at` used to return `bound(a)` and divnarrow truncated the
/// wrapped dividend at i32 (corpus `core::ptr::align_offset`).
#[test]
fn narrow_rejects_isub_wrap_dividend() {
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 10
    v3 = icmp.i64 ult v0, v2
    brif v3, block2, block3
block2:
    v4 = isub.i64 v0, v1
    v5 = iconst.i64 5
    v6 = urem.i64 v4, v5
    return v6
block3:
    v7 = iconst.i64 0
    return v7
}";
    let before = parse(src);
    let mut f = before.clone();
    let k = pliron_clif_opt::divconst::run(&mut f);
    assert_eq!(k, 1, "no expansion:\n{}", f.display());
    assert!(no_divs(&f));
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if let InstructionData::Unary {
                opcode: Opcode::Ireduce,
                ..
            } = f.dfg.insts[i]
            {
                panic!("narrowed a wrapping dividend: {}", f.display());
            }
        }
    }
    if let Err(m) = check(&before, &f, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", f.display());
    }
}

#[test]
fn vector_mulhi() {
    check_vec_mulhi("smulhi", "i32x4", true, 128);
    check_vec_mulhi("umulhi", "i32x4", false, 128);
    check_vec_mulhi("smulhi", "i16x8", true, 128);
    check_vec_mulhi("umulhi", "i8x16", false, 128);
}

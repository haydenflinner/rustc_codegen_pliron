//! `wide128::run` (`PLIRON_WIDE128=1`): 128-bit operations on word halves.
//! Shapes: foldhash's `folded_multiply` (`u64 x u64 -> u128`, then
//! `lo ^ hi`), its sponge (`s | (x << len)` with `len` 0 and 64, the mask
//! `len & 127` still unfolded), `(x >> 64) as u64`, `isplit` of a built
//! value, and two that must stay: a multiply whose high half is not known
//! zero and a shift by 32. The checker runs every rewritten function
//! against its original.

use cranelift_codegen::ir::{Function, InstructionData, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::wide128;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

fn run(src: &str) -> (Function, usize) {
    let before = parse(src);
    let mut after = before.clone();
    let n = wide128::run(&mut after, true);
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("invalid after wide128: {e}\n{after}"));
    check(&before, &after, &Facts::default(), &Config { trials: 64, ..Config::default() })
        .unwrap_or_else(|m| panic!("wide128 changed behaviour: {m:?}\n{after}"));
    (after, n)
}

/// `folded_multiply(a, b) = (a as u128 * b as u128) as u64 ^ ((.. >> 64) as u64)`.
#[test]
fn folded_multiply() {
    let src = "function %f(i64, i64) -> i64 {
    block0(v0: i64, v1: i64):
        v2 = uextend.i128 v0
        v3 = uextend.i128 v1
        v4 = imul v2, v3
        v5 = ireduce.i64 v4
        v6 = iconst.i8 64
        v7 = uextend.i128 v6
        v8 = ushr v4, v7
        v9 = ireduce.i64 v8
        v10 = bxor v5, v9
        return v10
    }";
    let (after, n) = run(src);
    assert!(n >= 4, "{after}");
    let live: Vec<_> = after
        .layout
        .blocks()
        .flat_map(|b| after.layout.block_insts(b))
        .filter(|&i| after.dfg.insts[i].opcode() == Opcode::Umulhi)
        .collect();
    assert_eq!(live.len(), 1, "{after}");
}

/// Widening overflow check: the product is an `i128` addend. Splitting
/// it would leave `umulhi` live across the still-wide add.
#[test]
fn imul_feeding_iadd_stays() {
    let src = "function %f(i64, i64, i128) -> i128 {
    block0(v0: i64, v1: i64, v2: i128):
        v3 = uextend.i128 v0
        v4 = uextend.i128 v1
        v5 = imul v3, v4
        v6 = iadd v5, v2
        return v6
    }";
    let (after, n) = run(src);
    assert_eq!(n, 0, "{after}");
    assert_eq!(count(&after, Opcode::Umulhi), 0, "{after}");
    assert_eq!(count(&after, Opcode::Uextend), 2, "{after}");
    assert_eq!(count(&after, Opcode::Imul), 1, "{after}");
}

/// The sponge: `s = 0; s |= a << (0 & 127); s |= b << (64 & 127)`, then
/// the halves.
#[test]
fn sponge() {
    let src = "function %f(i64, i64) -> i64, i64 {
    block0(v0: i64, v1: i64):
        v2 = iconst.i64 0
        v3 = iconcat v2, v2
        v4 = uextend.i128 v0
        v5 = iconst.i8 0
        v6 = iconst.i8 127
        v7 = band v5, v6
        v8 = uextend.i128 v7
        v9 = ishl v4, v8
        v10 = bor v3, v9
        v11 = uextend.i128 v1
        v12 = iconst.i8 64
        v13 = band v12, v6
        v14 = uextend.i128 v13
        v15 = ishl v11, v14
        v16 = bor v10, v15
        v17, v18 = isplit v16
        return v17, v18
    }";
    let (after, n) = run(src);
    assert!(n >= 5, "{after}");
    // The `isplit` is gone: its halves are the inputs.
    assert_eq!(count(&after, Opcode::Isplit), 0, "{after}");
}

#[test]
fn unknown_high_half_and_odd_shift_stay() {
    let src = "function %f(i64, i64, i64) -> i64, i64 {
    block0(v0: i64, v1: i64, v2: i64):
        v3 = iconcat v0, v1
        v4 = uextend.i128 v2
        v5 = imul v3, v4
        v6 = iconst.i8 32
        v7 = ishl v5, v6
        v8, v9 = isplit v7
        return v8, v9
    }";
    let (after, _) = run(src);
    assert_eq!(count(&after, Opcode::Umulhi), 0, "{after}");
    assert!(after
        .layout
        .blocks()
        .flat_map(|b| after.layout.block_insts(b))
        .any(|i| matches!(after.dfg.insts[i], InstructionData::Binary { opcode: Opcode::Ishl, .. })));
}

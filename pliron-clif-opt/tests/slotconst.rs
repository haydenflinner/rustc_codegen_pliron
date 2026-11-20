//! `slotconst::run` (`PLIRON_SLOTCONST=1`): a load from a stack slot whose
//! bytes constant stores in the same block just wrote becomes the
//! constant. Shapes: stdarch's NEON zero built byte by byte and reloaded
//! as `i8x8` (hashbrown's group probe), a scalar reload of two half
//! stores, a store of a non-constant in between (forgets its bytes), a
//! call in between (forgets everything), a store through another pointer
//! (forgets everything) and a load of a byte never stored (stays). The
//! checker runs every rewritten function against its original.

use cranelift_codegen::ir::{Function, InstructionData, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::slotconst;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::Load { opcode: Opcode::Load, .. }))
        .count()
}

fn run(src: &str) -> (Function, usize) {
    let before = parse(src);
    let mut after = before.clone();
    let n = slotconst::run(&mut after);
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("invalid after slotconst: {e}\n{after}"));
    check(&before, &after, &Facts::default(), &Config { trials: 64, ..Config::default() })
        .unwrap_or_else(|m| panic!("slotconst changed behaviour: {m:?}\n{after}"));
    (after, n)
}

#[test]
fn neon_zero_through_memory() {
    let src = "function %f(i64) -> i64 {
        ss0 = explicit_slot 8, align = 8
    block0(v0: i64):
        v1 = stack_addr.i64 ss0
        v2 = iconst.i8 0
        store notrap v2, v1
        store notrap v2, v1+1
        v3 = iconst.i64 2
        v4 = iadd v1, v3
        store notrap v2, v4
        store notrap v2, v4+1
        store notrap v2, v1+4
        store notrap v2, v1+5
        store notrap v2, v1+6
        store notrap v2, v1+7
        v5 = load.i8x8 notrap v1
        v6 = bitcast.i8x8 little v0
        v7 = icmp slt v6, v5
        v8 = bitcast.i64 little v7
        return v8
    }";
    let (after, n) = run(src);
    assert_eq!(n, 1, "{after}");
    assert_eq!(loads(&after), 0);
}

#[test]
fn scalar_from_halves() {
    let src = "function %f(i32) -> i32 {
        ss0 = explicit_slot 4
    block0(v0: i32):
        v1 = stack_addr.i64 ss0
        v2 = iconst.i16 0x1234
        v3 = iconst.i16 -2
        store notrap v2, v1
        store notrap v3, v1+2
        v4 = load.i32 notrap v1
        v5 = iadd v4, v0
        return v5
    }";
    let (after, n) = run(src);
    assert_eq!(n, 1, "{after}");
}

#[test]
fn non_constant_store_forgets() {
    let src = "function %f(i8) -> i16 {
        ss0 = explicit_slot 2
    block0(v0: i8):
        v1 = stack_addr.i64 ss0
        v2 = iconst.i8 7
        store notrap v2, v1
        store notrap v2, v1+1
        store notrap v0, v1+1
        v3 = load.i16 notrap v1
        return v3
    }";
    let (_, n) = run(src);
    assert_eq!(n, 0);
}

#[test]
fn foreign_store_forgets() {
    let src = "function %f(i64, i8) -> i8 {
        ss0 = explicit_slot 1
    block0(v0: i64, v1: i8):
        v2 = stack_addr.i64 ss0
        v3 = iconst.i8 7
        store notrap v3, v2
        store notrap v1, v0
        v4 = load.i8 notrap v2
        return v4
    }";
    let (_, n) = run(src);
    assert_eq!(n, 0);
}

#[test]
fn unwritten_byte_stays() {
    let src = "function %f() -> i16 {
        ss0 = explicit_slot 2
    block0:
        v1 = stack_addr.i64 ss0
        v2 = iconst.i8 7
        store notrap v2, v1
        v3 = load.i16 notrap v1
        return v3
    }";
    let (_, n) = run(src);
    assert_eq!(n, 0);
}

fn run_comb(src: &str) -> (Function, usize) {
    let before = parse(src);
    let mut after = before.clone();
    let n = slotconst::run_with(&mut after, true);
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("invalid after slotcomb: {e}\n{after}"));
    check(&before, &after, &Facts::default(), &Config { trials: 64, ..Config::default() })
        .unwrap_or_else(|m| panic!("slotcomb changed behaviour: {m:?}\n{after}"));
    (after, n)
}

/// `PLIRON_SLOTCOMB`: brotli's `Union1::shortcut(x)`: a payload and the tag
/// `2` stored as two words, reloaded as one `i64`.
#[test]
fn comb_enum_word() {
    let (after, n) = run_comb("function %f(i32, i64) {
        ss0 = explicit_slot 8, align = 8
    block0(v0: i32, v1: i64):
        v2 = stack_addr.i64 ss0
        store notrap v0, v2+4
        v3 = iconst.i32 2
        store notrap v3, v2
        v4 = load.i64 notrap v2
        store notrap v4, v1
        return
    }");
    assert_eq!(n, 1, "{after}");
    assert_eq!(loads(&after), 0, "{after}");
}

/// Mixed widths: an `i8` constant, an `i16` value and an `istore8` of an
/// `i32` would make one `i32` from three terms: over the two-term cap
/// (each further piece costs a shift and an OR), so the load stays.
#[test]
fn comb_three_terms_stay() {
    let (after, n) = run_comb("function %f(i16, i32) -> i32 {
        ss0 = explicit_slot 4, align = 4
    block0(v0: i16, v1: i32):
        v2 = stack_addr.i64 ss0
        v3 = iconst.i8 7
        store notrap v3, v2
        store notrap v0, v2+1
        istore8 notrap v1, v2+3
        v4 = load.i32 notrap v2
        return v4
    }");
    assert_eq!(n, 0, "{after}");
    assert_eq!(loads(&after), 1, "{after}");
}

/// Two pieces of different widths: an `istore8` of an `i32` (its low byte)
/// and an `i8` value at offset 1, read as one `i16`.
#[test]
fn comb_two_pieces() {
    let (after, n) = run_comb("function %f(i32, i8) -> i16 {
        ss0 = explicit_slot 2, align = 2
    block0(v0: i32, v1: i8):
        v2 = stack_addr.i64 ss0
        istore8 notrap v0, v2
        store notrap v1, v2+1
        v4 = load.i16 notrap v2
        return v4
    }");
    assert_eq!(n, 1, "{after}");
    assert_eq!(loads(&after), 0, "{after}");
}

/// A byte no store covered: the load stays.
#[test]
fn comb_uncovered_byte_stays() {
    let (after, n) = run_comb("function %f(i16) -> i32 {
        ss0 = explicit_slot 4, align = 4
    block0(v0: i16):
        v2 = stack_addr.i64 ss0
        store notrap v0, v2
        v3 = iconst.i8 1
        store notrap v3, v2+2
        v4 = load.i32 notrap v2
        return v4
    }");
    assert_eq!(n, 0, "{after}");
    assert_eq!(loads(&after), 1, "{after}");
}

/// A piece that reaches past the load (its upper half is read, the rest is
/// not): not combined (a single-store forward is `loadfwd`'s job).
#[test]
fn comb_piece_past_the_load_stays() {
    let (after, n) = run_comb("function %f(i32, i16) -> i32 {
        ss0 = explicit_slot 8, align = 8
    block0(v0: i32, v1: i16):
        v2 = stack_addr.i64 ss0
        store notrap v1, v2
        store notrap v0, v2+2
        v4 = load.i32 notrap v2
        return v4
    }");
    assert_eq!(n, 0, "{after}");
    assert_eq!(loads(&after), 1, "{after}");
}

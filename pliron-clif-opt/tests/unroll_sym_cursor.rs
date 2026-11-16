//! Full unroll of a pointer loop over a fixed-size array (`for b in
//! arr.iter_mut()`): the cursor starts at `arr`, steps by the element
//! size and stops at `arr + N * size`. Neither end is a constant, but
//! both are the same outside value plus a constant, so `p == end` is
//! decided by the offsets (`unroll::run_with(.., symon = true)`).
//! nbody's position update is this loop; LLVM unrolls it from the
//! pointer-difference trip count.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::pipeline::Analysis;

/// `end = v0 + end_off`, cursor from `v0` by 8.
fn src(end_off: i64, end_base: &str) -> String {
    format!(
        "function u0:0(i64, i64) fast {{
block0(v0: i64, v1: i64):
    v2 = iconst.i64 {end_off}
    v3 = iadd {end_base}, v2
    jump block1(v0)

block1(v5: i64):
    v6 = load.i64 notrap v5
    v7 = iconst.i64 1
    v8 = iadd v6, v7
    store.i64 notrap v8, v5
    v9 = iconst.i64 8
    v10 = iadd v5, v9
    v11 = icmp eq v10, v3
    brif v11, block2, block1(v10)

block2:
    return
}}"
    )
}

fn count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

fn unroll(src: &str, symon: bool) -> (Function, usize) {
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let n = pliron_clif_opt::unroll::run_with(
        &mut f,
        &mut Analysis::default(),
        &rustc_hash::FxHashSet::default(),
        &pliron_clif_opt::vmap::VMap::default(),
        &std::collections::HashMap::default(),
        symon,
    );
    pliron_clif_opt::jumpthread::remove_unreachable_blocks(&mut f);
    (f, n)
}

/// Five elements: five straight-line copies, no branch left.
#[test]
fn fixed_array_cursor_unrolls() {
    let (f, n) = unroll(&src(40, "v0"), true);
    assert_eq!(n, 1);
    assert_eq!(count(&f, Opcode::Store), 5);
    assert_eq!(count(&f, Opcode::Brif), 0);
}

/// Without the symbolic cursor the exit is undecided.
#[test]
fn off_by_default() {
    let (f, n) = unroll(&src(40, "v0"), false);
    assert_eq!(n, 0);
    assert_eq!(count(&f, Opcode::Brif), 1);
}

/// The end hangs off another value: nothing is known about the trip.
#[test]
fn other_base_stays_a_loop() {
    let (f, n) = unroll(&src(40, "v1"), true);
    assert_eq!(n, 0);
    assert_eq!(count(&f, Opcode::Brif), 1);
}

/// An end the cursor never lands on (not a multiple of the step) would
/// walk past it: the trip never decides within the visit cap, so the
/// loop stays.
#[test]
fn misaligned_end_stays_a_loop() {
    let (f, n) = unroll(&src(41, "v0"), true);
    assert_eq!(n, 0);
    assert_eq!(count(&f, Opcode::Brif), 1);
}

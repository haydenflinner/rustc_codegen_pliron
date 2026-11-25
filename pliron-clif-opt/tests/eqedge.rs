//! `eqedge`: on the edge where `icmp eq/ne` decides `v == K`, block args
//! carrying the constant pass `v` instead — regalloc coalesces what would
//! otherwise be a forced `mov rd, #K`.

use cranelift_codegen::ir::{BlockArg, Function, InstructionData, Opcode, Value};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// Args of the function's `brif` on destination `di` (0 taken, 1 not).
fn edge_args(func: &Function, di: usize) -> Vec<Value> {
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { blocks, .. } = func.dfg.insts[t] else {
            continue;
        };
        return blocks[di]
            .args(&func.dfg.value_lists)
            .map(|a| match a {
                BlockArg::Value(v) => v,
                _ => panic!("non-value arg"),
            })
            .collect();
    }
    panic!("no brif")
}

fn is_const(func: &Function, v: Value, k: i64) -> bool {
    let Some(i) = func.dfg.value_def(v).inst() else {
        return false;
    };
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if imm.bits() == k
    )
}

fn entry_param(f: &Function) -> Value {
    f.dfg.block_params(f.layout.entry_block().unwrap())[0]
}

#[test]
fn eq_taken_edge_swaps_constant_for_compared() {
    let mut f = parse(
        "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 3
    v2 = icmp eq v0, v1
    brif v2, block1(v1, v0), block2(v1)
block1(v3: i64, v4: i64):
    v5 = iadd v3, v4
    return v5
block2(v6: i64):
    return v6
}",
    );
    assert_eq!(pliron_clif_opt::eqedge::run(&mut f), 1);
    // Taken edge: the `3` arg now passes v0; the compared v0 stays.
    assert_eq!(edge_args(&f, 0), [entry_param(&f); 2]);
    // Not-taken edge untouched: still the constant.
    assert!(is_const(&f, edge_args(&f, 1)[0], 3));
}

#[test]
fn ne_fallthrough_edge_is_the_equality_edge() {
    let mut f = parse(
        "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 7
    v2 = icmp ne v0, v1
    brif v2, block2(v1), block1(v1)
block1(v3: i64):
    return v3
block2(v4: i64):
    return v4
}",
    );
    assert_eq!(pliron_clif_opt::eqedge::run(&mut f), 1);
    // `ne` not-taken (v0 == 7) edge: constant becomes v0.
    assert_eq!(edge_args(&f, 1), [entry_param(&f)]);
    // Taken edge keeps the constant.
    assert!(is_const(&f, edge_args(&f, 0)[0], 7));
}

#[test]
fn non_equality_edges_and_mismatched_types_untouched() {
    let mut f = parse(
        "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 3
    v2 = icmp ult v0, v1
    brif v2, block1(v1), block2(v1)
block1(v3: i64):
    return v3
block2(v4: i64):
    return v4
}",
    );
    assert_eq!(pliron_clif_opt::eqedge::run(&mut f), 0);
    assert!(is_const(&f, edge_args(&f, 0)[0], 3));

    // i32 compared value against an i64 param slot: no substitution.
    let mut f = parse(
        "function %f(i32) -> i64 {
block0(v0: i32):
    v1 = iconst.i32 3
    v9 = iconst.i64 3
    v2 = icmp eq v0, v1
    brif v2, block1(v9), block2(v9)
block1(v3: i64):
    return v3
block2(v4: i64):
    return v4
}",
    );
    assert_eq!(pliron_clif_opt::eqedge::run(&mut f), 0);
    assert!(is_const(&f, edge_args(&f, 0)[0], 3));
}

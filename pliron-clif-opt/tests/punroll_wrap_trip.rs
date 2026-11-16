//! punroll must not delete a loop on `trip_max`'s estimate. `while i != 0
//! { i = i.wrapping_add(2) }` from an odd `i` never ends (Rust defines
//! the infinite loop), but the estimate treats the unknown start as 0
//! and the bound 0 as "at most one trip": the peel's `full_unroll` then
//! replaced the loop by one copy that falls through to the exit
//! (`tests/willret`: the call returned). Only `trip_max_exact` may drop
//! the residual loop. The same loop with a step of one from a constant
//! start to a constant bound is still unrolled fully.

use cranelift_codegen::ir::{Function, InstructionData};
use pliron_clif_opt::pipeline::Analysis;

/// The shape `looprot` leaves: guard on entry, do-while latch.
fn src(start: &str, step: i64, bound: i64) -> String {
    format!(
        "function u0:0(i32) -> i32 fast {{
block0(v0: i32):
    v3 = iconst.i32 {bound}
    v6 = iconst.i32 {step}
    v1 = {start}
    v4 = icmp eq v1, v3
    brif v4, block2, block3(v1)

block3(v23: i32):
    v7 = iadd v23, v6
    v24 = icmp ne v7, v3
    brif v24, block3(v7), block2

block2:
    v9 = iconst.i32 0
    return v9
}}"
    )
}

/// Some block still branches back to itself or an earlier block.
fn has_backedge(f: &Function) -> bool {
    let order: Vec<_> = f.layout.blocks().collect();
    let pos = |b| order.iter().position(|&x| x == b).unwrap();
    order.iter().any(|&b| {
        let Some(t) = f.layout.last_inst(b) else { return false };
        match f.dfg.insts[t] {
            InstructionData::Brif { blocks, .. } => {
                blocks.iter().any(|bc| pos(bc.block(&f.dfg.value_lists)) <= pos(b))
            }
            InstructionData::Jump { destination, .. } => {
                pos(destination.block(&f.dfg.value_lists)) <= pos(b)
            }
            _ => false,
        }
    })
}

fn unroll(src: &str) -> Function {
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::punroll::run(&mut f, &mut Analysis::default(), "t", &Default::default(), false);
    pliron_clif_opt::jumpthread::remove_unreachable_blocks(&mut f);
    f
}

/// Unknown start, step 2, `!=` bound: the counter can skip the bound and
/// wrap for ever. The loop stays.
#[test]
fn wrapping_ne_loop_keeps_its_loop() {
    let f = unroll(&src("ineg v0", 2, 0));
    assert!(has_backedge(&f), "the loop was deleted:\n{}", f.display());
}

/// Constant start whose distance the step does not divide: same.
#[test]
fn undivided_const_distance_keeps_its_loop() {
    let f = unroll(&src("iconst.i32 1", 2, 4));
    assert!(has_backedge(&f), "the loop was deleted:\n{}", f.display());
}

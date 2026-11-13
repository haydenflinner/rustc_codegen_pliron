//! `bumpedge`: a block param that its block only steps is stepped on the
//! edges into the block instead, next to the load of the same pointer.
//! The rotated state loop of regex's `Fsm::next_state`
//! (`tests/parity/prog_loop.rs`). Differentially checked.

use cranelift_codegen::ir::{Block, Function, InstructionData, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

// block1 is the peeled first read, block2 the loop's top (steps the
// cursor, sums), block3 the exit test, block4 the next read.
const LOOP: &str = "function %t(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    v4 = icmp eq v0, v1
    brif v4, block6(v3), block1
block1:
    v5 = load.i64 notrap v0
    v6 = icmp ult v5, v2
    brif v6, block2(v0, v5, v3), block5
block2(v7: i64, v8: i64, v9: i64):
    v10 = iconst.i64 8
    v11 = iadd v7, v10
    v12 = iadd v9, v8
    jump block3
block3:
    v13 = icmp ne v11, v1
    brif v13, block4, block6(v12)
block4:
    v14 = load.i64 notrap v11
    v15 = icmp ult v14, v2
    brif v15, block2(v11, v14, v12), block5
block5 cold:
    trap user1
block6(v16: i64):
    return v16
}";

// The same, but block2 also reads the unstepped cursor: nothing to do.
const READ: &str = "function %t(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    v4 = icmp eq v0, v1
    brif v4, block6(v3), block1
block1:
    v5 = load.i64 notrap v0
    v6 = icmp ult v5, v2
    brif v6, block2(v0, v5, v3), block5
block2(v7: i64, v8: i64, v9: i64):
    v10 = iconst.i64 8
    v11 = iadd v7, v10
    v17 = load.i64 notrap v7
    v12 = iadd v9, v17
    jump block3
block3:
    v13 = icmp ne v11, v1
    brif v13, block4, block6(v12)
block4:
    v14 = load.i64 notrap v11
    v15 = icmp ult v14, v2
    brif v15, block2(v11, v14, v12), block5
block5 cold:
    trap user1
block6(v16: i64):
    return v16
}";

// A step no edge can pair with a memory access: left alone.
const NOMEM: &str = "function %t(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block2(v0)
block2(v7: i64):
    v10 = iconst.i64 8
    v11 = iadd v7, v10
    v13 = icmp ne v11, v1
    brif v13, block2(v11), block6
block6:
    return v11
}";

fn run(src: &str) -> (Function, Function, usize) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut f = before.clone();
    let n = pliron_clif_opt::bumpedge::run(&mut f, &mut Analysis::default());
    cranelift_codegen::verify_function(
        &f,
        &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    (before, f, n)
}

fn iadds_in(f: &Function, b: u32) -> usize {
    f.layout
        .block_insts(Block::from_u32(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::Binary { opcode: Opcode::Iadd, .. }))
        .count()
}

#[test]
fn step_moves_to_the_edges() {
    let (before, f, n) = run(LOOP);
    assert_eq!(n, 1, "{}", f.display());
    // block2 keeps only the sum; each reader steps its own pointer.
    assert_eq!(iadds_in(&f, 2), 1, "{}", f.display());
    assert_eq!(iadds_in(&f, 1), 1, "{}", f.display());
    assert_eq!(iadds_in(&f, 4), 1, "{}", f.display());
    let mut cfg = Config::default();
    cfg.trials = 64;
    if let Err(m) = check(&before, &f, &Facts::default(), &cfg) {
        panic!("{m}\n{}", f.display());
    }
}

#[test]
fn a_param_that_is_also_read_stays() {
    let (_, f, n) = run(READ);
    assert_eq!(n, 0, "{}", f.display());
}

#[test]
fn no_memory_access_no_move() {
    let (_, f, n) = run(NOMEM);
    assert_eq!(n, 0, "{}", f.display());
}

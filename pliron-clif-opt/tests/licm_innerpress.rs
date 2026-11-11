//! licm `PLIRON_LICM_INNERPRESS`: in a loop with an inner loop, a load
//! used only outside the inner loop is not hoisted (it would live across
//! the inner loop for nothing: num-bigint-dig's `montgomery` spilled an
//! inner loop's pointer after six outer-loop field loads were hoisted). A
//! load the inner loop uses still hoists.

use cranelift_codegen::ir::{Block, Function, Opcode, Value};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

// v0: a `noalias` read-only struct. Outer loop block1 reads `v0+8` (used
// in block1 only) and `v0+16` (used by the inner loop block2).
const SRC: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v5 = load.i64 notrap v0+8
    v6 = load.i64 notrap v0+16
    v7 = iadd v4, v5
    jump block2(v2, v7)
block2(v8: i64, v9: i64):
    v10 = iadd v9, v6
    v11 = iconst.i64 1
    v12 = iadd v8, v11
    v13 = icmp ult v12, v1
    brif v13, block2(v12, v10), block3
block3:
    v14 = iadd v3, v11
    v15 = icmp ult v14, v1
    brif v15, block1(v14, v10), block4
block4:
    return v10
}";

fn loads_in(f: &Function, b: Block) -> Vec<String> {
    f.layout
        .block_insts(b)
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .map(|i| f.dfg.display_inst(i).to_string())
        .collect()
}

#[test]
fn outer_only_load_stays_inner_used_load_hoists() {
    // Opt-in; this test binary holds only this test.
    unsafe { std::env::set_var("PLIRON_LICM_INNERPRESS", "1") };
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let v0 = Value::from_u32(0);
    let noalias: FxHashSet<Value> = [v0].into_iter().collect();
    let deref: FxHashMap<Value, (u64, bool)> = [(v0, (24, false))].into_iter().collect();
    pliron_clif_opt::licm::run(
        &mut f,
        &mut Analysis::default(),
        &noalias,
        &FxHashSet::default(),
        &deref,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        false,
    );
    let outer = loads_in(&f, Block::from_u32(1));
    // `v0+8` stays in the outer loop; `v0+16` left it.
    assert_eq!(outer.len(), 1, "{outer:?}\n{}", f.display());
    assert!(outer[0].contains("+8"), "{outer:?}\n{}", f.display());
}

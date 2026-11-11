//! licm `PLIRON_LICM_INNERPRESS=2`: a load used only outside a loop's
//! inner loops still hoists while every inner loop has register room
//! (regex's state-decode loop around a tiny varint loop), and stays put
//! when an inner loop is already crowded (montgomery: ~20 values live
//! across its multiply loops). Config is read once per process, so this
//! binary holds only `=2` tests.

use cranelift_codegen::ir::{Block, Function, Opcode, Value};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

fn loads_in(f: &Function, b: Block) -> usize {
    f.layout
        .block_insts(b)
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

fn run(src: &str) -> Function {
    unsafe { std::env::set_var("PLIRON_LICM_INNERPRESS", "2") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
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
    f
}

// Outer loop block1 reads `v0+8`, used only in block1; the inner loop
// block2 carries two values.
const ROOMY: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v5 = load.i64 notrap v0+8
    v7 = iadd v4, v5
    jump block2(v2, v7)
block2(v8: i64, v9: i64):
    v11 = iconst.i64 1
    v10 = iadd v9, v11
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

#[test]
fn roomy_inner_loop_lets_the_load_hoist() {
    let f = run(ROOMY);
    assert_eq!(loads_in(&f, Block::from_u32(1)), 0, "{}", f.display());
}

/// The same shape with 26 values defined before the outer loop and used
/// after the inner one: all live across the inner loop, past the 24-value
/// budget, so the load stays.
fn crowded() -> String {
    let n = 26;
    let mut defs = String::new();
    let mut sum = String::from("    v1000 = iconst.i64 0\n");
    let mut prev = 1000;
    for k in 0..n {
        let v = 2000 + k;
        defs += &format!("    v{} = iconst.i64 {}\n    v{v} = imul v1, v{}\n", 4000 + k, k + 3, 4000 + k);
        sum += &format!("    v{} = iadd v{prev}, v{v}\n", 3000 + k);
        prev = 3000 + k;
    }
    format!(
        "function %f(i64, i64) -> i64 {{
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
{defs}    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v5 = load.i64 notrap v0+8
    v7 = iadd v4, v5
    jump block2(v2, v7)
block2(v8: i64, v9: i64):
    v11 = iconst.i64 1
    v10 = iadd v9, v11
    v12 = iadd v8, v11
    v13 = icmp ult v12, v1
    brif v13, block2(v12, v10), block3
block3:
{sum}    v14 = iadd v3, v11
    v16 = iadd v10, v{prev}
    v15 = icmp ult v14, v1
    brif v15, block1(v14, v16), block4
block4:
    return v16
}}"
    )
}

#[test]
fn crowded_inner_loop_keeps_the_load() {
    let f = run(&crowded());
    assert_eq!(loads_in(&f, Block::from_u32(1)), 1, "{}", f.display());
}

//! Upward load PRE out of a loop body block (regex's `follow_epsilons`:
//! a `&mut SparseSet`'s fields reloaded every outer iteration). The body
//! block first stores through an unrelated pointer, then loads `v0 + 24`
//! through an address computed in the body. PRE's "clobbered earlier in
//! this block" check briefly treated stores to other roots as harmless
//! for an isolated `v0` (`kill_st`'s rule); fuzz seed 27992 miscompiled
//! under it and the plain `may_write` check is back, so the store blocks
//! this PRE again. The rewrite, if any, must stay sound: differentially
//! checked.

use cranelift_codegen::ir::{Block, Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

const SRC: &str = "function %t(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    v40 = iconst.i64 15
    v4 = band v2, v40
    jump block1(v3, v4)
block1(v10: i64, v11: i64):
    brif v11, block2, block9(v10)
block2:
    store.i64 notrap v11, v1
    v41 = iconst.i64 24
    v20 = iadd v0, v41
    v21 = load.i64 notrap can_move v20
    v22 = iadd v10, v21
    v42 = iconst.i64 -1
    v23 = iadd v11, v42
    jump block1(v22, v23)
block9(v30: i64):
    return v30
}";

fn loads_in(f: &Function, b: u32) -> usize {
    f.layout.block_insts(Block::from_u32(b)).filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load).count()
}

#[test]
fn field_load_pre_is_sound() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut f = before.clone();
    let noalias: FxHashSet<_> = [cranelift_codegen::ir::Value::from_u32(0)].into_iter().collect();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &noalias,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    );
    // The plain check keeps the body's load (an unrelated store precedes it).
    assert_eq!(loads_in(&f, 2), 1, "{}", f.display());
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let facts = Facts { noalias: Some(&noalias), ..Facts::default() };
    let s = check(&before, &f, &facts, &Config { trials: 128, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", f.display()));
    assert!(s.compared > 0, "{:?}", s.why);
}

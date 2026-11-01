//! Load forwarding from a wider store: `store.i64 v, p` then `uload32 p`
//! (regex's `cached_state`: an `ip: usize` stored to a stack slot whose
//! address escapes later, reloaded as `u32`), a `load.i32 p+4`
//! (the high half) and an `sload8 p+1` all read bits of `v`.
//! Little-endian. Differentially checked.

use cranelift_codegen::ir::{Function, InstructionData};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

const SRC: &str = "function %t(i64, i64) -> i64, i32, i32 {
    ss0 = explicit_slot 8
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    store.i64 notrap v0, v2
    v3 = uload32.i64 notrap v2
    v4 = load.i32 notrap v2+4
    v5 = sload8.i32 notrap v2+1
    store.i64 notrap v2, v1
    return v3, v4, v5
}";

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::Load { .. }))
        .count()
}

#[test]
fn narrow_loads_read_the_wider_store() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut f = before.clone();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    );
    assert_eq!(loads(&f), 0, "loads left:\n{}", f.display());
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let s = check(&before, &f, &Facts::default(), &Config { trials: 128, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", f.display()));
    assert!(s.compared > 0, "{:?}", s.why);
}

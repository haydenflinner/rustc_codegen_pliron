//! Upward load PRE: a load below a loop header (single-pred chain) whose
//! location is available on the hot back edge but not on entry or on the
//! back edge through a call that receives the pointer. PRE at the header
//! (compensating loads on entry and after the call) makes it a header
//! param, so the hot path stops reloading: regex's `follow_epsilons`
//! reloaded its `&mut SparseSet`'s buffer pointer every iteration. The
//! compensating loads run on paths that never reach the original load, so
//! the address must be dereferenceable (`deref`): without it nothing moves.
//! Differentially checked against the interpreter.

use cranelift_codegen::ir::{Block, Function, Opcode, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

const SRC: &str = "function %t(i64, i64) -> i64 {
    sig0 = (i64)
    fn0 = %grow sig0
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v5 = icmp ult v3, v1
    brif v5, block2, block5(v4)
block2:
    v6 = load.i64 notrap v0+8
    v7 = iadd v4, v6
    v8 = iconst.i64 1
    v9 = iadd v3, v8
    v10 = iconst.i64 7
    v11 = band v3, v10
    brif v11, block1(v9, v7), block3
block3:
    call fn0(v0)
    jump block1(v9, v7)
block5(v12: i64):
    return v12
}";

fn run(deref: bool) -> (Function, Function) {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut f = before.clone();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    let v0 = Value::from_u32(0);
    let noalias: FxHashSet<Value> = [v0].into_iter().collect();
    let mut dm: FxHashMap<Value, (u64, bool)> = FxHashMap::default();
    if deref {
        dm.insert(v0, (16, true));
    }
    // The call's arg is `nocapture`: `v0` stays an `ncc` root.
    let call = f.layout.block_insts(Block::from_u32(3)).next().unwrap();
    let nocap: FxHashSet<_> = [(call, 0u32)].into_iter().collect();
    pliron_clif_opt::loadfwd::run_deref(
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
        &nocap,
        &dm,
    );
    (before, f)
}

fn loads_in(f: &Function, b: u32) -> usize {
    f.layout
        .block_insts(Block::from_u32(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

#[test]
fn body_load_becomes_header_param() {
    let (before, after) = run(true);
    assert_eq!(loads_in(&after, 2), 0, "body still loads:\n{}", after.display());
    let mut cfg = Config::default();
    cfg.trials = 64;
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

#[test]
fn needs_dereferenceable_address() {
    let (_, after) = run(false);
    assert_eq!(loads_in(&after, 2), 1, "speculated without deref:\n{}", after.display());
}

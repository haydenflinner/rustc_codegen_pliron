//! `PLIRON_ESC_TRAP=1`: loadfwd ignores a slot's escapes in a block that
//! ends in `trap` and loads nothing. No code of this frame runs after
//! such a block starts (a plain `call` unwinds past the frame), so a value
//! stored before it can still be forwarded on every other path, even
//! across a store through an untracked pointer (num-bigint-dig's
//! `montgomery`: `assert!` formats `&x`, so `x` lives in a slot whose
//! address only reaches the panic's arguments). A dead-end block that
//! loads keeps the strict escape. Differentially checked.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

fn run(src: &str) -> (Function, Function) {
    // Opt-in; every test in this binary sets the same value.
    unsafe { std::env::set_var("PLIRON_ESC_TRAP", "1") };
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
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
        &FxHashMap::default(),
    );
    (before, f)
}

const PANIC_ESCAPE: &str = "function %f(i64, i64, i64) -> i64 {
    ss0 = explicit_slot 8
    sig0 = (i64)
    fn0 = %g sig0

block0(v0: i64, v1: i64, v2: i64):
    v3 = stack_addr.i64 ss0
    store.i64 notrap v0, v3
    brif v2, block1, block2

block1 cold:
    call fn0(v3)
    trap user1

block2:
    store.i64 notrap v2, v1
    v4 = load.i64 notrap v3
    return v4
}";

#[test]
fn escape_into_a_panic_does_not_stop_forwarding() {
    let (before, f) = run(PANIC_ESCAPE);
    assert_eq!(loads(&f), 0, "slot load kept:\n{}", f.display());
    if let Err(m) = check(&before, &f, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", f.display());
    }
}

// The dead-end block reads memory after its call: its escape stays.
const LOADING_DEAD_END: &str = "function %f(i64, i64, i64) -> i64 {
    ss0 = explicit_slot 8
    sig0 = (i64)
    fn0 = %g sig0

block0(v0: i64, v1: i64, v2: i64):
    v3 = stack_addr.i64 ss0
    store.i64 notrap v0, v3
    brif v2, block1, block2

block1 cold:
    call fn0(v3)
    v5 = load.i64 notrap v1
    trap user1

block2:
    store.i64 notrap v2, v1
    v4 = load.i64 notrap v3
    return v4
}";

#[test]
fn a_dead_end_that_loads_keeps_the_escape() {
    let (_, f) = run(LOADING_DEAD_END);
    assert_eq!(loads(&f), 2, "forwarded past a loading dead end:\n{}", f.display());
}

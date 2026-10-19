//! `loadfwd`'s `nocap` tier: a stack slot whose only escape is a call
//! operand the callee can't retain (`captures(none)`-equivalent) keeps no
//! lasting untracked alias — stores rooted elsewhere can't reach it — but
//! a call that is actually handed the pointer may still write through it,
//! so it must clobber the slot's cached locations like any other call.

use cranelift_codegen::ir::{Function, Inst};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn calls(f: &Function) -> Vec<Inst> {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode().is_call())
        .collect()
}

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Load)
        .count()
}

fn run(f: &mut Function, nocap: &FxHashSet<(Inst, u32)>) -> usize {
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(f);
    let pf = pliron_clif_opt::proots::facts_cfg(f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        f,
        &cfg,
        &pf,
        &ps,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        nocap,
    )
}

// v8's value must survive the `store v0, v9` (an untracked param's
// pointee) into v4 — but only because ss0's sole escape is the `nocap`
// operand of `call fn0(v1)`. Without the nocap mark the store kills it.
const SURVIVES_UNRELATED_STORE: &str = "function %f(i64, i64) -> i64 {
    ss0 = explicit_slot 8
    sig0 = (i64)
    fn0 = %g sig0

block0(v0: i64, v9: i64):
    v1 = stack_addr.i64 ss0
    v3 = load.i64 notrap v1
    call fn0(v1)
    v8 = load.i64 notrap v1
    store.i64 notrap v0, v9
    v4 = load.i64 notrap v1
    v5 = iadd v8, v4
    return v5
}";

#[test]
fn ncc_root_survives_unrelated_store() {
    let mut f = parse(SURVIVES_UNRELATED_STORE);
    let nocap: FxHashSet<(Inst, u32)> = [(calls(&f)[0], 0)].into_iter().collect();
    run(&mut f, &nocap);
    assert_eq!(loads(&f), 2, "{}", f.display());
}

#[test]
fn without_nocap_the_escape_still_kills() {
    let mut f = parse(SURVIVES_UNRELATED_STORE);
    run(&mut f, &FxHashSet::default());
    assert_eq!(loads(&f), 3, "{}", f.display());
}

// `captures(none)` is not `readonly`: the callee may write through the
// pointer during the call, so v4 must reload, not forward v3.
#[test]
fn nocap_call_receiving_the_root_still_clobbers() {
    let mut f = parse(
        "function %f(i64) -> i64 {
            ss0 = explicit_slot 8
            sig0 = (i64)
            fn0 = %g sig0

        block0(v0: i64):
            v1 = stack_addr.i64 ss0
            v3 = load.i64 notrap v1
            call fn0(v1)
            v4 = load.i64 notrap v1
            v5 = iadd v3, v4
            return v5
        }",
    );
    let nocap: FxHashSet<(Inst, u32)> = [(calls(&f)[0], 0)].into_iter().collect();
    run(&mut f, &nocap);
    assert_eq!(loads(&f), 2, "{}", f.display());
}

// A call handed only ss1 leaves ss0's cached value available: the callee
// holds no lasting alias that could reach it. The trailing `call fn0(v1)`
// is what makes ss0 an ncc root — and correctly kills it for v6.
#[test]
fn ncc_root_survives_a_call_it_is_not_passed_to() {
    let mut f = parse(
        "function %f(i64) -> i64 {
            ss0 = explicit_slot 8
            ss1 = explicit_slot 8
            sig0 = (i64)
            fn0 = %g sig0

        block0(v0: i64):
            v1 = stack_addr.i64 ss0
            v2 = stack_addr.i64 ss1
            v3 = load.i64 notrap v1
            call fn0(v2)
            v4 = load.i64 notrap v1
            call fn0(v1)
            v6 = load.i64 notrap v1
            v5 = iadd v4, v6
            return v5
        }",
    );
    let cs = calls(&f);
    let nocap: FxHashSet<(Inst, u32)> = [(cs[0], 0), (cs[1], 0)].into_iter().collect();
    let n = run(&mut f, &nocap);
    assert_eq!(n, 1, "{}", f.display());
    assert_eq!(loads(&f), 2, "{}", f.display());
}

// For `call_indirect` the callee rides operand 0; `(inst, 0)` must mean
// the first real argument (v1), not the callee. If indexing slipped, ss0
// would have an unbounded escape and the param store would kill v4.
#[test]
fn indirect_call_arg_zero_is_the_first_argument() {
    let mut f = parse(
        "function %f(i64, i64) -> i64 {
            ss0 = explicit_slot 8
            sig0 = (i64)

        block0(v0: i64, v9: i64):
            v1 = stack_addr.i64 ss0
            v10 = load.i64 notrap v9
            v3 = load.i64 notrap v1
            call_indirect sig0, v10(v1)
            v8 = load.i64 notrap v1
            store.i64 notrap v0, v9
            v4 = load.i64 notrap v1
            v5 = iadd v8, v4
            return v5
        }",
    );
    let nocap: FxHashSet<(Inst, u32)> = [(calls(&f)[0], 0)].into_iter().collect();
    run(&mut f, &nocap);
    // v10's load + v3 + v8 + v4 → v4 forwards; 4 loads become 3.
    assert_eq!(loads(&f), 3, "{}", f.display());
}

// Semantics check: the forwarded program must behave identically.
#[test]
fn ncc_forwarding_preserves_behavior() {
    let before = parse(SURVIVES_UNRELATED_STORE);
    let mut after = before.clone();
    let nocap: FxHashSet<(Inst, u32)> = [(calls(&after)[0], 0)].into_iter().collect();
    run(&mut after, &nocap);
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

//! `isolated_escapes` keys roots like loc keys do (`norm_root`): a
//! `noalias` value that concretes to a stack slot names the slot's
//! memory. Recording the escape under the scope name `V(v)` leaves
//! `S(slot)` isolated while its memory is reachable through the stashed
//! pointer — a store through an untracked root then fails to kill the
//! slot's cached locations.

use cranelift_codegen::ir::{Function, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Load)
        .count()
}

fn run(f: &mut Function, noalias: FxHashSet<Value>) -> usize {
    pliron_clif_opt::loadfwd::run(
        f,
        &noalias,
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    )
}

// `v1` is a `noalias` `stack_addr`: `norm_root` concretes its derived
// addresses to `S(ss0)`, so `store v2, v9` stashing `&ss0[16]` into
// opaque param memory must mark `S(ss0)` non-isolated. Otherwise the
// opaque `store v5, v9` keeps loc `(ss0,16)` and `v7` forwards the stale
// zero across the real write.
const ESCAPE_THROUGH_SCOPE_NAME: &str = "function %f(i64, i64) -> i64 {
    ss0 = explicit_slot 24

block0(v0: i64, v9: i64):
    v1 = stack_addr.i64 ss0
    v2 = iconst.i64 16
    v3 = iadd v1, v2
    v4 = iconst.i64 0
    store.i64 notrap v4, v3
    store.i64 notrap v3, v9
    v5 = iconst.i64 42
    store.i64 notrap v5, v9
    v7 = load.i64 notrap v3
    return v7
}";

#[test]
fn slot_escape_through_noalias_scope_name_kills_locs() {
    let mut f = parse(ESCAPE_THROUGH_SCOPE_NAME);
    let na: FxHashSet<Value> = [Value::from_u32(1)].into_iter().collect();
    run(&mut f, na);
    assert_eq!(loads(&f), 1, "{}", f.display());
}

#[test]
fn scope_name_escape_preserves_behavior() {
    let before = parse(ESCAPE_THROUGH_SCOPE_NAME);
    let mut after = before.clone();
    let na: FxHashSet<Value> = [Value::from_u32(1)].into_iter().collect();
    run(&mut after, na);
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

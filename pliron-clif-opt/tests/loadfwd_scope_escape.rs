//! `loadfwd`'s slot isolation must follow the *normalized* root: a
//! `stack_addr` marked `noalias` roots its derived pointers at `V(v)`,
//! but memory locs key under `S(slot)`. Stashing `&slot[k]` into another
//! slot used to escape only `V(v)` — the slot stayed "isolated", so a
//! later store through the loaded pointer (a `Vec::set_len`-style write)
//! couldn't kill the slot's cached locations and the len load forwarded
//! the stale `0` instead of `5`.

use cranelift_codegen::ir::{Function, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn run(f: &mut Function, na: &FxHashSet<Value>) -> usize {
    pliron_clif_opt::loadfwd::run(
        f,
        na,
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
    )
}

// v1 names ss0 through a `noalias` scope. `store v4, v5` stashes
// &ss0[16] into ss1 — an escape of the scope and therefore of the slot.
// `store v6, v8` writes ss0+16 through the loaded pointer; `v9` must see
// `v6 = 5`, not the stale `v2 = 0` the slot's cached loc still holds if
// ss0 wrongly counts as isolated.
const STASHED_LEN: &str = "function %f() -> i64 {
    ss0 = explicit_slot 24
    ss1 = explicit_slot 8

block0:
    v1 = stack_addr.i64 ss0
    v2 = iconst.i64 0
    store notrap v2, v1+16
    v3 = iconst.i64 16
    v4 = iadd v1, v3  ; v3 = 16
    v5 = stack_addr.i64 ss1
    store notrap v4, v5
    v6 = iconst.i64 5
    v8 = load.i64 notrap v5
    store notrap v6, v8
    v9 = load.i64 notrap v1+16
    return v9
}";

#[test]
fn scope_escape_unisolates_the_slot() {
    let before = parse(STASHED_LEN);
    let mut after = before.clone();
    let na: FxHashSet<Value> = [Value::from_u32(1)].into_iter().collect();
    run(&mut after, &na);
    // The len load forwards to the set_len value, not the stale init.
    let v9 = after.dfg.resolve_aliases(Value::from_u32(9));
    let v6 = Value::from_u32(6);
    assert_eq!(v9, v6, "{}", after.display());
    check(&before, &after, &Facts::default(), &Config { trials: 64, ..Config::default() })
        .unwrap_or_else(|m| panic!("{}", m));
}

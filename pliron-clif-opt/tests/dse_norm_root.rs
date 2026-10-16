//! `dse`'s read demands must key under the *normalized* root: a
//! `stack_addr` marked `noalias` roots its locs at `V(v)`, but stores
//! reaching the same slot through an unmarked `stack_addr` key under
//! `S(slot)`. With the raw root the load's demand lands under `V(v)`,
//! the slot's stores see no demand, and a live store dies — the
//! `Vec`/`RawVec` double-free in `SpecFromIterNested::from_iter`
//! (`char_indices().map().collect()`) where the final state stores are
//! re-read through a vectorized `load.i8x16` of a noalias-marked scope
//! name.

use cranelift_codegen::ir::{Function, Value};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn dse(f: &mut Function, na: &FxHashSet<Value>) -> usize {
    pliron_clif_opt::dse::run(
        f,
        na,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &mut pliron_clif_opt::pipeline::Analysis::default(),
        "dse",
        "t",
    )
}

// v3 names ss7 through a `noalias` scope; the `load.i8x16` through it
// must still demand the `store.i64` that fills ss7[0..8].
const SCOPE_LOAD: &str = "function %t(i64) -> i32 sret {
    ss7 = explicit_slot 24
block0(v0: i64):
    v1 = iconst.i64 7
    v2 = stack_addr.i64 ss7
    store.i64 notrap v1, v2
    v3 = stack_addr.i64 ss7
    v4 = load.i8x16 notrap v3
    store notrap v4, v0
    return
}";

#[test]
fn scope_named_load_demands_the_slot_store() {
    let mut func = parse(SCOPE_LOAD);
    let na: FxHashSet<Value> = [Value::from_u32(3)].into_iter().collect();
    assert_eq!(dse(&mut func, &na), 0, "{}", func.display());
}

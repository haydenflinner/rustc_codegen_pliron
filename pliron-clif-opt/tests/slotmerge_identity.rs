//! `slotmerge` must not fold a slot whose *address* is observed: an
//! `icmp`/`isub`/bit-op on a slot-derived pointer reads the address
//! itself, so folding `ss_a` onto `ss_b` would flip `addr(a) == addr(b)`
//! from false to true (and a ptrdiff to 0). Only byte-touches
//! (load/store/call out-params) are safe to merge.

use cranelift_codegen::ir::Function;
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn merge(f: &mut Function) -> usize {
    pliron_clif_opt::slotmerge::run(
        f,
        &FxHashMap::default(),
        &[],
        &FxHashSet::default(),
    )
}

fn slot_sizes(f: &Function) -> Vec<(String, u32)> {
    f.sized_stack_slots
        .iter()
        .map(|(s, d)| (s.to_string(), d.size))
        .collect()
}

// ss1 and ss2 have disjoint byte-use windows (icmp aside), so without
// the identity check they'd fold; the compare must pin both.
const ADDR_COMPARE: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    ss2 = explicit_slot 8, align = 8
    ss3 = explicit_slot 8, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = stack_addr.i64 ss1
    v3 = stack_addr.i64 ss2
    v4 = stack_addr.i64 ss3
    store.i64 v0, v1
    v5 = load.i64 v1
    v6 = icmp eq v2, v3
    store.i64 v5, v2
    v7 = load.i64 v2
    store.i64 v7, v3
    v8 = load.i64 v3
    brif v6, block1, block2
block1:
    store.i64 v8, v4
    v9 = load.i64 v4
    return v9
block2:
    return v8
}";

#[test]
fn compared_slot_addresses_never_merge() {
    let mut f = parse(ADDR_COMPARE);
    merge(&mut f);
    // ss1 and ss2 feed the icmp: both must keep their storage.
    let sizes = slot_sizes(&f);
    let get = |name: &str| sizes.iter().find(|(s, _)| s == name).map(|x| x.1);
    assert_eq!(get("ss1"), Some(8), "icmp'd ss1 must not fold: {sizes:?}");
    assert_eq!(get("ss2"), Some(8), "icmp'd ss2 must not fold: {sizes:?}");
}

// The same shape minus the icmp must still merge — the fix only pins
// identity-observed slots, not byte-disjoint ones.
const NO_COMPARE: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = stack_addr.i64 ss1
    store.i64 v0, v1
    v5 = load.i64 v1
    store.i64 v0, v2
    v7 = load.i64 v2
    return v7
}";

#[test]
fn byte_disjoint_slots_still_merge() {
    let mut f = parse(NO_COMPARE);
    let k = merge(&mut f);
    assert!(k > 0, "expected a merge: {k}");
}

// A ptrdiff between two slot addresses is data, not a pointer: merging
// the pair would report distance 0.
const PTRDIFF: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    ss2 = explicit_slot 8, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = stack_addr.i64 ss1
    v3 = stack_addr.i64 ss2
    store.i64 v0, v1
    v5 = load.i64 v1
    store.i64 v5, v2
    v6 = isub v2, v3
    v7 = load.i64 v2
    return v6
}";

#[test]
fn ptrdiff_between_slots_never_merges() {
    let mut f = parse(PTRDIFF);
    merge(&mut f);
    let sizes = slot_sizes(&f);
    let get = |name: &str| sizes.iter().find(|(s, _)| s == name).map(|x| x.1);
    assert_eq!(get("ss1"), Some(8), "ptrdiff'd ss1 must not fold: {sizes:?}");
    assert_eq!(get("ss2"), Some(8), "ptrdiff'd ss2 must not fold: {sizes:?}");
}

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
    pliron_clif_opt::slotmerge::run(f, &FxHashMap::default(), &[], &FxHashSet::default())
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
    assert_eq!(
        get("ss1"),
        Some(8),
        "ptrdiff'd ss1 must not fold: {sizes:?}"
    );
    assert_eq!(
        get("ss2"),
        Some(8),
        "ptrdiff'd ss2 must not fold: {sizes:?}"
    );
}

// A pointer stored into a container slot and reloaded later must keep
// the pointee's interval live: `load` results carry the *contained*
// pointers, not just "bytes of the container". If the reload is
// attributed to the container only, the pointee merges early and the
// reconstituted pointer derefs the folded storage.
const CONTAINED_PTR: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
    ss1 = explicit_slot 8, align = 8
    ss2 = explicit_slot 8, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = stack_addr.i64 ss1
    v3 = stack_addr.i64 ss2
    ; ss0 early byte-use
    store.i64 v0, v1
    v4 = load.i64 v1
    ; ss1 gets ss2's pointer stored in it (container edge)
    store.i64 v3, v2
    ; ss1's byte-uses end
    v5 = load.i64 v2
    ; ss0 dead -> ss1 dead: merge ss1..ss2 patterns? reconstituted ptr:
    v6 = load.i64 v5
    ; ss0's second live range after the reload's deref
    store.i64 v4, v1
    v7 = load.i64 v1
    return v7
}";

#[test]
fn contained_pointer_keeps_pointee_live() {
    let mut f = parse(CONTAINED_PTR);
    merge(&mut f);
    // ss2's pointer lives inside ss1 past ss1's last byte-use; ss2 must
    // not fold onto a slot that overlaps the reconstituted deref. The
    // conservative answer is that ss2 keeps its own storage.
    let sizes = slot_sizes(&f);
    let get = |name: &str| sizes.iter().find(|(s, _)| s == name).map(|x| x.1);
    assert_eq!(get("ss2"), Some(8), "ss2 must not fold: {sizes:?}");
}

// Two-hop containment: &ss2 stored into ss1, ss1's bytes vector-copied
// into ss0 (load.i8x16/store.i8x16 aggregate move — not a memcpy
// libcall), ss1 dies, then the pointer is reloaded out of ss0 and
// dereferenced. The vector-load gate leaves the copied bytes
// slot-anonymous, so no edge bounds ss2 past ss1's range: ss2 folds and
// the stale pointer keeps the old offset — which some other merged slot
// now owns.
const VECTOR_COPY_HOP: &str = "function %t(i64) -> i64 {
    ss0 = explicit_slot 16, align = 8
    ss1 = explicit_slot 16, align = 8
    ss2 = explicit_slot 8, align = 8
    ss3 = explicit_slot 8, align = 8
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = stack_addr.i64 ss1
    v3 = stack_addr.i64 ss2
    v4 = stack_addr.i64 ss3
    ; ss2 early use so it has a bounded interval ending soon
    store.i64 v0, v3
    v9 = load.i64 v3
    ; &ss2 into ss1 (container edge pins ss2 over ss1's range)
    store.i64 v3, v2
    ; aggregate-copy ss1 -> ss0 through a vector
    v5 = load.i8x16 v2
    store.i8x16 v5, v1
    ; ss1's last byte-use: from here ss2's interval (== ss1's) can end
    v6 = load.i64 v2
    ; ss3 fills ss2's old interval window, then gets reused after
    store.i64 v0, v4
    v7 = load.i64 v4
    ; reconstituted pointer out of ss0, dereferenced late
    v8 = load.i64 v1
    v10 = load.i64 v8
    ; ss3's second range overlapping the deref
    store.i64 v7, v4
    v11 = load.i64 v4
    v12 = iadd v11, v10
    return v12
}";

#[test]
fn vector_copied_pointer_keeps_pointee_live() {
    let mut f = parse(VECTOR_COPY_HOP);
    let k = merge(&mut f);
    // The stale &ss2 is dereferenced at v10 — inside ss3's live range —
    // so ss2's interval must cover it, making ss2/ss3 overlap and the
    // merge illegal.
    assert_eq!(k, 0, "merge folded a live pointee's storage: {k}");
}

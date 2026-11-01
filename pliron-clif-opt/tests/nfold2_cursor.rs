//! `nfold2`'s null-test fold proves a byte cursor non-null after passes
//! dropped its `derived` (inbounds) marks. regex's `InstPtrs` varint loop
//! tests `p == 0` per byte (the `Option<&u8>` niche of `slice::Iter`):
//! sameargs rebuilt the `p + 1` advance without a mark, and the outer
//! `p + nread` advance kept a mark whose base had been deleted. The cursor
//! starts at an inbounds offset of a `!nonnull` load. Differentially
//! checked.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Function, Opcode, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::jumpthread::NonNull;
use pliron_clif_opt::vmap::VMap;
use rustc_hash::FxHashSet;

/// `v12 = p + 1` has no mark; it is reached only after `uload8 p`.
const STEP: &str = "function %f(i64, i64) -> i32 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 17
    v3 = iadd v0, v2
    v4 = iadd v3, v1
    jump block1(v3)
block1(v5: i64):
    v6 = iconst.i64 0
    v7 = icmp eq v5, v6
    brif v7, block3, block2
block2:
    v8 = uload8.i32 notrap v5
    v9 = iconst.i32 128
    v10 = band v8, v9
    brif v10, block4, block3
block4:
    v11 = iconst.i64 1
    v12 = iadd v5, v11
    v13 = icmp eq v12, v4
    brif v13, block3, block1(v12)
block3:
    v14 = iconst.i32 0
    return v14
}";

/// `v12 = p + n` is marked inbounds of `v15`, a deleted value (the parser
/// leaves numbering gaps as tombstones).
const TOMB: &str = "function %f(i64, i64, i64) -> i32 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 17
    v4 = iadd v0, v3
    jump block1(v4, v1)
block1(v5: i64, v6: i64):
    v7 = iconst.i64 0
    v8 = icmp eq v5, v7
    brif v8, block3, block2
block2:
    v9 = uload8.i32 notrap v5
    v10 = iconst.i64 1
    v11 = isub v6, v10
    v12 = iadd v5, v2
    brif v11, block1(v12, v11), block3
block3:
    v20 = iconst.i32 0
    return v20
}";

fn null_test_on(f: &Function, v: Value) -> bool {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .any(|i| f.dfg.insts[i].opcode() == Opcode::Icmp && f.dfg.inst_args(i).contains(&v))
}

fn fold(src: &str, derived: &[(u32, u32)], ext: bool) -> (Function, Function, FxHashSet<Value>) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let nonnull: FxHashSet<Value> = [Value::from_u32(0)].into_iter().collect();
    let mut d = VMap::default();
    for &(v, b) in derived {
        d.insert(Value::from_u32(v), Value::from_u32(b));
    }
    let cfg = ControlFlowGraph::with_function(&after);
    let nn = NonNull::new_ext(&after, &cfg, &nonnull, &d, ext);
    pliron_clif_opt::niche::run_with(&mut after, &nn, true);
    (before, after, nonnull)
}

fn diff(before: &Function, after: &Function, nonnull: &FxHashSet<Value>) {
    let mut facts = Facts::default();
    facts.nonnull = Some(nonnull);
    if let Err(m) = check(before, after, &facts, &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}

#[test]
fn dereferenced_cursor_step_is_non_null() {
    let (_, after, _) = fold(STEP, &[(3, 0)], false);
    assert!(null_test_on(&after, Value::from_u32(5)), "folded without the ext proofs:\n{}", after.display());
    let (before, after, nonnull) = fold(STEP, &[(3, 0)], true);
    assert!(!null_test_on(&after, Value::from_u32(5)), "null test kept:\n{}", after.display());
    diff(&before, &after, &nonnull);
}

#[test]
fn deleted_base_offset_is_non_null() {
    let (_, after, _) = fold(TOMB, &[(4, 0), (12, 15)], false);
    assert!(null_test_on(&after, Value::from_u32(5)), "folded without the ext proofs:\n{}", after.display());
    let (before, after, nonnull) = fold(TOMB, &[(4, 0), (12, 15)], true);
    assert!(!null_test_on(&after, Value::from_u32(5)), "null test kept:\n{}", after.display());
    diff(&before, &after, &nonnull);
}

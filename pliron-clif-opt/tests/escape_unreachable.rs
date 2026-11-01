//! `isolated_escapes` ignores blocks no path from the entry reaches. A
//! folded branch leaves its dead arm in the layout until the next
//! unreachable sweep; regex's `SparseSet::insert` loop kept `qcur`
//! escaped through licm3 because the dead `Vec` grow call still took it.

use cranelift_codegen::ir::Value;
use pliron_clif_opt::loadfwd::{Root, isolated_escapes, param_src};
use rustc_hash::{FxHashMap, FxHashSet};

#[test]
fn unreachable_call_is_not_an_escape() {
    let src = "function %f(i64) -> i64 {
    sig0 = (i64)
    fn0 = %g sig0
block0(v0: i64):
    v1 = load.i64 notrap v0
    return v1
block1:
    call fn0(v0)
    v2 = iconst.i64 0
    return v2
}";
    let f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let na: FxHashSet<Value> = [Value::from_u32(0)].into_iter().collect();
    let ps = param_src(&f);
    let (iso, esc, _) = isolated_escapes(&f, &na, &ps, &FxHashMap::default(), &FxHashSet::default(), &FxHashSet::default());
    assert!(iso.contains(&Root::V(Value::from_u32(0))), "escapes {esc:?}");
}

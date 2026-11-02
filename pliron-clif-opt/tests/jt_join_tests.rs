//! `jumpthread::thread_join_tests` (`PLIRON_JOINTHREAD`): an inlined
//! `Option`-returning `peek()` in a scan loop merges its tag and byte
//! (`block4`), the loop tests the tag, and the loop's exits join to test
//! the same tag again for the caller (`block6`). Each exit is reached
//! through one arm of the first test, so its jump goes straight to the
//! decided successor and the re-test disappears; that leaves the merge's
//! values used only under the merge, so it can be threaded in turn
//! (serde_json's `parse_whitespace`). Differentially checked.

use cranelift_codegen::ir::{Function, InstructionData, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    jump block1(v2)
block1(v3: i64):
    v4 = icmp ult v3, v1
    brif v4, block2, block3
block2:
    v5 = iadd v0, v3
    v6 = load.i8 notrap v5
    v7 = iconst.i8 1
    jump block4(v7, v6)
block3:
    v8 = iconst.i8 0
    jump block4(v8, v8)
block4(v9: i8, v10: i8):
    v11 = uextend.i32 v9
    brif v11, block5, block9
block5:
    v12 = iconst.i8 32
    v13 = icmp eq v10, v12
    v14 = iconst.i64 1
    v15 = iadd v3, v14
    brif v13, block1(v15), block10
block9:
    jump block6
block10:
    jump block6
block6:
    brif v9, block7, block8
block7:
    v16 = uextend.i64 v10
    return v16
block8:
    v17 = iconst.i64 -1
    return v17
}";

fn brif_on(f: &Function, v: Value) -> bool {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).any(|i| {
        matches!(f.dfg.insts[i], InstructionData::Brif { arg, .. } if f.dfg.resolve_aliases(arg) == v)
    })
}

#[test]
fn join_retest_of_option_tag_is_threaded() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    let n = pliron_clif_opt::jumpthread::thread_join_tests(&mut after);
    pliron_clif_opt::jumpthread::remove_unreachable_blocks(&mut after);
    assert!(n >= 2, "threaded {n}:\n{}", after.display());
    assert!(!brif_on(&after, Value::from_u32(9)), "tag re-test kept:\n{}", after.display());
    if let Err(m) = check(&before, &after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}

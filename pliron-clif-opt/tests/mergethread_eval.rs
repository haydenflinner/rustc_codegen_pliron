//! mergethread with `PLIRON_MERGETHREAD_EVAL=1`: a merge whose branch tests
//! a value it computes from its parameters (foldhash's `sponge_len + 64 >=
//! 129`), decided on every edge. Differentially checked and verified: a
//! successor no edge picks used to stay in the layout reading the removed
//! merge's values (regalloc2's `slot_pair_layout` in the clcg corpus).

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn run(src: &str) -> (Function, Function, usize) {
    unsafe { std::env::set_var("PLIRON_MERGETHREAD_EVAL", "1") };
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::mergethread::run(&mut after);
    let flags = settings::Flags::new(settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(&after, &flags) {
        panic!("{e}\n{}", after.display());
    }
    (before, after, k)
}

fn checked(before: &Function, after: &Function) {
    let cfg = Config { trials: 256, ..Config::default() };
    if let Err(m) = check(before, after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

/// Two edges pass the length 64 and 128; the merge tests `len + 64 >= 129`.
#[test]
fn sponge_length_test_threads() {
    let (before, after, k) = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i8 64
    v3 = iconst.i8 128
    brif v0, block1(v2, v1), block1(v3, v0)
block1(v10: i8, v11: i64):
    v12 = uextend.i32 v10
    v13 = iconst.i32 64
    v14 = iadd v12, v13
    v15 = iconst.i32 129
    v16 = icmp uge v14, v15
    brif v16, block2, block3
block2:
    v20 = uextend.i64 v10
    v21 = iadd v11, v20
    return v21
block3:
    v30 = uextend.i64 v10
    v31 = isub v11, v30
    return v31
}");
    assert_eq!(k, 1, "{}", after.display());
    checked(&before, &after);
}

/// One edge only: the successor it does not pick is unreachable and must
/// not keep the merge's values.
#[test]
fn unpicked_successor_goes() {
    let (before, after, k) = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 1
    brif v0, block1(v2, v1), block4
block4:
    return v0
block1(v10: i64, v11: i64):
    v12 = ireduce.i8 v10
    brif v12, block2(v11), block3(v10)
block2(v20: i64):
    brif v12, block5(v20), block6
block3(v30: i64):
    brif v12, block5(v30), block6
block5(v40: i64):
    return v40
block6:
    v50 = iconst.i64 7
    return v50
}");
    assert!(k >= 1, "{}", after.display());
    checked(&before, &after);
}

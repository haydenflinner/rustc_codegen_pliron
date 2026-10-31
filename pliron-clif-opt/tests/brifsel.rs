//! brifsel: `brif c` args that are `select c, a, b` become `a` on the
//! taken edge and `b` on the other (the TrustedRandomAccess zip's
//! if-converted index update). Differentially checked.

use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0, v0)
block1(v10: i64, v11: i64):
    v12 = icmp ult v10, v1
    v13 = iconst.i64 1
    v14 = iadd v10, v13
    v15 = select v12, v14, v10
    v16 = iadd v11, v10
    brif v12, block1(v15, v16), block2(v15, v16)
block2(v20: i64, v21: i64):
    v22 = iadd v20, v21
    return v22
}";

#[test]
fn select_on_branch_condition_splits_per_edge() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::brifsel::run(&mut after), 2, "{}", after.display());
    let text = after.display().to_string();
    assert!(text.contains("block1(v14, v16), block2(v10, v16)"), "{text}");
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{text}");
    }
}

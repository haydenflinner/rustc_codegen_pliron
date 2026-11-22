//! `brbits`: a `br_table` of 54 entries with three non-default destinations
//! (lol_html's `tag_name_state` byte switch) becomes a bounds test and bit
//! tests. Checked against the interpreter on random `i8` inputs, with
//! block arguments on a destination and on the default.

use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i8) -> i32 {
block0(v0: i8):
    v1 = iconst.i8 -9
    v2 = iadd v0, v1
    v3 = uextend.i32 v2
    v9 = iconst.i8 77
    br_table v3, block30(v0), [block20(v9), block20(v9), block30(v0), block20(v9), block20(v9), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block20(v9), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block21, block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block30(v0), block22]
block20(v20: i8):
    v40 = sextend.i32 v20
    v41 = iconst.i32 1000
    v42 = iadd v40, v41
    return v42
block21:
    v43 = iconst.i32 21
    return v43
block22:
    v44 = iconst.i32 22
    return v44
block30(v30: i8):
    v45 = sextend.i32 v30
    return v45
}";

#[test]
fn table_becomes_bit_tests_and_agrees() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    let n = pliron_clif_opt::brbits::run(&mut after);
    assert_eq!(n, 1, "{}", after.display());
    assert!(!format!("{}", after.display()).contains("br_table"), "{}", after.display());
    let cfg = Config { trials: 3000, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

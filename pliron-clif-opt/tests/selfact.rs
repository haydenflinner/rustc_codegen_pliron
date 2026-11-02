//! jumpthread `PLIRON_SELFACT`: a `brif` on `select(c, 0, x)` tells the
//! taken edge that `c` is false (an inlined slice `next()` is
//! `select(ptr == end, 0, ptr)`; regex's varint decoder re-tested
//! `ptr == end` after it). The zero-side edge of `select(c, x, 0)` says
//! nothing about `c` unless `x` is a nonzero constant. Differentially
//! checked.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

// block1 (taken, v4 != 0) re-tests `v0 == v1`: decided false.
// block2 (not taken) holds `select(c, x, 0)`'s zero edge test, which a
// zero `x` (v0 == 0, v1 != 0) leaves undecided.
const SRC: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = icmp eq v0, v1
    v3 = iconst.i64 0
    v4 = select v2, v3, v0
    brif v4, block1, block2
block1:
    v5 = icmp eq v0, v1
    v6 = iconst.i64 7
    v7 = iconst.i64 9
    v8 = select v5, v6, v7
    return v8
block2:
    v9 = select v2, v0, v3
    brif v9, block3, block4
block3:
    return v3
block4:
    v10 = icmp eq v0, v1
    v11 = uextend.i64 v10
    return v11
}";

fn icmps(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Icmp)
        .count()
}

#[test]
fn select_branch_decides_its_condition() {
    // Opt-in; this test binary holds only this test.
    unsafe { std::env::set_var("PLIRON_SELFACT", "1") };
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::jumpthread::fold_dominated_conds(&mut after);
    // Only block1's re-test folds; block0's test and block4's stay.
    assert_eq!(k, 1, "{}", after.display());
    assert_eq!(icmps(&after), 2, "{}", after.display());
    if let Err(m) = check(&before, &after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}

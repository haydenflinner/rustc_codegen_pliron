//! mintrip: a fill loop with two exits (`rem != 0`, `len + 1 <u cap`;
//! SmallVec's `extend(repeat(x).take(n))`) gets a counted pre-loop of
//! `umin` trips before the original loop. Differentially checked,
//! including which exit is taken; a loop with a non-affine exit is left
//! alone.

use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

/// Inputs masked so trip counts stay small: len0 <= 7, cap <= 15,
/// rem0 <= 15. Returns the final len, +1000 on the capacity exit.
const SRC: &str = "function %f(i64, i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64, v4: i64):
    v40 = iconst.i64 7
    v41 = iconst.i64 15
    v5 = band v1, v40
    v6 = band v2, v41
    v7 = band v3, v41
    jump block1(v5, v7)
block1(v10: i64, v11: i64):
    v42 = iconst.i64 0
    v12 = icmp eq v11, v42
    brif v12, block3(v10), block2
block2:
    v43 = iconst.i64 3
    v13 = ishl v10, v43
    v14 = iadd v0, v13
    store notrap v4, v14
    v44 = iconst.i64 1
    v15 = iadd v10, v44
    v45 = iconst.i64 -1
    v16 = iadd v11, v45
    v17 = icmp ult v15, v6
    brif v17, block1(v15, v16), block4(v15)
block3(v20: i64):
    return v20
block4(v21: i64):
    v46 = iconst.i64 1000
    v22 = iadd v21, v46
    return v22
}";

fn blocks(f: &cranelift_codegen::ir::Function) -> usize {
    f.layout.blocks().count()
}

#[test]
fn two_exit_fill_gets_counted_preloop() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::mintrip::run(&mut after, &mut Analysis::default()), 1, "{}", after.display());
    assert!(blocks(&after) > blocks(&before));
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags).unwrap_or_else(|e| panic!("{e}\n{}", after.display()));
    let s = check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", after.display()));
    assert!(s.compared > 0, "nothing compared: {:?}", s.why);
}

#[test]
fn non_affine_exit_stays() {
    // The capacity test reads memory: not a linear form.
    let src = SRC.replace("v17 = icmp ult v15, v6", "v30 = load.i64 notrap v14\n    v17 = icmp ult v15, v30");
    let mut f = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    assert_eq!(pliron_clif_opt::mintrip::run(&mut f, &mut Analysis::default()), 0, "{}", f.display());
}

/// Two down-counting exits (`rem != 0`, `q >u lim`) and no param stepping
/// by +1: the pre-loop counts with a `j` of its own.
#[test]
fn no_unit_step_param_counts_with_j() {
    let src = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v40 = iconst.i64 15
    v5 = band v1, v40
    v6 = band v2, v40
    v7 = band v3, v40
    jump block1(v5, v6)
block1(v10: i64, v11: i64):
    v42 = iconst.i64 0
    v12 = icmp eq v10, v42
    brif v12, block3(v11), block2
block2:
    v43 = iconst.i64 3
    v13 = ishl v11, v43
    v14 = iadd v0, v13
    store notrap v10, v14
    v45 = iconst.i64 -1
    v15 = iadd v10, v45
    v16 = iadd v11, v45
    v17 = icmp ugt v16, v7
    brif v17, block1(v15, v16), block4(v16)
block3(v20: i64):
    return v20
block4(v21: i64):
    v46 = iconst.i64 1000
    v22 = iadd v21, v46
    return v22
}";
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::mintrip::run(&mut after, &mut Analysis::default()), 1, "{}", after.display());
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags).unwrap_or_else(|e| panic!("{e}\n{}", after.display()));
    let s = check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", after.display()));
    assert!(s.compared > 0, "nothing compared: {:?}", s.why);
}

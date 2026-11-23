//! mintrip: a fill loop with two exits (`rem != 0`, `len + 1 <u cap`;
//! SmallVec's `extend(repeat(x).take(n))`) gets a counted pre-loop of
//! `umin` trips before the original loop. Differentially checked,
//! including which exit is taken; a loop with a non-affine exit is left
//! alone.

use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

/// `PLIRON_MINTRIP_CANON=1` in every test: the knob is read once per
/// process, and the other tests must hold with it on.
fn canon_on() {
    unsafe { std::env::set_var("PLIRON_MINTRIP_CANON", "1") };
}

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
    canon_on();
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
    canon_on();
    // The capacity test reads memory: not a linear form.
    let src = SRC.replace("v17 = icmp ult v15, v6", "v30 = load.i64 notrap v14\n    v17 = icmp ult v15, v30");
    let mut f = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    assert_eq!(pliron_clif_opt::mintrip::run(&mut f, &mut Analysis::default()), 0, "{}", f.display());
}

/// Two down-counting exits (`rem != 0`, `q >u lim`) and no param stepping
/// by +1: the pre-loop counts with a `j` of its own.
#[test]
fn no_unit_step_param_counts_with_j() {
    canon_on();
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

/// blind-sig's `SmallVec::extend(repeat(x).take(n))` after unrolling and
/// jump threading: the latch is a block with params of its own, entered
/// only from `jump block14(v16, ..)`, so the back edge's `v84 + 1` is
/// `v16 + 1` and `v86` is `v68 - 1` only through that block's params.
/// Without `PLIRON_MINTRIP_CANON` the header params did not count as
/// stepping and the loop kept both exit tests per element.
#[test]
fn steps_through_latch_block_params() {
    canon_on();
    let src = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v40 = iconst.i64 7
    v41 = iconst.i64 15
    v5 = band v1, v40
    v6 = band v2, v41
    v7 = band v3, v41
    jump block1(v5, v7)
block1(v16: i64, v68: i64):
    v17 = icmp ult v16, v6
    brif v17, block4, block2(v16)
block2(v20: i64):
    v46 = iconst.i64 1000
    v22 = iadd v20, v46
    return v22
block4:
    v94 = iconst.i64 0
    v95 = icmp eq v68, v94
    brif v95, block27(v16), block28
block27(v87: i64):
    return v87
block28:
    v47 = iconst.i64 1
    v48 = isub v68, v47
    v45 = iconst.i64 9
    jump block14(v16, v45, v48)
block14(v84: i64, v85: i64, v86: i64):
    v89 = iconst.i64 3
    v90 = ishl v84, v89
    v60 = iadd v0, v90
    store notrap v85, v60
    v91 = iconst.i64 1
    v62 = iadd v84, v91
    jump block1(v62, v86)
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

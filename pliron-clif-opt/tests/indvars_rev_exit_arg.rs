//! The wasm reverse-index stepping (`base + (A - iv)` as a pointer
//! stepped down) may only delete the `A - iv` sub when the address is
//! its sole reader. `inst_users` was built from `inst_args`, which
//! leaves out branch arguments, so a sub that is also passed on the
//! loop's exit edge looked dead and was removed: the `brif` kept an
//! argument "defined by an inst which has no block". Five functions of
//! `jiff` (decimal printers writing digits from the end of a buffer and
//! returning the last index) stopped the wasm build of `~/code/dsa` in
//! `licm2`'s second indvars round.

use cranelift_codegen::settings::{self, Configurable};
use pliron_clif_opt::pipeline::Analysis;

/// `for i in 0.. { idx = (n - 1) - i; buf[idx] = d; if done { return idx } }`
const SRC: &str = "function u0:0(i32, i32, i64) -> i32 fast {
block0(v0: i32, v1: i32, v2: i64):
    v3 = iconst.i32 1
    v4 = isub v1, v3
    v5 = iconst.i32 0
    v6 = iconst.i64 10
    v20 = iconst.i64 0
    jump block1(v5, v2)

block1(v7: i32, v8: i64):
    v9 = udiv v8, v6
    v10 = ireduce.i8 v8
    v11 = isub v4, v7
    v12 = iadd v0, v11
    store notrap v10, v12
    v13 = icmp eq v9, v20
    v14 = iadd v7, v3
    brif v13, block2(v11), block1(v14, v9)

block2(v15: i32):
    return v15
}";

#[test]
fn sub_passed_on_the_exit_edge_stays_defined() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut derived = pliron_clif_opt::vmap::VMap::default();
    // `amode_scale` false is the wasm configuration.
    pliron_clif_opt::indvars::run(&mut f, &mut Analysis::default(), false, &mut derived);
    let mut b = settings::builder();
    b.enable("enable_verifier").unwrap();
    cranelift_codegen::verifier::verify_function(&f, &settings::Flags::new(b))
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
}

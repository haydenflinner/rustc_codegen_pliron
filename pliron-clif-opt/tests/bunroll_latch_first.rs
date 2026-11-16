//! bunroll clones a loop's blocks in `plan.blocks` order, which is not
//! dominance order. With the latch laid out ahead of the body, the
//! latch's clone was made before the body's values had copies, and its
//! back-edge arguments kept naming the original loop:
//! `brif c, h'(i, v512, v513)` "uses value v512 from non-dominating
//! inst" (rapier2d's `smallest_abs_diff_between_angles::<WideF32x4>`,
//! a four-lane loop; the wasm build of `~/code/dsa` stopped there, and
//! the same input fails natively). The clones are now remapped once
//! the whole copy exists.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};
use pliron_clif_opt::pipeline::Analysis;

/// Four trips. The loop is header `block2`, latch `block3`, body
/// `block4`, laid out in that order, and the latch passes the body's
/// `v8` around the back edge.
const SRC: &str = "function u0:0(i64) -> i64 fast {
block0(v0: i64):
    v1 = iconst.i32 0
    v2 = iconst.i64 0
    jump block2(v1, v2)

block2(v6: i32, v7: i64):
    jump block4

block3(v3: i32):
    v4 = iconst.i32 4
    v5 = icmp ult v3, v4
    brif v5, block2(v3, v8), block5

block4:
    v9 = uextend.i64 v6
    v13 = iadd v0, v9
    v14 = load.i64 notrap v13
    v8 = iadd v7, v14
    v11 = iconst.i32 1
    v12 = iadd v6, v11
    jump block3(v12)

block5:
    return v8
}";

fn verify(f: &Function) {
    let mut b = settings::builder();
    b.enable("enable_verifier").unwrap();
    cranelift_codegen::verifier::verify_function(f, &settings::Flags::new(b))
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
}

#[test]
fn latch_laid_out_before_the_body() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    verify(&f);
    let n = pliron_clif_opt::bunroll::run(&mut f, &mut Analysis::default());
    assert_eq!(n, 1, "the four-trip loop was not unrolled:\n{}", f.display());
    verify(&f);
}

//! `PLIRON_STUBHOIST` (default-on): a single-pred block of pure insts ending
//! `jump t(args)` folds into the pred's edge — the insts clone before the
//! pred terminator and the edge retargets at `t`. fib's leaf stub
//! (`uext n; jump ret(v)`) disappears, so the entry `brif` arm points
//! straight at the shared return like LLVM's. `brif` preds inside loops
//! are skipped (the clone would be dead work on the loop-carried arm).

use cranelift_codegen::ir::Function;
use pliron_clif_opt::stubhoist;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// fib's post-`tailrec` shape: `block5` computes `uext n` only to jump
/// to the shared return; `block7` does the same for the loop exit but
/// sits on the latch's else arm — folding it would hoist `uext`/`iadd`
/// into the loop body.
const FIB: &str = "function %f(i32) -> i64 {
    sig0 = (i32) -> i64
    fn0 = %g sig0

block0(v0: i32):
    v3 = iconst.i32 2
    v20 = icmp ult v0, v3
    brif v20, block5, block6

block6:
    v16 = iconst.i64 0
    v39 = iconst.i32 -1
    v41 = iconst.i32 -2
    v43 = iconst.i32 2
    jump block1(v0, v16)

block1(v27: i32, v28: i64):
    v40 = iadd v27, v39
    v7 = call fn0(v40)
    v42 = iadd v27, v41
    v44 = icmp uge v42, v43
    v18 = iadd v28, v7
    brif v44, block1(v42, v18), block7

block7:
    v36 = uextend.i64 v42
    v37 = iadd v36, v18
    jump block2(v37)

block5:
    v33 = uextend.i64 v0
    jump block2(v33)

block2(v38: i64):
    return v38
}";

#[test]
fn leaf_stub_folds_into_brif_arm() {
    let mut f = parse(FIB);
    let k = stubhoist::run(&mut f);
    assert_eq!(k, 1, "one edge folded");
    let s = f.to_string();
    // The leaf arm points at the shared return with the uext computed
    // in block0; the stub is gone and the loop-exit stub survives
    // (its pred is the in-loop latch).
    assert!(s.contains("uextend.i64 v0"), "{s}");
    assert!(s.contains("block2(") && s.contains("brif v20, block2"), "{s}");
    assert!(!s.contains("block5"), "{s}");
    assert!(s.contains("block7:"), "{s}");
}

/// A `jump` pred folds even inside a loop — the stub's body ran
/// unconditionally, so moving it into the pred changes nothing. The
/// latch's `brif`-edge stub (`block7`-style) stays skipped.
const JUMPPRED: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v3 = iconst.i64 0
    jump block1(v0, v3)

block1(v10: i64, v11: i64):
    v12 = iconst.i64 1
    v13 = iadd v10, v12
    v14 = icmp uge v13, v11
    brif v14, block2, block3(v13)

block2:
    return v10

block3(v15: i64):
    jump block5(v15)

block5(v16: i64):
    v17 = iadd v16, v11
    jump block1(v17, v11)
}";

#[test]
fn jump_pred_folds_in_loop() {
    let mut f = parse(JUMPPRED);
    let k = stubhoist::run(&mut f);
    assert_eq!(k, 1, "jump pred folds");
    let s = f.to_string();
    assert!(!s.contains("block5:"), "{s}");
    // The cloned iadd sits in block3 before its retargeted jump.
    assert!(s.contains("iadd v15, v11"), "{s}");
    assert!(s.contains("jump block1("), "{s}");
}

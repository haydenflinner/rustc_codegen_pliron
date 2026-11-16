//! `errcold`: const-only CondBr→join Err tails become cold; Ok arms with
//! a live payload do not.

use cranelift_codegen::ir::Function;
use cranelift_reader::parse_functions;

fn parse(src: &str) -> Function {
    parse_functions(src).unwrap().remove(0)
}

#[test]
fn marks_err_tail_not_ok() {
    let mut f = parse(
        "function %f(i32, i32) -> i32 {
block0(v0: i32, v1: i32):
    brif v0, block1, block2
block1:
    v2 = iconst.i32 1
    v3 = iconst.i32 0
    jump block3(v2, v3)
block2:
    v4 = iconst.i32 0
    jump block3(v4, v1)
block3(v5: i32, v6: i32):
    return v5
}",
    );
    let n = pliron_clif_opt::clifpeep::errcold(&mut f);
    assert_eq!(n, 1, "exactly the Err arm");
    let blocks: Vec<_> = f.layout.blocks().collect();
    // block1 = Err (const args), block2 = Ok (live v1)
    assert!(f.layout.is_cold(blocks[1]), "Err tail cold");
    assert!(!f.layout.is_cold(blocks[2]), "Ok arm stays warm");
    assert!(!f.layout.is_cold(blocks[3]), "join stays warm");
}

#[test]
fn skips_when_both_arms_carry_live() {
    let mut f = parse(
        "function %f(i32, i32, i32) -> i32 {
block0(v0: i32, v1: i32, v2: i32):
    brif v0, block1, block2
block1:
    jump block3(v1)
block2:
    jump block3(v2)
block3(v3: i32):
    return v3
}",
    );
    assert_eq!(pliron_clif_opt::clifpeep::errcold(&mut f), 0);
}

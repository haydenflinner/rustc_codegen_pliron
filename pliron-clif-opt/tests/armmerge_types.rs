//! `armmerge` paired arm insts by `InstructionData` alone, which doesn't
//! carry the controlling type: `load.i8 p` and `load.i16 p` (or
//! `iconst.i8 0` and `iconst.i32 0`) looked like twins, so the merged
//! block ran T's narrow load on E's path too — E's `uextend` saw one byte
//! instead of two. The `iconst` form surfaced as a verifier error in
//! `compiler_builtins::int::mul::i32_overflowing_mul` once the inliner
//! stopped unrolling recursion (`icmp.i32` fed an `i8` zero).

use cranelift_codegen::ir::Function;
use pliron_clif_opt::armmerge;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const ARMS: &str = "function %f(i8, i64) -> i64 {
    block0(v0: i8, v9: i64):
        jump block4

    block4:
        brif v0, block1, block2

    block1:
        v1 = load.i8 notrap aligned v9
        v2 = uextend.i64 v1
        jump block3(v2)

    block2:
        v3 = load.i16 notrap aligned v9
        v4 = uextend.i64 v3
        jump block3(v4)

    block3(v5: i64):
        return v5
    }";

#[test]
fn arms_differing_only_in_type_stay_apart() {
    let mut f = parse(ARMS);
    let n = armmerge::run(&mut f);
    assert_eq!(n, 0, "load.i8/load.i16 arms must not merge:\n{}", f.display());
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap();
}

//! `selord_sel` rewrote `select c, 1, 0` (an i8 bool) to
//! `icmp ne c, iconst.i8 0` whatever `c`'s width, and aliased the result
//! straight to a 0/1-valued `c` even when `c` was wider than i8. armmerge
//! builds exactly that shape from twin overflow-check arms —
//! `select (band.i32 x, 0x8000_0000), 1, 0` in
//! `compiler_builtins::int::mul::i32_overflowing_mul` — and the verifier
//! rejected `icmp.i32 ne v288, v303` with `v303` an i8 zero.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(f, &flags).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
}

#[test]
fn select_on_wide_cond_keeps_cond_type() {
    for (src, what) in [
        (
            "function %f(i32) -> i8 {
            block0(v0: i32):
                v1 = iconst.i32 0x8000_0000
                v2 = band v0, v1
                v3 = iconst.i8 1
                v4 = iconst.i8 0
                v5 = select v2, v3, v4
                return v5
            }",
            "select c, 1, 0",
        ),
        (
            "function %f(i32) -> i8 {
            block0(v0: i32):
                v1 = iconst.i32 1
                v2 = band v0, v1
                v3 = iconst.i8 0
                v4 = iconst.i8 1
                v5 = select v2, v3, v4
                return v5
            }",
            "select c, 0, 1",
        ),
        (
            // 0/1-valued i32 `c`: aliasing the i8 result to it is a type error.
            "function %f(i32) -> i8 {
            block0(v0: i32):
                v1 = iconst.i32 7
                v2 = icmp ult v0, v1
                v3 = uextend.i32 v2
                v4 = iconst.i8 1
                v5 = iconst.i8 0
                v6 = select v3, v4, v5
                return v6
            }",
            "select (uextend.i32 bool), 1, 0",
        ),
    ] {
        let before = parse(src);
        let mut after = before.clone();
        pliron_clif_opt::clifpeep::iconst128(&mut after, true);
        assert!(!after.display().to_string().contains("select"), "{what}: not rewritten");
        verify(&after);
        check(&before, &after, &Facts::default(), &Config::default())
            .unwrap_or_else(|e| panic!("{what}: {e}"));
    }
}

//! bunroll exit-cone LCSSA repair: postcard's `try_take_varint_*`
//! decoders (`for i in 0..varint_max` loops) fail the old free-var
//! check because the `done` exit validates `i == max` — a loop-defined
//! value read directly by an out-of-loop block. The cone walk now
//! threads those values through the exit edges as block params so the
//! loop is unrollable; cloned exits forward the cloned values.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::pipeline::Analysis;

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("{e}\n{}", f.display());
    }
}

fn unroll(src: &str) -> (usize, Function) {
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    verify(&f);
    let n = pliron_clif_opt::bunroll::run(&mut f, &mut Analysis::default());
    (n, f)
}

/// `i` counted `0..5`; the `done` exit reads `v2` (the counter) and,
/// one block deeper, `v3` (the accumulator) — free uses that once
/// forced `leak-arg`.
fn leak_src() -> &'static str {
    "function u0:0(i64) -> i64 fast {
block0(v0: i64):
    v1 = iconst.i64 0
    v9 = iconst.i64 1
    jump block1(v1, v9)

block1(v2: i64, v3: i64):
    v4 = iconst.i64 5
    v5 = icmp ult v2, v4
    brif v5, block2, block3

block2:
    v6 = iconst.i64 1
    v7 = iadd v2, v6
    v8 = iadd v3, v0
    jump block1(v7, v8)

block3:
    v10 = iconst.i64 5
    v11 = icmp eq v2, v10
    brif v11, block4, block5

block4:
    return v3

block5:
    return v0
}"
}

#[test]
fn leaked_counter_and_accumulator_unroll() {
    unsafe { std::env::set_var("PLIRON_BUNROLL", "1") };
    let before = cranelift_reader::parse_functions(leak_src()).unwrap().remove(0);
    let (n, f) = unroll(leak_src());
    assert!(n > 0, "{}", f.display());
    // Unrolled: the original header `block1` is reached only from the
    // last copy — every earlier copy exits straight into `block3`.
    verify(&f);
    let s = pliron_clif_opt::diffcheck::check(
        &before,
        &f,
        &Default::default(),
        &pliron_clif_opt::diffcheck::Config {
            trials: 64,
            ..Default::default()
        },
    )
    .unwrap_or_else(|m| panic!("{m}\n{}", f.display()));
    assert!(s.compared > 0);
}

/// Same loop with no leaked uses — the repair path is empty and the
/// plain unroll is unchanged.
#[test]
fn leak_free_still_unrolls() {
    unsafe { std::env::set_var("PLIRON_BUNROLL", "1") };
    let src = "function u0:0(i64) -> i64 fast {
block0(v0: i64):
    v1 = iconst.i64 0
    v9 = iconst.i64 1
    jump block1(v1, v9)

block1(v2: i64, v3: i64):
    v4 = iconst.i64 5
    v5 = icmp ult v2, v4
    brif v5, block2, block3

block2:
    v6 = iconst.i64 1
    v7 = iadd v2, v6
    v8 = iadd v3, v0
    jump block1(v7, v8)

block3:
    return v0
}";
    let (n, f) = unroll(src);
    assert!(n > 0, "{}", f.display());
    verify(&f);
}

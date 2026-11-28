//! `PLIRON_DEMANDED=1`: demanded bits. Masks nobody reads go; narrow
//! `i8`/`i16` chains that are extended or stored narrow are rebuilt wide.
//! Every shape runs through the differential checker; the ones that must
//! change assert the change, so `PLIRON_DEMANDED=0` fails them.

use cranelift_codegen::ir::{Function, Opcode, types};
use pliron_cfg::target::Target;
use pliron_clif_opt::demanded::run;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn knob() {
    if std::env::var_os("PLIRON_DEMANDED").is_none() {
        unsafe { std::env::set_var("PLIRON_DEMANDED", "1") };
    }
}

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

/// Run the step for `target`, check it against the original, return the
/// result and the number of rewrites.
fn go(src: &str, target: &Target) -> (Function, usize) {
    knob();
    let before = parse(src);
    let mut after = before.clone();
    let n = run(&mut after, target);
    cranelift_codegen::verifier::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("verifier: {e}\n{after}"));
    let s = check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() })
        .unwrap_or_else(|m| panic!("demanded changed behaviour: {m:?}\n{after}"));
    assert!(s.compared > 0, "no trial compared: {:?}", s.why);
    (after, n)
}

/// `(y as u8).wrapping_add(0xe0) as u64 * 96`: the `ireduce`'s `uxtb` and
/// the root's are one `and #0xff` on the wide add.
#[test]
fn extend_of_narrow_add_widens_with_a_mask() {
    let (f, n) = go(
        "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = ireduce.i8 v0
    v2 = iconst.i8 0xe0
    v3 = iadd v1, v2
    v4 = uextend.i64 v3
    v5 = iconst.i64 96
    v6 = imul v4, v5
    return v6
}",
        &Target::host(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Ireduce), 0, "{f}");
    assert_eq!(count(&f, Opcode::Uextend), 0, "{f}");
    assert_eq!(count(&f, Opcode::Band), 1, "{f}");
}

/// The extended bits are masked off again by every use (`& 0x7f`), so the
/// root needs no mask either.
#[test]
fn undemanded_extension_goes() {
    let (f, n) = go(
        "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = ireduce.i8 v0
    v3 = ireduce.i8 v1
    v4 = isub v2, v3
    v5 = uextend.i64 v4
    v6 = iconst.i64 0x7f
    v7 = band v5, v6
    return v7
}",
        &Target::host(),
    );
    assert!(n >= 1, "{f}");
    assert_eq!(count(&f, Opcode::Ireduce), 0, "{f}");
    assert_eq!(count(&f, Opcode::Uextend), 0, "{f}");
    assert_eq!(count(&f, Opcode::Band), 1, "{f}");
}

/// A narrow store of a narrow add stores the wide add with `istore8`.
#[test]
fn narrow_store_of_add_stores_wide() {
    let (f, n) = go(
        "function %f(i64, i32) -> i8 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64, v1: i32):
    v2 = stack_addr.i64 ss0
    v3 = ireduce.i8 v0
    v4 = ireduce.i8 v1
    v5 = imul v3, v4
    v6 = iconst.i8 3
    v7 = ishl v5, v6
    store notrap aligned v7, v2
    v8 = load.i8 notrap aligned v2
    return v8
}",
        &Target::host(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Istore8), 1, "{f}");
    assert_eq!(count(&f, Opcode::Ireduce), 1, "{f}"); // the i64 -> i32
}

/// `band x, 0xff` read only by `istore8` is `x`.
#[test]
fn mask_under_byte_store_goes() {
    let (f, n) = go(
        "function %f(i64, i32) -> i8 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64, v1: i32):
    v2 = stack_addr.i64 ss0
    v3 = iconst.i32 0xff
    v4 = band v1, v3
    istore8 notrap aligned v4, v2
    v8 = load.i8 notrap aligned v2
    return v8
}",
        &Target::host(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Band), 0, "{f}");
}

/// A mask on a loop-carried value that also leaves the loop whole stays:
/// the exit argument's demand reaches the `band` through the back edge.
#[test]
fn loop_carried_demand_keeps_the_mask() {
    let (f, n) = go(
        "function %f(i32, i32) -> i32 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i32, v1: i32):
    v2 = stack_addr.i64 ss0
    jump block1(v0, v1)
block1(v3: i32, v4: i32):
    v5 = iconst.i32 0xff
    v6 = band v3, v5
    istore8 notrap aligned v6, v2
    v7 = iconst.i32 0x101
    v8 = iadd v6, v7
    v9 = iconst.i32 1
    v10 = isub v4, v9
    brif v10, block1(v8, v10), block2(v8)
block2(v11: i32):
    return v11
}",
        &Target::host(),
    );
    assert_eq!(n, 0, "{f}");
    assert_eq!(count(&f, Opcode::Band), 1, "{f}");
}

/// The same loop with only the low byte leaving: the mask goes.
#[test]
fn loop_carried_low_byte_drops_the_mask() {
    let (f, n) = go(
        "function %f(i32, i32) -> i8 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i32, v1: i32):
    v2 = stack_addr.i64 ss0
    jump block1(v0, v1)
block1(v3: i32, v4: i32):
    v5 = iconst.i32 0xff
    v6 = band v3, v5
    istore8 notrap aligned v6, v2
    v7 = iconst.i32 0x101
    v8 = iadd v6, v7
    v9 = iconst.i32 1
    v10 = isub v4, v9
    brif v10, block1(v8, v10), block2(v8)
block2(v11: i32):
    v12 = ireduce.i8 v11
    return v12
}",
        &Target::host(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Band), 0, "{f}");
}

/// A sign extension whose sign bits nobody reads widens like a zero
/// extension, with no mask.
#[test]
fn sextend_with_undemanded_sign() {
    let (f, n) = go(
        "function %f(i64, i64) -> i32 {
block0(v0: i64, v1: i64):
    v2 = ireduce.i16 v0
    v3 = ireduce.i16 v1
    v4 = iadd v2, v3
    v5 = sextend.i32 v4
    v6 = iconst.i32 0xfff
    v7 = band v5, v6
    return v7
}",
        &Target::host(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Sextend), 0, "{f}");
    assert_eq!(count(&f, Opcode::Ireduce), 2, "{f}"); // to i32
}

/// A demanded sign extension stays.
#[test]
fn demanded_sextend_stays() {
    let (f, n) = go(
        "function %f(i64, i64) -> i32 {
block0(v0: i64, v1: i64):
    v2 = ireduce.i16 v0
    v3 = ireduce.i16 v1
    v4 = iadd v2, v3
    v5 = sextend.i32 v4
    return v5
}",
        &Target::host(),
    );
    assert_eq!(n, 0, "{f}");
    assert_eq!(count(&f, Opcode::Sextend), 1, "{f}");
}

/// wasm: two narrow adds of truncated words, zero-extended, cost five
/// `& 255` (one per `ireduce`, one per add); the wide chain costs the
/// root's one.
#[test]
fn wasm_chain_of_byte_adds() {
    let (f, n) = go(
        "function %f(i32, i32, i32) -> i32 {
block0(v0: i32, v1: i32, v2: i32):
    v3 = ireduce.i8 v0
    v4 = ireduce.i8 v1
    v5 = ireduce.i8 v2
    v6 = iadd v3, v4
    v7 = iadd v6, v5
    v8 = uextend.i32 v7
    return v8
}",
        &Target::wasm32(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Band), 1, "{f}");
    assert_eq!(count(&f, Opcode::Ireduce), 0, "{f}");
    assert_eq!(count(&f, Opcode::Iadd), 2, "{f}");
}

/// wasm: the same chain over block parameters (each would need its own
/// mask) is left alone.
#[test]
fn wasm_chain_of_params_stays() {
    let (f, n) = go(
        "function %f(i8, i8, i8) -> i32 {
block0(v0: i8, v1: i8, v2: i8):
    v6 = iadd v0, v1
    v7 = iadd v6, v2
    v8 = uextend.i32 v7
    return v8
}",
        &Target::wasm32(),
    );
    assert_eq!(n, 0, "{f}");
}

/// gb18030's `(b - 0x20) as u64 * 96` and `(b - 0x20) as u32 * 96`: one
/// narrow add read by two extensions of different widths is rebuilt once,
/// at the wider, and the narrower root reads it reduced.
#[test]
fn shared_top_feeds_two_roots() {
    let (f, n) = go(
        "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = ireduce.i8 v0
    v2 = iconst.i8 -32
    v3 = iadd v1, v2
    v4 = uextend.i64 v3
    v5 = iconst.i64 96
    v6 = imul v4, v5
    v7 = uextend.i32 v3
    v8 = iconst.i32 96
    v9 = imul v7, v8
    v10 = uextend.i64 v9
    v11 = iadd v6, v10
    return v11
}",
        &Target::host(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Iadd), 2, "{f}"); // the wide add and the sum
    assert_eq!(count(&f, Opcode::Uextend), 1, "{f}"); // v10
}

/// A byte carried around a loop (`c = c.wrapping_add(k)`) and stored and
/// zero-extended at the exit: the parameter becomes a word, the per-trip
/// `ireduce` goes.
#[test]
fn loop_parameter_web() {
    let (f, n) = go(
        "function %f(i64, i32) -> i64 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64, v1: i32):
    v2 = stack_addr.i64 ss0
    v3 = ireduce.i8 v0
    jump block1(v3, v1)
block1(v4: i8, v5: i32):
    v6 = ireduce.i8 v5
    v7 = iadd v4, v6
    store notrap aligned v7, v2
    v8 = iconst.i32 1
    v9 = isub v5, v8
    brif v9, block1(v7, v9), block2(v7)
block2(v10: i8):
    v11 = uextend.i64 v10
    v12 = load.i8 notrap aligned v2
    v13 = uextend.i64 v12
    v14 = iadd v11, v13
    return v14
}",
        &Target::host(),
    );
    assert_eq!(n, 1, "{f}");
    assert_eq!(count(&f, Opcode::Ireduce), 0, "{f}");
    assert_eq!(count(&f, Opcode::Istore8), 1, "{f}");
}


/// miniz_oxide's dictionary fill loop: the byte entering the hash chain is
/// an in-loop `ireduce.i16` of an i32, the hash is loop-carried, and every
/// extended bit is masked off at the roots. The conversion the widening
/// removes executes per trip; the one it adds for the entry seed executes
/// once, so loop depth breaks their static-cost tie.
#[test]
fn loop_carried_hash_chain_widens() {
    let (f, n) = go(
        "function %f(i16, i32, i32) -> i64 {
block0(v0: i16, v1: i32, v2: i32):
    jump block1(v0, v1, v2)
block1(v3: i16, v4: i32, v5: i32):
    v6 = ireduce.i16 v4
    v7 = bxor v3, v6
    v8 = iconst.i16 0x7fff
    v9 = band v7, v8
    v10 = uextend.i64 v7
    v11 = iconst.i64 0x7fff
    v12 = band v10, v11
    v13 = iconst.i16 5
    v14 = ishl v9, v13
    v15 = iconst.i32 1
    v16 = iadd v4, v15
    v17 = iconst.i32 0xffff
    v18 = band v16, v17
    v19 = isub v5, v15
    brif v19, block1(v14, v18, v19), block2(v9)
block2(v20: i16):
    v21 = uextend.i64 v20
    return v21
}",
        &Target::host(),
    );
    assert!(n >= 1, "{f}");
    assert_eq!(count(&f, Opcode::Ireduce), 0, "{f}");
    let b1 = f.layout.blocks().nth(1).unwrap();
    assert_eq!(f.dfg.value_type(f.dfg.block_params(b1)[0]), types::I32, "{f}");
}

/// A web member with a use outside the web (a compare) keeps the whole web
/// narrow.
#[test]
fn hard_use_keeps_the_web() {
    let (f, n) = go(
        "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = ireduce.i8 v0
    v2 = iconst.i8 -32
    v3 = iadd v1, v2
    v4 = uextend.i64 v3
    v5 = iconst.i8 7
    v6 = icmp ult v3, v5
    v7 = uextend.i64 v6
    v8 = iadd v4, v7
    return v8
}",
        &Target::host(),
    );
    assert_eq!(n, 0, "{f}");
}

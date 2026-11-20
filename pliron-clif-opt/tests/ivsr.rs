//! ivsr: `base + (i + k) * S` in a loop, `S` not a power of two, becomes a
//! pointer header parameter stepped by `S` on the back edge (brotli's
//! 20-byte `nodes[i + 1]`). Differentially checked against the interpreter.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn run(src: &str) -> (Function, Function, usize) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::ivsr::run_with(&mut after, &mut Analysis::default(), 0);
    (before, after, k)
}

fn checked(before: &Function, after: &Function) {
    let mut cfg = Config::default();
    cfg.trials = 64;
    if let Err(m) = check(before, after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

/// Sum of the `i32` at offset 4 of 20-byte records `start + 1 .. n`
/// (`nodes[i + 1].cost`), plus the tag at offset 0 in a second address
/// with another constant part.
const NODES: &str = "function %f(i64, i64, i64) -> i32 {
    ss0 = explicit_slot 400, align = 8
block0(v0: i64, v1: i64, v2: i64):
    v3 = stack_addr.i64 ss0
    v4 = iconst.i32 0
    v5 = iconst.i64 18
    v6 = band v1, v5
    jump block1(v6, v4)
block1(v7: i64, v8: i32):
    v9 = iconst.i64 1
    v10 = iadd v7, v9
    v11 = iconst.i64 20
    v12 = imul v10, v11
    v13 = iadd v3, v12
    v14 = load.i32 notrap v13+4
    v15 = imul v7, v11
    v16 = iadd v15, v3
    v17 = load.i32 notrap v16
    v18 = iadd v8, v14
    v19 = iadd v18, v17
    v20 = icmp ult v10, v5
    brif v20, block1(v10, v19), block2(v19)
block2(v21: i32):
    return v21
}";

#[test]
fn record_address_becomes_pointer_iv() {
    let (before, after, k) = run(NODES);
    assert_eq!(k, 2, "{}", after.display());
    // Both loads in the loop read the new header parameter (directly or
    // plus a constant), not a product.
    let b1 = after.layout.blocks().nth(1).unwrap();
    let p = *after.dfg.block_params(b1).last().unwrap();
    for i in after.layout.block_insts(b1) {
        if after.dfg.insts[i].opcode() != Opcode::Load {
            continue;
        }
        let a = after.dfg.resolve_aliases(after.dfg.inst_args(i)[0]);
        let from_p = a == p
            || after.dfg.value_def(a).inst().is_some_and(|d| {
                after.dfg.insts[d].opcode() == Opcode::Iadd && after.dfg.inst_args(d).iter().any(|&x| after.dfg.resolve_aliases(x) == p)
            });
        assert!(from_p, "load {} not off the pointer IV:\n{}", after.dfg.display_inst(i), after.display());
    }
    // The only multiply that stays live is on the entry edge.
    let b0 = after.layout.entry_block().unwrap();
    assert!(after.layout.block_insts(b0).any(|i| after.dfg.insts[i].opcode() == Opcode::Imul), "{}", after.display());
    checked(&before, &after);
}

/// A power-of-two stride folds into the address mode: left alone.
#[test]
fn pow2_stride_stays() {
    let (_, after, k) = run(&NODES.replace("iconst.i64 20", "iconst.i64 16"));
    assert_eq!(k, 0, "{}", after.display());
}

/// A base defined inside the loop is not invariant: left alone.
#[test]
fn variant_base_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i32 {
block0(v0: i64, v1: i64):
    v4 = iconst.i32 0
    jump block1(v0, v4)
block1(v7: i64, v8: i32):
    v9 = iconst.i64 1
    v10 = iadd v7, v9
    v11 = iconst.i64 20
    v12 = imul v10, v11
    v3 = load.i64 notrap v1
    v13 = iadd v3, v12
    v14 = load.i32 notrap v13
    v18 = iadd v8, v14
    v20 = icmp ult v10, v11
    brif v20, block1(v10, v18), block2(v18)
block2(v21: i32):
    return v21
}");
    assert_eq!(k, 0, "{}", after.display());
}

/// An address computed only in a conditional arm: the new parameter would
/// be live on every trip for a multiply saved on some. Left alone.
#[test]
fn arm_only_address_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i32 {
    ss0 = explicit_slot 400, align = 8
block0(v0: i64, v1: i64):
    v3 = stack_addr.i64 ss0
    v4 = iconst.i32 0
    v5 = iconst.i64 18
    v6 = band v0, v5
    jump block1(v6, v4)
block1(v7: i64, v8: i32):
    v9 = iconst.i64 1
    v10 = iadd v7, v9
    v30 = band v7, v9
    brif v30, block2, block3(v8)
block2:
    v11 = iconst.i64 20
    v12 = imul v10, v11
    v13 = iadd v3, v12
    v14 = load.i32 notrap v13
    v18 = iadd v8, v14
    jump block3(v18)
block3(v19: i32):
    v20 = icmp ult v10, v5
    brif v20, block1(v10, v19), block4(v19)
block4(v21: i32):
    return v21
}");
    assert_eq!(k, 0, "{}", after.display());
}

/// The loop already carries a pointer stepping by `S*c`: no second one.
#[test]
fn existing_pointer_iv_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i32 {
    ss0 = explicit_slot 400, align = 8
block0(v0: i64, v1: i64):
    v3 = stack_addr.i64 ss0
    v4 = iconst.i32 0
    v5 = iconst.i64 18
    v6 = band v0, v5
    jump block1(v6, v4, v3)
block1(v7: i64, v8: i32, v22: i64):
    v9 = iconst.i64 1
    v10 = iadd v7, v9
    v11 = iconst.i64 20
    v12 = imul v10, v11
    v13 = iadd v3, v12
    v14 = load.i32 notrap v13
    v15 = load.i32 notrap v22
    v18 = iadd v8, v14
    v19 = iadd v18, v15
    v23 = iadd v22, v11
    v20 = icmp ult v10, v5
    brif v20, block1(v10, v19, v23), block2(v19)
block2(v21: i32):
    return v21
}");
    assert_eq!(k, 0, "{}", after.display());
}

/// The address is the back-edge argument of a header parameter: the loop
/// carries that pointer already (rebuilt from the index each trip).
#[test]
fn carried_address_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i32 {
    ss0 = explicit_slot 400, align = 8
block0(v0: i64, v1: i64):
    v3 = stack_addr.i64 ss0
    v4 = iconst.i32 0
    v5 = iconst.i64 18
    v6 = band v0, v5
    v11 = iconst.i64 20
    v30 = imul v6, v11
    v31 = iadd v3, v30
    jump block1(v6, v4, v31)
block1(v7: i64, v8: i32, v22: i64):
    v9 = iconst.i64 1
    v10 = iadd v7, v9
    v15 = load.i32 notrap v22
    v19 = iadd v8, v15
    v12 = imul v10, v11
    v13 = iadd v3, v12
    v20 = icmp ult v10, v5
    brif v20, block1(v10, v19, v13), block2(v19)
block2(v21: i32):
    return v21
}");
    assert_eq!(k, 0, "{}", after.display());
}

/// `PLIRON_IVSR=2`'s choice: both versions compile, both costs are read, and
/// the function is replaced only when the copy is cheaper.
#[cfg(target_arch = "aarch64")]
#[test]
fn pick_compiles_both() {
    use cranelift_codegen::settings::{self, Configurable};
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin").unwrap().finish(settings::Flags::new(b)).unwrap();
    let mut f = cranelift_reader::parse_functions(&NODES.replace("function %f(i64, i64, i64) -> i32 {", "function %f(i64, i64, i64) -> i32 apple_aarch64 {")).unwrap().remove(0);
    let before = f.clone();
    let (k, a, b, take) = pliron_clif_opt::ivsr::pick(&mut f, &*isa, 0.0).expect("ivsr fires");
    assert_eq!(k, 2);
    let (a, b) = (a.expect("cost before"), b.expect("cost after"));
    assert!(a[0] > 0.0 && b[0] > 0.0 && a[1] <= a[0] && b[1] <= b[0], "{a:?} {b:?}");
    assert_eq!(take, b[0] < a[0] && b[1] <= a[1]);
    assert_eq!(take, f.display().to_string() != before.display().to_string());
}

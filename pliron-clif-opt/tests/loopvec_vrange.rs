//! `PLIRON_VRANGE`: a `band`ed closed-range keep mask in a vectorized
//! count (`iter().filter(|x| (lo..=hi).contains(x)).count()`) emits the
//! strict-out-of-range form `~((x < lo) | (x > hi))` — one shared sign
//! flip of the data, invariant bound flips that hoist, one complement
//! total — instead of two self-contained `ule`/`uge` lowers (~12 vs ~6
//! insts per lane group on SSE2). Opt-in; `PLIRON_VRANGE=0` keeps the
//! pairwise-`band` emission.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Function, InstructionData, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};
use target_lexicon::PointerWidth;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn tcfg() -> TargetFrontendConfig {
    TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    }
}

fn run_vec(f: &mut Function, name: &str) -> usize {
    pliron_clif_opt::loopvec::run(
        f,
        &mut Analysis::default(),
        name,
        &FxHashSet::default(),
        tcfg(),
        true,
        true,
        true,
        &FxHashMap::default(),
        pliron_clif_opt::loopvec::WideCaps::BASE,
    )
}

/// `cnt += (x >= lo && x <= hi) ? 1 : 0` spelled
/// `select(band(cmp_lo, 1), cmp_hi, 0)` — the bool-tree AND rustc's
/// `bitselect` lowering produces; the cond is truth-tested, so the
/// rewritten 0/-1 mask must NOT re-apply `& 1`.
const SRC_SEL: &str = "function %f(i64, i64, i32, i32) -> i64 {
block0(v0: i64, v1: i64, v2: i32, v3: i32):
    v4 = iconst.i64 4
    v5 = imul v1, v4
    v6 = iadd v0, v5
    v7 = iconst.i64 0
    jump block1(v0, v7)
block1(v8: i64, v9: i64):
    v10 = icmp ult v8, v6
    brif v10, block2, block5
block2:
    v11 = load.i32 notrap v8
    v12 = icmp ule v2, v11
    v16 = iconst.i8 1
    v13 = band v12, v16
    v14 = icmp ule v11, v3
    v17 = iconst.i8 0
    v15 = select v13, v14, v17
    v18 = uextend.i64 v15
    v19 = iadd v9, v18
    v20 = iadd v8, v4
    jump block1(v20, v19)
block5:
    return v9
}";

/// `cnt += (x >= lo && x <= hi)` over `i32` elems — the bool tree is a
/// `band` of two `ule`s wrapped in `band .., 1` (0/1 normalization).
const SRC: &str = "function %f(i64, i64, i32, i32) -> i64 {
block0(v0: i64, v1: i64, v2: i32, v3: i32):
    v4 = iconst.i64 4
    v5 = imul v1, v4
    v6 = iadd v0, v5
    v7 = iconst.i64 0
    jump block1(v0, v7)
block1(v8: i64, v9: i64):
    v10 = icmp ult v8, v6
    brif v10, block2, block5
block2:
    v11 = load.i32 notrap v8
    v12 = icmp ule v2, v11
    v13 = icmp ule v11, v3
    v14 = band v12, v13
    v16 = iconst.i64 1
    v15 = band v14, v16
    v17 = uextend.i64 v15
    v18 = iadd v9, v17
    v19 = iadd v8, v4
    jump block1(v19, v18)
block5:
    return v9
}";

fn insts<'a>(f: &'a Function, op: Opcode) -> impl Iterator<Item = InstructionData> + 'a {
    f.layout
        .blocks()
        .flat_map(move |b| f.layout.block_insts(b))
        .filter(move |&i| f.dfg.insts[i].opcode() == op)
        .map(move |i| f.dfg.insts[i])
}

/// Unsigned `icmp`s on vector-typed operands — the scalar remainder
/// loop keeps its `ule`s legitimately; only lane compares count.
fn vec_icmp_ule(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| {
            matches!(
                f.dfg.insts[i],
                InstructionData::IntCompare { cond, args, .. }
                    if matches!(cond, IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual | IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual)
                        && f.dfg.value_type(args[0]).is_vector()
            )
        })
        .count()
}

#[test]
fn range_pair_rewrites_to_strict_out_of_range() {
    // Opt-in gate: must be set before pliron_cfg's one-shot env parse.
    unsafe { std::env::set_var("PLIRON_VRANGE", "1") };
    let before = parse(SRC);
    let mut on = before.clone();
    let n = run_vec(&mut on, "vr");
    assert!(n > 0, "did not vectorize\n{}", on.display());
    // The vector body's keep mask is `bor` + `bnot` over two strict
    // compares — no surviving unsigned `ule`/`uge` lane compare.
    let bor = insts(&on, Opcode::Bor).count();
    let bnot = insts(&on, Opcode::Bnot).count();
    assert!(
        bor >= 1 && bnot >= 1,
        "no fused out-of-range mask\n{}",
        on.display()
    );
    assert_eq!(
        vec_icmp_ule(&on),
        0,
        "unsigned lane compare survived\n{}",
        on.display()
    );
    // Same answer as the scalar loop.
    let cfg = Config {
        trials: 32,
        ..Config::default()
    };
    if let Err(m) = check(&before, &on, &Facts::default(), &cfg) {
        panic!("mismatch: {m}\n---- after\n{}", on.display());
    }
}

/// Same loop with the range mask inside a truth-tested `select` cond —
/// the rewritten mask keeps 0/-1 lanes (re-applying `& 1` here would
/// break `i2`'s contribution).
#[test]
fn select_cond_range_pair_keeps_01_mask() {
    unsafe { std::env::set_var("PLIRON_VRANGE", "1") };
    let before = parse(SRC_SEL);
    let mut on = before.clone();
    let n = run_vec(&mut on, "vrsel");
    assert!(n > 0, "did not vectorize\n{}", on.display());
    assert_eq!(
        vec_icmp_ule(&on),
        0,
        "unsigned lane compare survived\n{}",
        on.display()
    );
    let cfg = Config {
        trials: 32,
        ..Config::default()
    };
    if let Err(m) = check(&before, &on, &Facts::default(), &cfg) {
        panic!("mismatch: {m}\n---- after\n{}", on.display());
    }
}

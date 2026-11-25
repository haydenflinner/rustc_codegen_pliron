//! `PLIRON_VECPROBE`: an early-exit search loop's first vector iteration
//! is peeled ahead of the loop entry — a hit inside the first 64-byte
//! window resumes the scalar loop at the failing lane without paying the
//! vector loop's setup. Each case must still convert and
//! differential-check equal.

use cranelift_codegen::ir::{Block, Function, Inst, Value};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};
use target_lexicon::PointerWidth;

fn knobs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for k in ["PLIRON_SELKNOWN_COND", "PLIRON_VEC_EXACT", "PLIRON_VECPROBE"] {
            if std::env::var_os(k).is_none() {
                unsafe { std::env::set_var(k, "1") };
            }
        }
    });
}

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn run_vec(f: &mut Function, name: &str) -> usize {
    let tcfg = TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    };
    pliron_clif_opt::loopvec::run(
        f,
        &mut Analysis::default(),
        name,
        &FxHashSet::default(),
        tcfg,
        true,
        false,
        true,
        &FxHashMap::default(),
        pliron_clif_opt::loopvec::WideCaps::BASE,
    )
}

fn insts(f: &Function) -> impl Iterator<Item = Inst> + '_ {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b))
}

fn is_vector(f: &Function, v: Value) -> bool {
    f.dfg.value_type(v).is_vector()
}

/// Edges into `b` (the scalar loop header's re-entry count).
fn edges_to(f: &Function, b: Block) -> usize {
    insts(f)
        .filter(|&i| {
            f.dfg.insts[i]
                .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                .iter()
                .any(|bc| bc.block(&f.dfg.value_lists) == b)
        })
        .count()
}

fn vectorizes(src: &str, name: &str) -> Function {
    vectorizes_full(src, name, false)
}

/// `sel`: run `edgeknown::selects` first (the zip-shape's header selects
/// need it, as in the real pipeline's `selknown` step before `loopvec`).
fn vectorizes_sel(src: &str, name: &str) -> Function {
    vectorizes_full(src, name, true)
}

fn vectorizes_full(src: &str, name: &str, sel: bool) -> Function {
    knobs();
    let before = parse(src);
    let mut after = before.clone();
    if sel {
        pliron_clif_opt::edgeknown::selects(&mut after);
    }
    let n = run_vec(&mut after, name);
    assert!(n > 0, "{name} did not vectorize\n{}", after.display());
    assert!(
        insts(&after).any(|i| after.dfg.inst_results(i).iter().any(|&r| is_vector(&after, r))),
        "{name}: no vector inst\n{}",
        after.display()
    );
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{name} mismatch: {m}\n---- after\n{}", after.display());
    }
    after
}

const TAKE_WS: &str = "function %take_ws(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iadd v0, v1
    v3 = iconst.i64 0
    v4 = iconst.i64 1
    v5 = iconst.i8 32
    v6 = iconst.i8 1
    v7 = iconst.i8 9
    jump block1(v0, v3)
block1(v10: i64, v11: i64):
    v12 = icmp eq v10, v2
    brif v12, block4(v11), block2
block2:
    v13 = load.i8 notrap v10
    v14 = icmp eq v13, v5
    v15 = icmp eq v13, v7
    v16 = select v14, v6, v15
    v17 = band v16, v6
    brif v17, block3, block4(v11)
block3:
    v18 = iadd v10, v4
    v19 = iadd v11, v4
    jump block1(v18, v19)
block4(v20: i64):
    return v20
}";

/// The probe peels iteration 0's lane test: the scalar header gains a
/// third re-entry (epilogue, loop resume, probe resume), and a second
/// fused `vall_true` sits ahead of the vector loop.
#[test]
fn probe_peels_first_iteration() {
    let f = vectorizes(TAKE_WS, "take_ws");
    // block1 is the scalar loop header.
    let h = Block::from_u32(1);
    // Edges: `pre` + `cb` + `ve` + `r` + `pr` + the scalar latch.
    assert_eq!(
        edges_to(&f, h),
        6,
        "expected pre + cb + epilogue + resume + probe + latch edges\n{}",
        f.display()
    );
    let fused = insts(&f)
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::VallTrue)
        .count();
    assert!(fused >= 2, "no peeled lane test\n{}", f.display());
}

/// Without the probe the same function has one fewer header re-entry.
/// (`PLIRON_VECPROBE` is process-global: this documents the expected
/// count under the probe for contrast, not a second config.)
#[test]
fn probe_resume_uses_exact_lane() {
    let f = vectorizes(TAKE_WS, "take_ws");
    // Two `exact_lane` chains: the loop resume's and the probe resume's —
    // 2 * (unroll-1) `vhigh_bits`... each chain ends in one `vhigh_bits`,
    // so two chains = two `vhigh_bits` insts.
    let lanes = insts(&f)
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::VhighBits)
        .count();
    assert_eq!(lanes, 2, "probe resume lost its exact lane\n{}", f.display());
}

/// `take_while(|&&b| b != b'"' && b != b'\\')` under the probe.
#[test]
fn and_select_probe() {
    vectorizes(
        "function %take_str(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iadd v0, v1
    v3 = iconst.i64 0
    v4 = iconst.i64 1
    v5 = iconst.i8 34
    v6 = iconst.i8 0
    v7 = iconst.i8 92
    v8 = iconst.i8 1
    jump block1(v0, v3)
block1(v10: i64, v11: i64):
    v12 = icmp eq v10, v2
    brif v12, block4(v11), block2
block2:
    v13 = load.i8 notrap v10
    v14 = icmp ne v13, v5
    v15 = icmp ne v13, v7
    v16 = select v14, v15, v6
    v17 = band v16, v8
    brif v17, block3, block4(v11)
block3:
    v18 = iadd v10, v4
    v19 = iadd v11, v4
    jump block1(v18, v19)
block4(v20: i64):
    return v20
}",
        "take_str",
    );
}

/// A mismatch scan over two streams under the probe (the header selects
/// fold via `edgeknown` first, as in the real pipeline).
#[test]
fn zip_mismatch_probe() {
    vectorizes_sel(
        "function %mismatch(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = iconst.i64 0
    v5 = umin v1, v3
    v6 = iconst.i64 1
    jump block1(v4, v4, v4)
block1(v10: i64, v11: i64, v12: i64):
    v13 = icmp ult v10, v5
    v14 = iadd v0, v10
    v15 = iadd v2, v10
    v16 = iadd v10, v6
    v17 = select v13, v14, v4
    v18 = select v13, v15, v11
    v19 = select v13, v16, v10
    brif v13, block2, block4(v12)
block2:
    v20 = load.i8 notrap v17
    v21 = load.i8 notrap v18
    v22 = icmp eq v20, v21
    brif v22, block3, block4(v12)
block3:
    v23 = iadd v12, v6
    jump block1(v19, v18, v23)
block4(v24: i64):
    return v24
}",
        "mismatch",
    );
}

/// The scalar header keeps its non-resume entries too (`pre`'s short
/// trip and `cb`'s pred-fail brifs still reach it).
#[test]
fn short_trip_stays_scalar() {
    let f = vectorizes(TAKE_WS, "take_ws");
    let h = Block::from_u32(1);
    assert!(
        edges_to(&f, h) >= 4,
        "expected preheader + epilogue + resume + probe edges\n{}",
        f.display()
    );
}

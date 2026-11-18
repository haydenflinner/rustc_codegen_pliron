//! Early-exit search loops whose test is a short-circuit `a || b` / `a && b`
//! that ifconv flattened into a select over compare results
//! (`take_while(|&b| b == b' ' || b == b'\t')`), and the header selects on
//! the loop's own bounds test that hide `zip(..).take_while(..)`'s pointers
//! from loopvec (`PLIRON_SELKNOWN_COND=1`). Each positive case must convert
//! and differential-check equal. The resume re-enters the scalar loop at the
//! first failing lane (`PLIRON_VEC_EXACT=1`).

use cranelift_codegen::ir::{Function, InstructionData, Opcode, Value};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};
use target_lexicon::PointerWidth;

/// The knobs these tests cover. `PLIRON_*` is parsed once per process, so
/// every test sets them before its first read.
fn knobs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for k in ["PLIRON_SELKNOWN_COND", "PLIRON_VEC_EXACT"] {
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

fn insts(f: &Function) -> impl Iterator<Item = cranelift_codegen::ir::Inst> + '_ {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b))
}

fn is_vector(f: &Function, v: Value) -> bool {
    f.dfg.value_type(v).is_vector()
}

/// A vector `icmp` against an all-zero constant: the {0,1} lanes of a
/// `band(mask, 1)` turned back into a mask.
fn zero_tests(f: &Function) -> usize {
    let zero = |v: Value| match f.dfg.value_def(f.dfg.resolve_aliases(v)).inst().map(|i| f.dfg.insts[i]) {
        Some(InstructionData::UnaryConst { constant_handle, .. }) => {
            f.dfg.constants.get(constant_handle).as_slice().iter().all(|&b| b == 0)
        }
        Some(InstructionData::Unary { opcode: Opcode::Splat, arg }) => {
            pliron_clif_opt::loopidiom::iconst(f, arg) == Some(0)
        }
        _ => false,
    };
    insts(f)
        .filter(|&i| match f.dfg.insts[i] {
            InstructionData::IntCompare { args, .. } => is_vector(f, args[0]) && args.iter().any(|&a| zero(a)),
            _ => false,
        })
        .count()
}

fn vectorizes(src: &str, name: &str) -> Function {
    knobs();
    let before = parse(src);
    let mut after = before.clone();
    let n = run_vec(&mut after, name);
    assert!(n > 0, "{name} did not vectorize\n{}", after.display());
    assert!(insts(&after).any(|i| f_vec(&after, i)), "{name}: no vector inst\n{}", after.display());
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{name} mismatch: {m}\n---- after\n{}", after.display());
    }
    after
}

fn f_vec(f: &Function, i: cranelift_codegen::ir::Inst) -> bool {
    f.dfg.inst_results(i).iter().any(|&r| is_vector(f, r))
}

/// `s.iter().take_while(|&&b| b == b' ' || b == b'\t').count()` after
/// threading: the `||` is `select(b == ' ', 1, b == '\t')`, tested through
/// `& 1`. The exit tests the `bor` of the two lane masks as is.
#[test]
fn or_select_early_exit() {
    let f = vectorizes(
        "function %take_ws(i64, i64) -> i64 {
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
}",
        "take_ws",
    );
    assert_eq!(zero_tests(&f), 0, "band(mask, 1) re-tested against zero\n{}", f.display());
    let lanes = insts(&f).filter(|&i| f.dfg.insts[i].opcode() == Opcode::VhighBits).count();
    assert_eq!(lanes, 1, "resume does not find the failing lane\n{}", f.display());
}

/// `take_while(|&&b| b != b'"' && b != b'\\')`: the `&&` is
/// `select(b != '"', b != '\\', 0)`.
#[test]
fn and_select_early_exit() {
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

/// `a.iter().zip(b).take_while(|(x, y)| x == y).count()`: ifconv leaves
/// `p = select (i < n), base + i, old` above the header's `brif (i < n)`.
/// Every use of `p` is on the taken side, so it is `base + i`, and the loop
/// then vectorizes.
#[test]
fn header_select_on_bounds_test() {
    knobs();
    let src = "function %mismatch(i64, i64, i64, i64) -> i64 {
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
}";
    let before = parse(src);
    let mut after = before.clone();
    let k = pliron_clif_opt::edgeknown::selects(&mut after);
    assert!(k >= 3, "header selects not folded ({k})\n{}", after.display());
    let selects = insts(&after)
        .filter(|&i| after.dfg.insts[i].opcode() == Opcode::Select)
        .filter(|&i| {
            let r = after.dfg.first_result(i);
            insts(&after).any(|j| after.dfg.inst_values(j).any(|v| after.dfg.resolve_aliases(v) == r))
        })
        .count();
    assert_eq!(selects, 0, "a header select is still used\n{}", after.display());
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("selects mismatch: {m}\n---- after\n{}", after.display());
    }
}

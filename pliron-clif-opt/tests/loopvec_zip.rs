//! Multi-IV zip headers: a lagged param fed `base + iv*K` (recomputed
//! each iteration rather than `prev + K`) upgrades to `Step` when the
//! affine terms are recognized — including in-body const/symbol defs
//! that `param_coeffs` used to reject.

use cranelift_codegen::ir::{Function, Opcode};
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
        false,
        true,
        &FxHashMap::default(),
        pliron_clif_opt::loopvec::WideCaps::BASE,
    )
}

fn has_vector_inst(f: &Function) -> bool {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .any(|i| {
            f.dfg
                .inst_results(i)
                .iter()
                .any(|&r| f.dfg.value_type(r).is_vector())
                || matches!(f.dfg.insts[i].opcode(), Opcode::Shuffle | Opcode::Splat)
        })
}

/// `z[i] = x[i] ^ k` over a zip: the load pointer is recomputed
/// `src + i` each iteration and fed to a dead header param — the
/// "multi-IV zip" shape rustc emits for `iter_mut().zip(..)`. The
/// feed's root is an in-body `iconst` — the rematable-def case
/// `param_coeffs` used to reject.
#[test]
fn lagged_ptr_param_vectorizes() {
    let src = "function %f(i64, i64, i64) {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    v4 = iconst.i64 1
    v20 = iconst.i64 90
    jump block1(v3, v1)
block1(v5: i64, v6: i64):
    v7 = icmp ult v5, v2
    brif v7, block2, block3
block2:
    v8 = iadd v5, v4
    v10 = imul v5, v4
    v11 = iadd v1, v10
    v12 = load.i8 notrap v11
    v13 = bxor v12, v20
    v14 = iadd v0, v10
    store.i8 notrap v13, v14
    v30 = iconst.i64 4096
    v31 = iadd v30, v10
    jump block1(v8, v31)
block3:
    return
}";
    let before = parse(src);
    let mut after = before.clone();
    let n = run_vec(&mut after, "zip_lagged");
    assert!(n > 0, "lagged-ptr zip loop did not vectorize\n{}", after.display());
    assert!(has_vector_inst(&after));
    let cfg = Config { trials: 32, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("mismatch: {m}\n---- after\n{}", after.display());
    }
}

/// Same shape but the lagged param is read at the exit — the epilogue's
/// `entry + s*done` must reproduce the last latch-fed value. `len` is
/// fixed at 64 (`vf*UNROLL`) so the vector loop consumes every iteration
/// (`rem == 0 && nm ≥ 1`) — the exit reads the deferred value directly.
#[test]
fn lagged_param_read_at_exit() {
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v3 = iconst.i64 0
    v4 = iconst.i64 1
    v2 = iconst.i64 64
    jump block1(v3, v1)
block1(v5: i64, v6: i64):
    v7 = icmp ult v5, v2
    brif v7, block2, block3
block2:
    v8 = iadd v5, v4
    v10 = imul v5, v4
    v11 = iadd v1, v10
    v12 = load.i8 notrap v11
    v14 = iadd v0, v10
    store.i8 notrap v12, v14
    jump block1(v8, v11)
block3:
    return v6
}";
    let before = parse(src);
    let mut after = before.clone();
    let n = run_vec(&mut after, "zip_lagged_read");
    assert!(n > 0, "did not vectorize\n{}", after.display());
    let cfg = Config { trials: 16, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("mismatch: {m}\n---- after\n{}", after.display());
    }
}

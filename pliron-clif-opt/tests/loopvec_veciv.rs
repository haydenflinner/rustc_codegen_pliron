//! `PLIRON_VECIV` (default-on): iv-stream lane values affine in the counted iv
//! (`coeff*iv + off`, |coeff| = 1) are carried through the vector loop
//! as `vt` block params stepped once per outer iteration, instead of
//! recomputing `splat(iv+g) + iota` then a vector add/sub per group —
//! LLVM's vector-IV form. On `(n - i) & 7` accumulation this drops the
//! body from ~8 insts/2 lanes to ~3. Default on (`=0` keeps the
//! per-group splat seed.

use cranelift_codegen::ir::Function;
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

/// `acc += (n - i) & 7` over `i in 0..min(n,200)`: the lane value
/// `n - i` is affine in the up-counted iv (coeff -1). `umin` bounds
/// the trip count so the differential interpreter terminates.
const SRC: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v7 = iconst.i64 0
    v30 = iconst.i64 200
    v31 = umin v0, v30
    jump block10(v7, v7)
block10(v108: i64, v109: i64):
    v1 = iconst.i64 1
    v114 = iadd v108, v1
    v115 = icmp ult v114, v31
    v116 = isub v0, v108
    v8 = iconst.i64 7
    v118 = band v116, v8
    v119 = iadd v109, v118
    brif v115, block10(v114, v119), block23
block23:
    return v119
}";

/// `i64x2`-typed params on any block — the carried affine vectors ride
/// the vector header next to the accumulator params.
fn vec_params(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.dfg.block_params(b).iter())
        .filter(|&&p| f.dfg.value_type(p).is_vector())
        .count()
}

#[test]
fn affine_lane_values_carried_as_vector_params() {
    // Explicit-on keeps this test independent of the default flip.
    unsafe { std::env::set_var("PLIRON_VECIV", "1") };
    let before = parse(SRC);
    let mut on = before.clone();
    let n = run_vec(&mut on, "vi");
    assert!(n > 0, "did not vectorize\n{}", on.display());
    // Carried vectors add i64x2 header params beyond the accumulators
    // (the iv itself and `n - i` both qualify for this loop).
    assert!(
        vec_params(&on) > 4,
        "no carried affine vector param\n{}",
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

//! `Step` header params advance `s` per scalar iteration at *their own*
//! width, which needn't be the IV's. The epilogue/resume value is
//! `entry + iters*s` — loopvec computed `iters`/`nm` at `iv_ty` and fed
//! the i64 product straight into `iadd` with the i32 entry value:
//! `iadd.i32 v_entry, (imul.i64 nm, 1)` — arg type mismatch.
//! Real-world hit: moxcms `ToneReprCurve::build_gamma_table` (i64 byte
//! IV, i32 stepped counter) in the bevy_solari build.

use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_codegen::settings;
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
        &mut pliron_clif_opt::pipeline::Analysis::default(),
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

#[test]
fn narrow_step_param_resume_typed() {
    // i64 element-index IV over an i16 store stream; the i32 header
    // param steps 1/iter — a `Param::Step` narrower than `iv_ty`.
    // Vectorizing must emit its resume/epilogue value as
    // `entry + iters*1` at i32, not i64.
    let src = "function %f(i64, i64, i64) {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    v4 = iconst.i32 7
    v20 = iconst.i8 90
    jump block1(v3, v4)
block1(v5: i64, v6: i32):
    v7 = icmp ult v5, v2
    brif v7, block2, block3
block2:
    v10 = iadd v1, v5
    v12 = load.i8 notrap v10
    v13 = bxor v12, v20
    v14 = iadd v0, v5
    store.i8 notrap v13, v14
    v11 = iconst.i64 1
    v15 = iadd v5, v11
    v16 = iconst.i32 1
    v17 = iadd v6, v16
    jump block1(v15, v17)
block3:
    return
}";
    let mut f = parse(src);
    let n = run_vec(&mut f, "narrow_step");
    assert!(n > 0, "expected the loop to vectorize:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(&f, &flags) {
        panic!("{e}\n{}", f.display());
    }
}

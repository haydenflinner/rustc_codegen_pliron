//! veclegalize used to copy an f32x2 `extractlane` result with
//! `iadd_imm v, 0`, an integer add on a float that the verifier rejects
//! (CubeCL's `<2 x f32>` kernels hit it).

use cranelift_codegen::ir::types;
use cranelift_codegen::settings;

#[test]
fn f32x2_extractlane_stays_valid() {
    let src = "function %f(i64) -> f32 {
block0(v0: i64):
    v1 = load.f32x2 notrap aligned v0
    v2 = extractlane v1, 1
    return v2
}";
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let n = pliron_clif_opt::veclegalize::run(&mut f, types::F32X2);
    assert!(n > 0, "veclegalize did nothing:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(&f, &flags) {
        panic!("{e}\n{}", f.display());
    }
}

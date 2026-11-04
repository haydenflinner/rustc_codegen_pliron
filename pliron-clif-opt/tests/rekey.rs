//! Equality as an invariant (`PLIRON_REKEY`): after a param merge makes
//! two pure instructions identical, `clifpeep::rekey` aliases the
//! dominated one at once, transitively through its users, instead of
//! leaving it for a later whole-function `dgvn`.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::clifpeep;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("invalid IR: {e}\n{}", f.display());
    }
}

/// `sameargs` drops `v3` (both edges pass `v0`); `v4 = v3 + 4` is then
/// `v2`, and `v6 = v4 * 8` is then `v5`.
const MERGE: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 4
    v2 = iadd v0, v1
    v7 = iconst.i64 8
    v5 = imul v2, v7
    brif v0, block1(v0), block2
block2:
    jump block1(v0)
block1(v3: i64):
    v4 = iadd v3, v1
    v6 = imul v4, v7
    v8 = iadd v6, v5
    return v8
}";

#[test]
fn merged_param_users_alias_their_dominating_equals_transitively() {
    let mut f = parse(MERGE);
    let (k, pmap) = clifpeep::sameargs(&mut f);
    assert!(k > 0 && !pmap.is_empty(), "sameargs did nothing:\n{}", f.display());
    let n = clifpeep::rekey(&mut f, pmap.values().copied());
    verify(&f);
    let text = f.display().to_string();
    assert_eq!(n, 2, "iadd and imul both re-keyed:\n{text}");
    let count = |op: &str| text.lines().filter(|l| l.contains(&format!(" = {op}"))).count();
    assert_eq!(count("iadd"), 2, "v2 and v8 remain:\n{text}");
    assert_eq!(count("imul"), 1, "v6 aliased to v5:\n{text}");
    assert!(text.contains("v4 -> v2") && text.contains("v6 -> v5"), "{text}");
}

/// No merge, no work: an empty seed set touches nothing.
#[test]
fn no_seeds_is_a_no_op() {
    let mut f = parse(MERGE);
    assert_eq!(clifpeep::rekey(&mut f, std::iter::empty()), 0);
}

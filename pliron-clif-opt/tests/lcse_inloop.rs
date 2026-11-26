//! `lcse`'s in-loop ident-alias guard keeps a fold from stretching the
//! alias target's live range across a loop it never entered (`tokenize`'s
//! scan loop). Under `PLIRON_LCSLIVE=1` the guard relaxes when the target is
//! already live into the loop: `isub (iadd i, n), i → n` then folds —
//! `n`'s own `n+i` already forces it live-through — and the whole
//! downstream chain (`*8>>3` len decode, `min`) hoists. num-bigint-dig's
//! `montgomery` paid ~13 insts per outer iteration for this recompute
//! (blind-sig +1.58M executed instructions vs LLVM).

use cranelift_codegen::ir::Function;
use cranelift_reader::parse_functions;

fn parse(src: &str) -> Function {
    parse_functions(src).unwrap().remove(0)
}

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("invalid IR: {e}\n{}", f.display());
    }
}

/// `isub (iadd i, n), i` inside the loop body: `n` is already live into
/// the loop (the `iadd` and the exit test both read it), so the fold
/// stretches nothing and fires under `PLIRON_LCSLIVE=1`.
#[test]
fn inloop_fold_when_target_already_live() {
    unsafe { std::env::set_var("PLIRON_LCSLIVE", "1") };
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v5: i64):
    v12 = iconst.i64 0
    brif v12, block2, block1(v12)

block1(v16: i64):
    v34 = iadd v5, v16
    v36 = isub v34, v16
    v40 = imul v36, v5
    v60 = iadd v16, v34
    v70 = icmp ult v60, v5
    brif v70, block1(v60), block2

block2:
    return v12
}";
    let mut f = parse(src);
    assert!(pliron_clif_opt::clifpeep::lcse(&mut f) > 0);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        !text.lines().any(|l| l.contains(" = isub")),
        "isub(iadd i, n), i did not fold to n:\n{text}"
    );
    assert!(
        text.lines().any(|l| l.contains("v36 -> v5")),
        "the isub result did not alias to n:\n{text}"
    );
}

/// Same loop, but the fold target is an out-of-loop value nothing in the
/// loop reads (`imul x, 1 → x`): the guard still holds — folding would
/// force `x` live-through for the first time.
#[test]
fn inloop_guard_keeps_cold_target() {
    unsafe { std::env::set_var("PLIRON_LCSLIVE", "1") };
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v5: i64):
    v12 = iconst.i64 0
    v35 = iadd v0, v0
    v19 = iconst.i64 1
    brif v12, block2, block1(v12)

block1(v16: i64):
    v36 = imul v35, v19
    v60 = iadd v16, v19
    v70 = icmp ult v60, v5
    brif v70, block1(v60), block2

block2:
    return v35
}";
    let mut f = parse(src);
    pliron_clif_opt::clifpeep::lcse(&mut f);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        text.lines().any(|l| l.contains(" = imul")),
        "imul x, 1 folded to a loop-cold target:\n{text}"
    );
}

/// Same `imul x, 1` in-loop, but `x` is stored inside the loop — already
/// live-through — so the fold is free and fires.
#[test]
fn inloop_fold_when_target_used_in_loop() {
    unsafe { std::env::set_var("PLIRON_LCSLIVE", "1") };
    let src = "function %f(i64, i64) -> i64 {
block0(v0: i64, v5: i64):
    v12 = iconst.i64 0
    v35 = iadd v0, v0
    v19 = iconst.i64 1
    brif v12, block2, block1(v12)

block1(v16: i64):
    v36 = imul v35, v19
    store.i64 notrap v35, v0
    v60 = iadd v16, v19
    v70 = icmp ult v60, v5
    brif v70, block1(v60), block2

block2:
    return v35
}";
    let mut f = parse(src);
    pliron_clif_opt::clifpeep::lcse(&mut f);
    verify(&f);
    let text = f.display().to_string();
    assert!(
        !text.lines().any(|l| l.contains(" = imul")),
        "imul x, 1 on a live-through target did not fold:\n{text}"
    );
}

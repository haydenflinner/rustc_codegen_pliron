//! `PLIRON_LOADFWD_SRCCOPY`: a load covered by `store (load s)` re-reads
//! the source. Two regressions:
//!
//! 1. The destination lookup must include the load's own `+imm` offset —
//!    without it `load p+8` matched the entry for `p+0` and re-read the
//!    wrong field (`unicode_escape` compared field 0 to itself and lost
//!    a store).
//! 2. The emitted address is `q + qoff + delta`, not `q + base + delta`:
//!    `base` already contains `q`'s normalized root offset, so a source
//!    load through `iadd s, 9` double-counted (`core::net::parser`).

use cranelift_codegen::ir::{Function, InstructionData};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

fn run(src: &str) -> (Function, Function) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut f = before.clone();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f,
        &cfg,
        &pf,
        &ps,
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
    );
    (before, f)
}

fn checked(before: Function, f: Function) -> Function {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let s = check(&before, &f, &Facts::default(), &Config { trials: 128, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", f.display()));
    assert!(s.compared > 0, "{:?}", s.why);
    f
}

fn nloads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::Load { .. }))
        .count()
}

/// `load p+8` once forwarded to the value of `load p` — the `+8` was
/// dropped from the loc, so the `p+0` entry "covered" it. Both fields
/// must still be read.
#[test]
fn inst_offset_not_dropped() {
    unsafe { std::env::set_var("PLIRON_LOADFWD_SRCCOPY", "1") };
    let src = "function %t(i64) -> i64, i64 {
block0(v0: i64):
    v1 = load.i64 notrap v0
    v2 = load.i64 notrap v0+8
    return v1, v2
}";
    let (before, f) = run(src);
    assert_eq!(nloads(&f), 2, "field +8 must not re-read field 0:\n{}", f.display());
    checked(before, f);
}

/// Copy round-trip: `store (load s+k), d` then a narrow `uload8 d+j`
/// re-reads `s+k+j`. The source load goes through `iadd s, 16`, so its
/// normalized root offset is nonzero — emitting `load (q, base+d)`
/// read `s+k+j+16`. (`load.i64 d` would hit exact `Fwd::V` first —
/// the width mismatch is what reaches `copy_src`.)
#[test]
fn copy_through_offset_root() {
    unsafe { std::env::set_var("PLIRON_LOADFWD_SRCCOPY", "1") };
    // The destination is a stack slot: a `V`-rooted param store would
    // kill the source's avail entry as a may-alias clobber — which is
    // exactly what kept this test honest on the first draft.
    let src = "function %t(i64) -> i64 {
    ss0 = explicit_slot 8
block0(v0: i64):
    v9 = iconst.i64 16
    v10 = iadd v0, v9  ; v9 = 16
    v3 = load.i64 notrap v10
    v1 = stack_addr.i64 ss0
    store.i64 notrap v3, v1
    v4 = load.i8 notrap v1+3
    v5 = uextend.i64 v4
    return v5
}";
    let (before, f) = run(src);
    let f = checked(before, f);
    let text = f.display().to_string();
    // Byte 3 of the copy is source byte 3 — `load.i8 v10+3`. The
    // `base+d` form emitted `v10+19`: 16 bytes past the field.
    assert!(text.contains("v10+3"), "expected source re-read at v10+3:\n{text}");
}

/// Same shape with the byte loads the brotli `sub_match!` buffers emit:
/// `uload8` reads of a copied word re-read the source.
#[test]
fn narrow_reads_of_copy() {
    unsafe { std::env::set_var("PLIRON_LOADFWD_SRCCOPY", "1") };
    let src = "function %t(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v3 = load.i64 notrap v0+8
    store.i64 notrap v3, v1
    v4 = uload8.i64 notrap v1
    v5 = uload8.i64 notrap v1+7
    v7 = iconst.i64 56
    v6 = ishl v5, v7  ; v7 = 56
    v8 = bor v4, v6
    return v8
}";
    let (before, f) = run(src);
    checked(before, f);
}

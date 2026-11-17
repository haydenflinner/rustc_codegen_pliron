//! licm `PLIRON_LICM_GTE=1`: a `notrap` load that runs every time the
//! loop is entered (on the header's straight line, before any branch,
//! call or trapping instruction) hoists to the entry edge without a
//! dereferenceability fact for its address: it would have executed on the
//! first iteration anyway. hmget under single-caller inlining kept the
//! map's `items` load in its lookup loop because `&HashMap`, read out of
//! the closure environment, has no `dereferenceable` fact.
//!
//! None of the pointers here carries a fact, so without the rule nothing
//! hoists (the near-misses check the rule does not overreach).

use cranelift_codegen::ir::{Block, Function, Opcode};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

fn run(src: &str) -> Function {
    // Every test in this binary sets the same value before the config is
    // first read.
    unsafe { std::env::set_var("PLIRON_LICM_GTE", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::licm::run(
        &mut f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        false,
        false,
    );
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(&f, &flags) {
        panic!("{e}\n{}", f.display());
    }
    f
}

fn loads_in(f: &Function, b: u32) -> usize {
    f.layout
        .block_insts(Block::from_u32(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

fn loads_total(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

/// The guarded (rotated) loop's header reads `v1+8` before anything else:
/// it leaves the loop. It must land on the entry edge, after the guard:
/// not in `block0`, whose `brif` skips the loop when `v0 == 0`.
#[test]
fn header_load_hoists_onto_the_entry_edge() {
    let f = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    brif v0, block1(v2, v2), block2(v2)
block1(v3: i64, v4: i64):
    v5 = load.i64 notrap v1+8
    v6 = iadd v4, v5
    v7 = iconst.i64 1
    v8 = iadd v3, v7
    v9 = icmp ult v8, v0
    brif v9, block1(v8, v6), block2(v6)
block2(v10: i64):
    return v10
}");
    assert_eq!(loads_in(&f, 1), 0, "the header load stayed:\n{}", f.display());
    assert_eq!(loads_in(&f, 0), 0, "hoisted above the zero-trip guard:\n{}", f.display());
    assert_eq!(loads_total(&f), 1, "{}", f.display());
}

/// A straight `jump` chain from the header still runs on every entry.
#[test]
fn load_after_a_jump_chain_hoists() {
    let f = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v7 = iconst.i64 1
    v8 = iadd v3, v7
    jump block3
block3:
    v5 = load.i64 notrap v1+8
    v6 = iadd v4, v5
    v9 = icmp ult v8, v0
    brif v9, block1(v8, v6), block2(v6)
block2(v10: i64):
    return v10
}");
    assert_eq!(loads_in(&f, 3), 0, "{}", f.display());
}

/// Near-miss: the load sits behind the header's branch, so it runs only
/// on iterations that take that arm (an `Option` payload pointer is valid
/// only on its arm). It must stay.
#[test]
fn load_behind_a_branch_stays() {
    let f = run("function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v20: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    brif v20, block3, block4(v4)
block3:
    v5 = load.i64 notrap v1+8
    v6 = iadd v4, v5
    jump block4(v6)
block4(v11: i64):
    v7 = iconst.i64 1
    v8 = iadd v3, v7
    v9 = icmp ult v8, v0
    brif v9, block1(v8, v11), block2(v11)
block2(v10: i64):
    return v10
}");
    assert_eq!(loads_in(&f, 3), 1, "the guarded load hoisted:\n{}", f.display());
}

/// Near-miss: a call ahead of the load in the header may not return
/// (a panic) or may be what makes the pointer valid; the load stays.
#[test]
fn load_after_a_call_stays() {
    let f = run("function %f(i64, i64) -> i64 {
    sig0 = ()
    fn0 = %g sig0
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    call fn0()
    v5 = load.i64 notrap v1+8
    v6 = iadd v4, v5
    v7 = iconst.i64 1
    v8 = iadd v3, v7
    v9 = icmp ult v8, v0
    brif v9, block1(v8, v6), block2(v6)
block2(v10: i64):
    return v10
}");
    assert_eq!(loads_in(&f, 1), 1, "the load after the call hoisted:\n{}", f.display());
}

/// Near-miss: a trapping division ahead of the load stops the first
/// iteration before it when `v0 == 0`; the load stays.
#[test]
fn load_after_a_trapping_inst_stays() {
    let f = run("function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v12 = udiv v3, v0
    v5 = load.i64 notrap v1+8
    v6 = iadd v4, v5
    v13 = iadd v6, v12
    v7 = iconst.i64 1
    v8 = iadd v3, v7
    v9 = icmp ult v8, v0
    brif v9, block1(v8, v13), block2(v13)
block2(v10: i64):
    return v10
}");
    assert_eq!(loads_in(&f, 1), 1, "the load after the udiv hoisted:\n{}", f.display());
}

/// Near-miss: the loop stores through an unrelated pointer; the value
/// could change, so the clobber scan keeps the load in place whatever
/// the execution argument says.
#[test]
fn load_with_a_clobbering_store_stays() {
    let f = run("function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v20: i64):
    v2 = iconst.i64 0
    jump block1(v2, v2)
block1(v3: i64, v4: i64):
    v5 = load.i64 notrap v1+8
    v6 = iadd v4, v5
    store notrap v6, v20
    v7 = iconst.i64 1
    v8 = iadd v3, v7
    v9 = icmp ult v8, v0
    brif v9, block1(v8, v6), block2(v6)
block2(v10: i64):
    return v10
}");
    assert_eq!(loads_in(&f, 1), 1, "{}", f.display());
}

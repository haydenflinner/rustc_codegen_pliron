//! rustc's load ranges and no-wrap flags (`PLIRON_VFACTS=1`, kept by
//! lowering as `pliron_clif_opt::vfacts::VFacts`): `vrp` decides a compare
//! only the fact proves, and the differential checker stops a trial that
//! breaks the fact (the original's behavior there is undefined) instead of
//! reporting the rewrite.

use cranelift_codegen::ir::{Function, Opcode, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use pliron_clif_opt::vfacts::{NSW, NUW, Nw, VFacts};

/// `x < 2` on a loaded byte: an enum tag with two variants.
const RANGE: &str = "function u0:0(i64) -> i8 system_v {
block0(v0: i64):
    v1 = load.i8 notrap aligned v0
    v2 = iconst.i8 2
    v3 = icmp ult v1, v2
    brif v3, block1, block2

block1:
    v4 = iconst.i8 1
    return v4

block2:
    v5 = iconst.i8 7
    return v5
}";

/// `x + 1 == 0`: never, without unsigned wrap.
const NUW_SRC: &str = "function u0:0(i32) -> i8 system_v {
block0(v0: i32):
    v1 = iconst.i32 1
    v2 = iadd v0, v1
    v3 = iconst.i32 0
    v4 = icmp eq v2, v3
    brif v4, block1, block2

block1:
    v5 = iconst.i8 1
    return v5

block2:
    v6 = iconst.i8 7
    return v6
}";

/// `x + 1 > i32::MIN`: always, without signed wrap.
const NSW_SRC: &str = "function u0:0(i32) -> i8 system_v {
block0(v0: i32):
    v1 = iconst.i32 1
    v2 = iadd v0, v1
    v3 = iconst.i32 0x8000_0000
    v4 = icmp sgt v2, v3
    brif v4, block1, block2

block1:
    v5 = iconst.i8 1
    return v5

block2:
    v6 = iconst.i8 7
    return v6
}";

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn vrp(src: &str, facts: &VFacts) -> (Function, usize) {
    let mut f = parse(src);
    let n = pliron_clif_opt::vrp::run_with(&mut f, &mut Analysis::default(), facts);
    (f, n)
}

/// Whether a `brif` was folded away or tests a constant.
fn decided(f: &Function) -> bool {
    !f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).any(|i| {
        f.dfg.insts[i].opcode() == Opcode::Brif && {
            let c = f.dfg.resolve_aliases(f.dfg.inst_args(i)[0]);
            !f.dfg.value_def(c).inst().is_some_and(|d| f.dfg.insts[d].opcode() == Opcode::Iconst)
        }
    })
}

fn v(n: u32) -> Value {
    Value::from_u32(n)
}

/// `v2 = iadd v0, v1` with `bits`.
fn add_nw(bits: u8) -> Nw {
    Nw { bits, op: Opcode::Iadd, args: [v(0), v(1)] }
}

#[test]
fn load_range_decides_compare() {
    let (f, _) = vrp(RANGE, &VFacts::default());
    assert!(!decided(&f), "decided without the fact:\n{}", f.display());
    let mut vf = VFacts::default();
    vf.range.insert(v(1), (8, 0, 1));
    let (f, n) = vrp(RANGE, &vf);
    assert!(n > 0 && decided(&f), "not decided with range 0..=1:\n{}", f.display());
}

#[test]
fn wrapping_load_range_is_signed() {
    // -1..=1 wraps through zero: signed [-1, 1], so `x u< 2` is not
    // decided (0xff is in range), but nothing wrong is concluded either.
    let mut vf = VFacts::default();
    vf.range.insert(v(1), (8, 0xff, 1));
    let (f, _) = vrp(RANGE, &vf);
    assert!(!decided(&f), "wrongly decided with range -1..=1:\n{}", f.display());
}

#[test]
fn nuw_decides_compare() {
    let (f, _) = vrp(NUW_SRC, &VFacts::default());
    assert!(!decided(&f), "decided without nuw:\n{}", f.display());
    let mut vf = VFacts::default();
    vf.nw.insert(v(2), add_nw(NSW));
    let (f, _) = vrp(NUW_SRC, &vf);
    assert!(!decided(&f), "nsw alone must not decide `x + 1 == 0`:\n{}", f.display());
    vf.nw.insert(v(2), add_nw(NUW));
    let (f, n) = vrp(NUW_SRC, &vf);
    assert!(n > 0 && decided(&f), "not decided with nuw:\n{}", f.display());
}

#[test]
fn nsw_decides_compare() {
    let (f, _) = vrp(NSW_SRC, &VFacts::default());
    assert!(!decided(&f), "decided without nsw:\n{}", f.display());
    let mut vf = VFacts::default();
    vf.nw.insert(v(2), add_nw(NUW));
    let (f, _) = vrp(NSW_SRC, &vf);
    assert!(!decided(&f), "nuw alone must not decide `x + 1 > MIN`:\n{}", f.display());
    vf.nw.insert(v(2), add_nw(NSW));
    let (f, n) = vrp(NSW_SRC, &vf);
    assert!(n > 0 && decided(&f), "not decided with nsw:\n{}", f.display());
}

/// The checker runs the original under the facts: a trial whose load is
/// outside the range stops, so the fact-based fold is not a mismatch; with
/// no facts it is one (random memory holds bytes above 1).
#[test]
fn diffcheck_respects_load_range() {
    let before = parse(RANGE);
    let mut vf = VFacts::default();
    vf.range.insert(v(1), (8, 0, 1));
    let (after, _) = vrp(RANGE, &vf);
    let cfg = Config { trials: 64, ..Config::default() };
    let facts = Facts { vfacts: Some(&vf), ..Facts::default() };
    match check(&before, &after, &facts, &cfg) {
        Err(m) => panic!("mismatch under the range fact: {m}"),
        // A random byte is in 0..=1 one time in 128: the trials seed an
        // in-range byte where the load broke the range and run again.
        Ok(sum) => assert!(sum.compared >= 32, "trials not repaired: {sum:?}"),
    }
    assert!(
        check(&before, &after, &Facts::default(), &cfg).is_err(),
        "the fold should differ from the original without the fact"
    );
}

#[test]
fn diffcheck_respects_nuw() {
    let before = parse(NUW_SRC);
    let mut vf = VFacts::default();
    vf.nw.insert(v(2), add_nw(NUW));
    let (after, _) = vrp(NUW_SRC, &vf);
    let cfg = Config { trials: 256, ..Config::default() };
    let facts = Facts { vfacts: Some(&vf), ..Facts::default() };
    if let Err(m) = check(&before, &after, &facts, &cfg) {
        panic!("mismatch under nuw: {m}");
    }
}

/// A pass may rebuild a value's definition in place (`isub x, 1` as
/// `iadd x, -1`, same value): `nuw` given for the `isub` says nothing about
/// the add (which would wrap for every `x != 0`). pulldown-cmark's
/// `Parser::handle_inline` lost a bounds test this way: `x - 1 u< len`
/// folded to false.
#[test]
fn nw_fact_needs_its_own_definition() {
    let src = "function u0:0(i64, i64) -> i8 system_v {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 -1
    v3 = iadd v0, v2
    v4 = icmp ult v3, v1
    brif v4, block1, block2

block1:
    v5 = iconst.i8 1
    return v5

block2:
    v6 = iconst.i8 7
    return v6
}";
    // Recorded for `v3 = isub v0, v9` (v9 = 1) as lowered.
    let mut vf = VFacts::default();
    vf.nw.insert(v(3), Nw { bits: NUW, op: Opcode::Isub, args: [v(0), v(9)] });
    let (f, _) = vrp(src, &vf);
    assert!(!decided(&f), "a fact of the old `isub` decided the add's compare:\n{}", f.display());
    // The same fact recorded for this very add does decide it (the add
    // of -1 cannot be `nuw` unless v0 == 0).
    vf.nw.insert(v(3), Nw { bits: NUW, op: Opcode::Iadd, args: [v(0), v(2)] });
    let (f, _) = vrp(src, &vf);
    assert!(decided(&f), "the add's own fact should decide:\n{}", f.display());
}

/// Step `vrpf` (`run_facts_only`) runs only where a fact decides
/// something, and there makes the plain folds too.
#[test]
fn facts_only_runs_where_a_fact_decides() {
    let src = |k: i64| {
        format!(
            "function u0:0(i32) -> i8 system_v {{
block0(v0: i32):
    v1 = iconst.i32 1
    v2 = iadd v0, v1
    v3 = iconst.i32 {k}
    v4 = icmp eq v2, v3
    v7 = iconst.i32 5
    v8 = icmp ult v1, v7
    v9 = band v4, v8
    brif v9, block1, block2

block1:
    v5 = iconst.i8 1
    return v5

block2:
    v6 = iconst.i8 7
    return v6
}}"
        )
    };
    let mut vf = VFacts::default();
    vf.nw.insert(v(2), add_nw(NUW));
    // `x + 1 == 0` under nuw: decided by the fact, and `1 < 5` with it.
    let mut f = parse(&src(0));
    let n = pliron_clif_opt::vrp::run_facts_only(&mut f, &mut Analysis::default(), &vf);
    let text = f.display().to_string();
    assert!(n >= 2, "both folds expected:\n{text}");
    assert!(!text.contains("icmp eq v2, v3") && !text.contains("icmp ult v1, v7"), "{text}");
    // `x + 1 == 9`: nuw decides nothing, so `1 < 5` stays for later.
    let mut f = parse(&src(9));
    let n = pliron_clif_opt::vrp::run_facts_only(&mut f, &mut Analysis::default(), &vf);
    assert_eq!(n, 0, "ran without a fact-driven fold:\n{}", f.display());
}

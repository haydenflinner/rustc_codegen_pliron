//! `diffcheck::check_cross`: the differential checker on two functions
//! from two builds of one program (`clif-check xcorpus`, the IR passes
//! checked by ablation).

use cranelift_codegen::ir::{Function, Value};
use pliron_clif_opt::diffcheck::{Config, Cross, Facts, Syms, check, check_cross};
use rustc_hash::{FxHashMap, FxHashSet};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn syms(pairs: &[(&str, &str)]) -> Syms {
    Syms {
        names: pairs.iter().map(|(d, s)| (d.to_string(), s.to_string())).collect(),
        ..Syms::default()
    }
}

fn cross_check(
    a: &Function,
    b: &Function,
    sa: &Syms,
    sb: &Syms,
    facts: &Facts,
) -> Result<pliron_clif_opt::diffcheck::Summary, pliron_clif_opt::diffcheck::Mismatch> {
    let (nw, ext) = (FxHashSet::default(), FxHashMap::default());
    let c = Cross { before: sa, after: sb, self_symbol: None, nowrite: &nw, extent: &ext };
    check_cross(a, b, facts, Some(&c), &Config { trials: 32, ..Config::default() })
}

const CALL: &str = "function %f(i64) -> i64 {
    sig0 = (i64) -> i64
    fn0 = u0:ID sig0
block0(v0: i64):
    v1 = call fn0(v0)
    return v1
}";

/// A callee is `u0:<FuncId>` in the text, and the ids are a build's own:
/// the same call in two builds is told apart by `check` and is one call
/// to `check_cross`, which keys the stub by the symbol.
#[test]
fn callee_keyed_by_symbol() {
    let a = parse(&CALL.replace("ID", "3"));
    let b = parse(&CALL.replace("ID", "7"));
    assert!(check(&a, &b, &Facts::default(), &Config::default()).is_err());
    let s = cross_check(&a, &b, &syms(&[("u0:3", "foo")]), &syms(&[("u0:7", "foo")]), &Facts::default())
        .unwrap_or_else(|m| panic!("{m}"));
    assert!(s.compared > 0);
    // A different symbol is a different callee.
    assert!(cross_check(&a, &b, &syms(&[("u0:3", "foo")]), &syms(&[("u0:7", "bar")]), &Facts::default()).is_err());
}

const PASS_SLOT: &str = "function %f(i64) -> i64 {
    ss0 = explicit_slot 8
    sig0 = (i64) -> i64
    fn0 = u0:3 sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = iconst.i64 K
    v3 = iadd v0, v2
    store v3, v1
    v4 = call fn0(v1)
    return v4
}";

/// What a callee can read through a pointer to the caller's frame is part
/// of the call: an IR pass that changes those bytes (memcpyopt rebasing a
/// temporary onto a destination a loop has changed) changes what the
/// callee sees. `check` hashes a frame pointer without its offset and
/// without the bytes behind it (CLIF passes move slots), so it sees no
/// difference.
#[test]
fn callee_sees_the_bytes_behind_a_frame_pointer() {
    let a = parse(&PASS_SLOT.replace(" K", " 0"));
    let b = parse(&PASS_SLOT.replace(" K", " 1"));
    let s = syms(&[("u0:3", "foo")]);
    assert!(check(&a, &b, &Facts::default(), &Config::default()).is_ok());
    let m = cross_check(&a, &b, &s, &s, &Facts::default());
    assert!(m.is_err(), "the callee reads v0 in one and v0 + 1 in the other");
    assert!(cross_check(&a, &a, &s, &s, &Facts::default()).is_ok());
}

const TWO_CALLS_THEN_COPY: &str = "function %f(i64) -> i64 {
    ss0 = explicit_slot 8
    sig0 = (i64) -> i64
    sig1 = (i64, i64, i64) -> i64
    fn0 = u0:3 sig0
    fn1 = %Memcpy sig1
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = iconst.i64 0
    store v2, v1
    v3 = call fn0(v1)
    v4 = iconst.i64 K
    v5 = iadd v0, v4
    store v5, v1
    v6 = call fn0(v1)
    v7 = iconst.i64 0x100_0000_0000
    v8 = call fn1(v1, v1, v7)
    return v8
}";

/// The original stops where the sandbox cannot follow (a copy of 1 TiB):
/// no trial returns, and a check of returned runs only compares nothing.
/// The calls made before that are still the program's, and the second one
/// differs.
#[test]
fn calls_before_a_sandbox_stop_are_compared() {
    let a = parse(&TWO_CALLS_THEN_COPY.replace(" K", " 0"));
    let b = parse(&TWO_CALLS_THEN_COPY.replace(" K", " 1"));
    let s = syms(&[("u0:3", "foo")]);
    let m = cross_check(&a, &b, &s, &s, &Facts::default()).expect_err("second call differs");
    assert!(m.to_string().contains("call 1"), "{m}");
    let same = cross_check(&a, &a, &s, &s, &Facts::default()).unwrap_or_else(|m| panic!("{m}"));
    assert_eq!(same.compared, 0);
}

const STORE_UNDEF: &str = "function %f(i64) {
block0(v0: i64):
    v1 = iconst.i64 0
    STORE
    return
}";

/// A value the lowering wrote as 0 for `undef` (`; diffcheck-undef:`)
/// determines nothing: one build stores it, the other (a pass that stopped
/// copying the undefined bytes) does not.
#[test]
fn undef_zero_pins_nothing() {
    let a = parse(&STORE_UNDEF.replace("STORE", "store v1, v0"));
    let b = parse(&STORE_UNDEF.replace("STORE", ""));
    let s = Syms::default();
    assert!(cross_check(&a, &b, &s, &s, &Facts::default()).is_err());
    let undef: FxHashSet<Value> = [Value::from_u32(1)].into_iter().collect();
    let facts = Facts { undef: Some(&undef), ..Facts::default() };
    cross_check(&a, &b, &s, &s, &facts).unwrap_or_else(|m| panic!("{m}"));
}

const PADDING_TO_CALLEE: &str = "function %f(i64) -> i64 {
    ss0 = explicit_slot 8
    sig0 = (i64) -> i64
    fn0 = u0:3 sig0
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    PRE
    v2 = iconst.i32 0
    store v2, v1
    v3 = load.i64 v1
    v4 = call fn0(v3)
    v6 = iconst.i64 1
    v5 = band v4, v6
    brif v5, block1, block2
block1:
    trap user2
block2:
    return v4
}";

/// An `i64` copy of an `i32` field and its padding goes to a callee: the
/// padding was never written, so the stub's answer, and with it whether
/// the function takes its panic path, is not pinned down. A rewritten
/// function with other bytes under the padding (slotmerge put another
/// slot there: dsa's `fuse_filaments_with` closure) that then traps is not
/// a mismatch. One that traps before making the call still is.
#[test]
fn trap_after_an_undetermined_call_argument_pins_nothing() {
    let a = parse(&PADDING_TO_CALLEE.replace("PRE", ""));
    let b = parse(&PADDING_TO_CALLEE.replace("PRE", "v9 = iconst.i64 0x1234_0000_0000\n    store v9, v1"));
    let cfg = Config { trials: 64, ..Config::default() };
    check(&a, &b, &Facts::default(), &cfg).unwrap_or_else(|m| panic!("{m}"));
    let s = syms(&[("u0:3", "foo")]);
    cross_check(&a, &b, &s, &s, &Facts::default()).unwrap_or_else(|m| panic!("{m}"));
    // Trapping before the call is a difference the padding cannot explain.
    let c = parse(&PADDING_TO_CALLEE.replace("PRE", "trap user2\nblock9:"));
    assert!(check(&a, &c, &Facts::default(), &cfg).is_err());
}

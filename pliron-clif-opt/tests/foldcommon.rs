//! foldcommon: `brif A, S, F` / `S: brif B, T, F` (same args to the
//! shared edge) folds to `brif (band A B), T, F` in `b`; the mirror is
//! `bor`. Restricted to `icmp`/`uextend(icmp)` conditions with at least
//! one 32/64-bit compare — the shapes the aarch64 `ccmp` fusion
//! consumes — so the fold is always `cmp; ccmp; b` or better at machine
//! level. Differentially checked.

use pliron_clif_opt::diffcheck::{Config, Facts, check};

const AND: &str = "function %f(i64, i64, i64, i64) -> i32 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp ne v0, v1
    brif v4, block1, block3
block1:
    v5 = icmp slt v2, v3
    brif v5, block2, block3
block2:
    v10 = iconst.i32 1
    return v10
block3:
    v11 = iconst.i32 0
    return v11
}";

const OR: &str = "function %f(i64, i64, i64, i64) -> i32 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp ne v0, v1
    brif v4, block2, block1
block1:
    v5 = icmp slt v2, v3
    brif v5, block2, block3
block2:
    v10 = iconst.i32 1
    return v10
block3:
    v11 = iconst.i32 0
    return v11
}";

// Both compares narrower than 32 bits: no `ccmp` reg/reg secondary is
// available either order, so folding would materialize
// `cset; op; cbnz` — decline.
const NARROW: &str = "function %f(i8, i8, i8, i8) -> i32 {
block0(v0: i8, v1: i8, v2: i8, v3: i8):
    v4 = icmp ne v0, v1
    brif v4, block1, block3
block1:
    v5 = icmp slt v2, v3
    brif v5, block2, block3
block2:
    v10 = iconst.i32 1
    return v10
block3:
    v11 = iconst.i32 0
    return v11
}";

// A non-`icmp` condition can't feed `ccmp` — decline.
const NONICMP: &str = "function %f(i64, i64, i64, i64) -> i32 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp ne v0, v1
    brif v4, block1, block3
block1:
    v5 = icmp slt v2, v3
    v6 = iconst.i8 7
    v7 = band.i8 v5, v6
    brif v7, block2, block3
block2:
    v10 = iconst.i32 1
    return v10
block3:
    v11 = iconst.i32 0
    return v11
}";

// `S`'s edge to the shared block carries different args than `b`'s —
// decline, the fold can't represent both.
const DIFFARGS: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp ne v0, v1
    brif v4, block1, block3(v2)
block1:
    v5 = icmp slt v2, v3
    brif v5, block2, block3(v3)
block2:
    return v0
block3(v6: i64):
    return v6
}";

// Matching args through `S`'s params — folds, with the `band` operands
// substituted through the param map.
const PARAMS: &str = "function %f(i64, i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v4 = icmp ne v0, v1
    brif v4, block1(v3), block3(v3)
block1(v20: i64):
    v5 = icmp slt v2, v20
    brif v5, block2, block3(v20)
block2:
    return v0
block3(v6: i64):
    return v6
}";

fn run(src: &str) -> (String, String) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let n = pliron_clif_opt::foldcommon::run(&mut after);
    let text = after.display().to_string();
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{text}");
    }
    (format!("{n}"), text)
}

#[test]
fn and_chain_folds_to_band() {
    let (n, text) = run(AND);
    assert_eq!(n, "1", "{text}");
    assert!(text.contains("band"), "{text}");
    assert!(text.contains("block1") == false, "{text}");
}

#[test]
fn or_chain_folds_to_bor() {
    let (n, text) = run(OR);
    assert_eq!(n, "1", "{text}");
    assert!(text.contains("bor"), "{text}");
}

#[test]
fn narrow_compares_decline() {
    let (n, text) = run(NARROW);
    assert_eq!(n, "0", "{text}");
}

#[test]
fn non_icmp_condition_declines() {
    let (n, text) = run(NONICMP);
    assert_eq!(n, "0", "{text}");
}

#[test]
fn differing_common_args_decline() {
    let (n, text) = run(DIFFARGS);
    assert_eq!(n, "0", "{text}");
}

#[test]
fn matching_param_args_fold() {
    let (n, text) = run(PARAMS);
    assert_eq!(n, "1", "{text}");
    assert!(text.contains("band"), "{text}");
}

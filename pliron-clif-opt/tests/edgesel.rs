//! edgesel: a `select c`/`brif c` inside a block whose every incoming
//! edge pins `c` folds to the known arm. `str::chars`'s
//! `next().unwrap_or(0)` clamps reach CLIF as `select(at_end, 0, cur)`
//! in the `!at_end` target — isel emits a dup `cmp` + `cmov` pair per
//! char on tokenize's ASCII scan unless the fold fires.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn dcheck(before: &Function, after: &Function) {
    match check(before, after, &Facts::default(), &Config::default()) {
        Ok(s) => assert!(s.compared > 0, "{s:?}"),
        Err(m) => panic!("{m}
---- after
{}", after.display()),
    }
}

#[test]
fn else_edge_folds_selects() {
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block2, block1

        block1:
            v3 = iconst.i64 0
            v4 = select.i64 v2, v3, v0
            v5 = iadd.i64 v4, v0
            v6 = select.i64 v2, v0, v5
            brif v2, block2, block3(v6)

        block2:
            return v0

        block3(v7: i64):
            return v7
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::run(&mut after);
    // Both selects fold to the known arm; the redundant `brif v2`
    // becomes a `jump`.
    assert_eq!(n, 3);
    let s = after.to_string();
    assert!(!s.contains("select"), "{s}");
    assert!(s.contains("v4 -> v0") && s.contains("v6 -> v5"), "{s}");
    assert!(s.contains("jump block3(v6)"), "{s}");
    dcheck(&before, &after);
}

#[test]
fn then_edge_folds_selects() {
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block1, block2

        block1:
            v3 = iconst.i64 9
            v4 = select.i64 v2, v3, v0
            return v4

        block2:
            return v0
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::run(&mut after);
    assert_eq!(n, 1);
    let s = after.to_string();
    assert!(!s.contains("select"), "{s}");
    assert!(s.contains("v4 -> v3"), "{s}");
    dcheck(&before, &after);
}

#[test]
fn mixed_edges_keep_select() {
    // One `jump` pred establishes nothing — the select must survive.
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block3, block1

        block1:
            jump block2

        block2:
            v3 = iconst.i64 9
            v4 = select.i64 v2, v3, v0
            return v4

        block3:
            jump block2
        }",
    );
    let mut after = before.clone();
    let n = pliron_clif_opt::edgesel::run(&mut after);
    assert_eq!(n, 0);
    dcheck(&before, &after);
}

#[test]
fn both_arms_same_target_keeps_select() {
    // `brif c, b, b` reaches b on either polarity — no fold.
    let before = parse(
        "function %f(i64, i64) -> i64 {
        block0(v0: i64, v1: i64):
            v2 = icmp eq v0, v1
            brif v2, block1, block1

        block1:
            v3 = iconst.i64 9
            v4 = select.i64 v2, v3, v0
            return v4
        }",
    );
    let mut after = before.clone();
    let _ = pliron_clif_opt::edgesel::run(&mut after);
    let s = after.to_string();
    assert!(s.contains("select"), "{s}");
    dcheck(&before, &after);
}

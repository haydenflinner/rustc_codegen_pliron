//! `bool_mask`'s `src` must compute the returned mask. Two peels handed on
//! the inner compare after changing the mask — `c ^ 1` (inverted) and
//! `icmp eq c, 0` (re-based) — and `selord_icmp`'s "unmodified predicate"
//! shortcut then aliased `icmp eq 1, (c ^ 1)` to `c` itself: the branch
//! on `PartialOrd`'s `ne` outcome in rustc's UI
//! `derives/eq-ord/deriving-cmp-generic-struct-enum.rs` went the wrong
//! way at -O (`assert_eq!(es1 < es2, lt)` failed with `true != true`).

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

#[test]
fn inverted_or_rebased_bools_do_not_alias_their_inner_compare() {
    for (src, what) in [
        (
            "function %f(i64, i64) -> i8 {
            block0(v0: i64, v1: i64):
                v2 = icmp eq v0, v1
                v3 = iconst.i8 1
                v4 = bxor v2, v3
                v5 = icmp eq v3, v4
                return v5
            }",
            "icmp eq 1, (c ^ 1)",
        ),
        (
            "function %f(i64, i64) -> i8 {
            block0(v0: i64, v1: i64):
                v2 = icmp eq v0, v1
                v3 = iconst.i8 0
                v4 = icmp eq v2, v3
                v5 = iconst.i8 1
                v6 = icmp eq v4, v5
                return v6
            }",
            "icmp eq (icmp eq c, 0), 1",
        ),
    ] {
        let before = parse(src);
        let mut after = before.clone();
        pliron_clif_opt::clifpeep::iconst128(&mut after, true);
        check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() })
            .unwrap_or_else(|e| panic!("{what}: {e}\n{}", after.display()));
    }
}

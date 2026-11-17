//! `undefphi::run` (`PLIRON_UNDEFPHI=1`): a block parameter whose incoming
//! values are one value `v` or the lowering's undefined zeros becomes `v`
//! when `v` dominates the block. Shapes: the `unwrap_unchecked` join (the
//! error edge passes an undefined payload that is never read), the same
//! through a loop-carried copy of the payload, a join of two real values
//! (stays), and a value defined in one arm only (does not dominate the
//! join: stays). The checker runs every rewritten function against its
//! original; the undefined payload is only read on paths that ignore it.

use cranelift_codegen::ir::{Function, Value};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::undefphi;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn v(n: u32) -> Value {
    Value::from_u32(n)
}

fn params(f: &Function) -> usize {
    f.layout.blocks().map(|b| f.dfg.block_params(b).len()).sum()
}

fn run(src: &str, undef: &[u32]) -> (Function, usize) {
    let before = parse(src);
    let mut after = before.clone();
    let u: Vec<Value> = undef.iter().map(|&n| v(n)).collect();
    let n = undefphi::run(&mut after, &u);
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("invalid after undefphi: {e}\n{after}"));
    check(&before, &after, &Facts::default(), &Config { trials: 64, ..Config::default() })
        .unwrap_or_else(|m| panic!("undefphi changed behaviour: {m:?}\n{after}"));
    (after, n)
}

/// `match i64::try_from(x) { Ok(v) => v, Err(_) => undef }`, the payload
/// read only on the `Ok` side.
#[test]
fn unwrap_join_takes_the_value() {
    let src = "function %f(i64) -> i64 {
    block0(v0: i64):
        v1 = iconst.i64 0
        v2 = icmp sge v0, v1
        brif v2, block1, block2
    block1:
        v3 = iconst.i8 0
        jump block3(v3, v0)
    block2:
        v4 = iconst.i8 1
        v5 = iconst.i64 0
        jump block3(v4, v5)
    block3(v6: i8, v7: i64):
        brif v6, block5, block4
    block4:
        return v7
    block5:
        v8 = iconst.i64 7
        return v8
    }";
    let (after, n) = run(src, &[5]);
    assert_eq!(n, 1, "{after}");
    assert_eq!(params(&after), 2, "only the entry param and the tag stay");
}

/// The payload's slot carried around a loop: the header's parameter gets
/// the undefined zero on entry and the join's value on the back edge.
#[test]
fn loop_carried_payload() {
    let src = "function %f(i64, i64) -> i64 {
    block0(v0: i64, v1: i64):
        v2 = iconst.i64 0
        v3 = iconst.i64 0
        jump block1(v3, v2, v2)
    block1(v4: i64, v5: i64, v6: i64):
        v7 = icmp sge v0, v4
        brif v7, block2, block3
    block2:
        v8 = iconst.i8 0
        jump block4(v8, v0)
    block3:
        v9 = iconst.i8 1
        jump block4(v9, v5)
    block4(v10: i8, v11: i64):
        v12 = iconst.i64 1
        v13 = iadd v4, v12
        v14 = select v10, v6, v11
        v15 = iadd v6, v14
        v16 = icmp ult v13, v1
        brif v16, block1(v13, v11, v15), block5
    block5:
        return v15
    }";
    // v2 is the undefined zero; v3 is a real 0 (the counter's start).
    let (after, n) = run(src, &[2]);
    assert!(n >= 1, "{after}");
}

#[test]
fn two_values_stay() {
    let src = "function %f(i64, i64) -> i64 {
    block0(v0: i64, v1: i64):
        v2 = iconst.i64 0
        v3 = icmp sge v0, v2
        brif v3, block1, block2
    block1:
        jump block3(v0)
    block2:
        jump block3(v1)
    block3(v4: i64):
        return v4
    }";
    let (_, n) = run(src, &[2]);
    assert_eq!(n, 0);
}

#[test]
fn arm_value_does_not_dominate() {
    let src = "function %f(i64) -> i64 {
    block0(v0: i64):
        v1 = iconst.i64 0
        v2 = icmp sge v0, v1
        brif v2, block1, block2
    block1:
        v3 = iconst.i64 5
        v4 = iadd v0, v3
        jump block3(v4)
    block2:
        v5 = iconst.i64 0
        jump block3(v5)
    block3(v6: i64):
        return v6
    }";
    let (_, n) = run(src, &[5]);
    assert_eq!(n, 0);
}

/// A loop header whose parameter gets the undefined zero on entry and a
/// value computed in the header itself on the back edge: that value does
/// not dominate the parameter, so it stays (blind-sig's build crashed
/// in the egraph on the use-before-def this left).
#[test]
fn header_value_does_not_dominate_its_own_param() {
    let src = "function %f(i64, i64) -> i64 {
    block0(v0: i64, v1: i64):
        v2 = iconst.i64 0
        v3 = iconst.i64 0
        jump block1(v3, v2)
    block1(v4: i64, v5: i64):
        v6 = iadd v5, v0
        v7 = iconst.i64 1
        v8 = iadd v4, v7
        v9 = icmp ult v8, v1
        brif v9, block1(v8, v6), block2
    block2:
        return v6
    }";
    let (_, n) = run(src, &[2]);
    assert_eq!(n, 0);
}

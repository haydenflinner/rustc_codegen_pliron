//! memcmpk: `memcmp(a, b, n)` with a constant small `n` expands inline.
//! Exact (the sign) for n in {1, 2, 4, 8}; other n only under zero tests.
//! Differentially checked against the real libcall; the functions return
//! the result's sign, all memcmp promises.

use cranelift_codegen::ir::{Function, InstructionData};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn src(n: i64, tail: &str) -> String {
    format!(
        "function %f(i64, i64) -> i8, i8 {{
    sig0 = (i64, i64, i64) -> i32
    fn0 = %Memcmp sig0
block0(v0: i64, v1: i64):
    v2 = iconst.i64 {n}
    v3 = call fn0(v0, v1, v2)
    v4 = iconst.i32 0
{tail}
}}"
    )
}

const SIGN: &str = "    v5 = icmp slt v3, v4\n    v6 = icmp sgt v3, v4\n    return v5, v6";
const ZERO: &str = "    v5 = icmp eq v3, v4\n    v6 = icmp ne v4, v3\n    return v5, v6";

fn calls(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| matches!(f.dfg.insts[i], InstructionData::Call { .. }))
        .count()
}

fn expanded(src: &str) -> Function {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    pliron_clif_opt::memcmpk::run(&mut after, &[], "f");
    if calls(&after) == 0 {
        cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder())).unwrap();
        // Half the trials point both args at the same bytes, so equal
        // ranges get compared too.
        let cfg = Config { trials: 128, ..Config::default() };
        match check(&before, &after, &Facts::default(), &cfg) {
            Err(m) => panic!("{m}\n{}", after.display()),
            Ok(s) => assert!(s.compared > 0, "nothing compared: {:?}", s.why),
        }
    }
    after
}

#[test]
fn exact_sizes_expand_with_sign() {
    for n in [1, 2, 4, 8] {
        assert_eq!(calls(&expanded(&src(n, SIGN))), 0, "n={n}");
    }
}

#[test]
fn odd_size_expands_under_zero_tests() {
    for n in [3, 7, 13, 16] {
        assert_eq!(calls(&expanded(&src(n, ZERO))), 0, "n={n}");
    }
}

#[test]
fn odd_size_with_sign_use_stays() {
    assert_eq!(calls(&expanded(&src(3, SIGN))), 1);
}

#[test]
fn large_size_stays() {
    assert_eq!(calls(&expanded(&src(17, ZERO))), 1);
}

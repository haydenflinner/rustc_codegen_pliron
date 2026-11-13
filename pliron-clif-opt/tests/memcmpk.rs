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

/// `memcmp` reached through a user symbol (how rustc's `compare_bytes`
/// lowers: `u0:N` named `memcmp` in the module's table). The checker runs
/// it as the libcall only with `Facts::memcmp`; without it the call is an
/// opaque stub and the expansion looks like a mismatch (what the corpus
/// replay reported before the fact existed).
#[test]
fn user_symbol_memcmp_checks_against_the_libcall() {
    let src = "function %f(i64, i64) -> i8, i8 {
    sig0 = (i64, i64, i64) -> i32
    fn0 = u0:5 sig0
block0(v0: i64, v1: i64):
    v2 = iconst.i64 4
    v3 = call fn0(v0, v1, v2)
    v4 = iconst.i32 0
    v5 = icmp slt v3, v4
    v6 = icmp sgt v3, v4
    return v5, v6
}";
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let names: Vec<String> = (0..6).map(|i| if i == 5 { "memcmp".into() } else { String::new() }).collect();
    assert_eq!(pliron_clif_opt::memcmpk::run(&mut after, &names, "f"), 1);
    let cfg = Config { trials: 128, ..Config::default() };
    let fr = cranelift_codegen::ir::FuncRef::from_u32(0);
    let facts = Facts { memcmp: [fr].into_iter().collect(), ..Facts::default() };
    let s = check(&before, &after, &facts, &cfg).unwrap_or_else(|m| panic!("{m}\n{}", after.display()));
    assert!(s.compared > 0);
    assert!(check(&before, &after, &Facts::default(), &cfg).is_err(), "the stub should disagree");
}

/// serde's derived field visitors: `len == 5` guards
/// `memcmp(s, "title", len)` with the variable length. The guard pins it.
#[test]
fn length_pinned_by_dominating_guard() {
    let src = "function %f(i64, i64, i64) -> i8 {
    sig0 = (i64, i64, i64) -> i32
    fn0 = %Memcmp sig0
block0(v0: i64, v1: i64, v20: i64):
    v21 = iconst.i64 7
    v2 = band v20, v21
    v3 = iconst.i64 5
    v4 = icmp eq v2, v3
    brif v4, block1, block2
block1:
    v5 = call fn0(v0, v1, v2)
    v6 = iconst.i32 0
    v7 = icmp eq v5, v6
    return v7
block2:
    v8 = iconst.i8 0
    return v8
}";
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::memcmpk::run(&mut after, &[], "f"), 1, "{}", after.display());
    let s = check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", after.display()));
    assert!(s.compared > 0);
}

fn has(f: &Function, op: cranelift_codegen::ir::Opcode) -> bool {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).any(|i| f.dfg.insts[i].opcode() == op)
}

/// A power-of-two size whose result is only tested against zero is one
/// compare of the two loads, not the byte-swapped three-way form
/// (`starts_with(b"\xFF\xFE")`, encoding_rs's `for_bom`). The sign uses
/// keep the swaps.
#[test]
fn power_of_two_under_zero_tests_is_a_plain_compare() {
    use cranelift_codegen::ir::Opcode;
    for n in [2, 4, 8] {
        let f = expanded(&src(n, ZERO));
        assert_eq!(calls(&f), 0, "n={n}");
        assert!(!has(&f, Opcode::Bswap), "n={n}: {}", f.display());
        assert!(has(&expanded(&src(n, SIGN)), Opcode::Bswap), "n={n}");
    }
}

/// One side is a constant data object: its bytes are read at compile
/// time, so no load of it is left and each chunk is a compare with an
/// immediate, or-ed (the `ne` test) or and-ed (the `eq` test).
#[test]
fn constant_side_is_read_at_compile_time() {
    use cranelift_codegen::ir::Opcode;
    let src = "function %f(i64) -> i8, i8 {
    gv0 = symbol colocated u1:7
    sig0 = (i64, i64, i64) -> i32
    fn0 = %Memcmp sig0
block0(v0: i64):
    v1 = symbol_value.i64 gv0
    v2 = iconst.i64 3
    v3 = call fn0(v0, v1, v2)
    v4 = iconst.i32 0
    v5 = icmp eq v3, v4
    v6 = icmp ne v3, v4
    return v5, v6
}";
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut tables = pliron_clif_opt::memcmpk::Tables::default();
    tables.insert(7, std::sync::Arc::from(&b"\xEF\xBB\xBF"[..]));
    let mut f = before.clone();
    assert_eq!(pliron_clif_opt::memcmpk::run_with(&mut f, &[], "f", &tables, true), 1);
    cranelift_codegen::verify_function(&f, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()))
        .unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let text = f.display().to_string();
    // Two loads (of the argument), none through the symbol.
    let loads = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count();
    assert_eq!(loads, 2, "{text}");
    assert!(text.contains("iconst.i16 -17425"), "0xBBEF as i16: {text}");
    assert!(text.contains("iconst.i8 -65"), "0xBF as i8: {text}");
    assert!(has(&f, Opcode::Band) && has(&f, Opcode::Bor), "{text}");
    // A big-endian target, a table too short, or the knob's off state
    // (no table): the loads stay.
    for (t, little) in [(&tables, false), (&pliron_clif_opt::memcmpk::Tables::default(), true)] {
        let mut g = before.clone();
        assert_eq!(pliron_clif_opt::memcmpk::run_with(&mut g, &[], "f", t, little), 1);
        assert!(!g.display().to_string().contains("iconst.i16"), "{}", g.display());
    }
    let mut short = pliron_clif_opt::memcmpk::Tables::default();
    short.insert(7, std::sync::Arc::from(&b"\xEF\xBB"[..]));
    let mut g = before.clone();
    assert_eq!(pliron_clif_opt::memcmpk::run_with(&mut g, &[], "f", &short, true), 1);
    assert!(!g.display().to_string().contains("iconst.i16"), "{}", g.display());
}

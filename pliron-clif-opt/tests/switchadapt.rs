//! `PLIRON_SWITCHADAPT`: sparse grouped switch cases become direct
//! equality/range/bitmask tests. The html-lexer shape test runs every byte
//! through the original and rewritten machine code; its `has_table` assert
//! fails without the transform.
#![cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    any(target_os = "macos", target_os = "linux")
))]

mod jtcommon;
use cranelift_codegen::ir::{Function, Opcode};
use jtcommon::*;

/// Byte switch over raw values 9..=62: whitespace bytes 9/10/12/32 share
/// block2, slash (47) is block3, greater-than (62) is block4, everything
/// else is the default block1. This is lol_html's tag-name state shape.
fn html_shape() -> String {
    let mut arms = vec!["block1".to_string(); 54];
    for i in [0, 1, 3, 23] {
        arms[i] = "block2".into();
    }
    arms[38] = "block3".into();
    arms[53] = "block4".into();
    format!(
        "function %f(i8, i64) -> i64 {{\n\
         block0(v0: i8, v1: i64):\n\
         \x20   v2 = iconst.i8 9\n\
         \x20   v3 = isub v0, v2\n\
         \x20   v4 = uextend.i32 v3\n\
         \x20   br_table v4, block1, [{}]\n\
         block1:\n\
         \x20   v10 = iconst.i64 1000\n\
         \x20   v11 = iadd v1, v10\n\
         \x20   return v11\n\
         block2:\n\
         \x20   v12 = iconst.i64 2000\n\
         \x20   v13 = iadd v1, v12\n\
         \x20   return v13\n\
         block3:\n\
         \x20   v14 = iconst.i64 3000\n\
         \x20   v15 = iadd v1, v14\n\
         \x20   return v15\n\
         block4:\n\
         \x20   v16 = iconst.i64 4000\n\
         \x20   v17 = iadd v1, v16\n\
         \x20   return v17\n\
         }}\n",
        arms.join(", ")
    )
}

fn has_table(f: &Function) -> bool {
    f.layout
        .blocks()
        .filter_map(|b| f.layout.last_inst(b))
        .any(|i| f.dfg.insts[i].opcode() == Opcode::BrTable)
}

fn has_ushr(f: &Function) -> bool {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .any(|i| f.dfg.insts[i].opcode() == Opcode::Ushr)
}

fn want(b: u8, x: i64) -> i64 {
    let idx = b.wrapping_sub(9);
    x + match (idx, b) {
        // The subtraction wraps below 9; those indices are far out of range.
        (0..=53, 9 | 10 | 12 | 32) => 2000,
        (0..=53, 47) => 3000,
        (0..=53, 62) => 4000,
        _ => 1000,
    }
}

#[test]
fn lexer_shape_becomes_bit_tests_and_keeps_every_byte() {
    let before = parse(&html_shape());
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::switchadapt::run(&mut after), 1, "{}", after.display());
    assert!(!has_table(&after), "{}", after.display());
    assert!(has_ushr(&after), "no bitmask test:\n{}", after.display());

    let a = compile_func(before);
    let b = compile_func(after);
    let fa: extern "C" fn(u8, i64) -> i64 = unsafe { std::mem::transmute(a.entry()) };
    let fb: extern "C" fn(u8, i64) -> i64 = unsafe { std::mem::transmute(b.entry()) };
    for byte in 0..=255u8 {
        assert_eq!(fa(byte, 7), want(byte, 7), "original byte {byte}");
        assert_eq!(fb(byte, 7), want(byte, 7), "rewritten byte {byte}");
    }
}

#[test]
fn block_arguments_are_preserved_on_every_edge() {
    let src = "function %f(i32, i64, i64) -> i64 {
block0(v0: i32, v1: i64, v2: i64):
    br_table v0, block1(v1), [block2(v2), block2(v2), block1(v1), block2(v2), block1(v1), block1(v1)]
block1(v10: i64):
    return v10
block2(v11: i64):
    v12 = iconst.i64 1
    v13 = iadd v11, v12
    return v13
}
";
    let before = parse(src);
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::switchadapt::run(&mut after), 1, "{}", after.display());
    assert!(!has_table(&after));
    let a = compile_func(before);
    let b = compile_func(after);
    let fa: extern "C" fn(i32, i64, i64) -> i64 = unsafe { std::mem::transmute(a.entry()) };
    let fb: extern "C" fn(i32, i64, i64) -> i64 = unsafe { std::mem::transmute(b.entry()) };
    for i in [-1, 0, 1, 2, 3, 4, 5, 6, 100] {
        assert_eq!(fa(i, 70, 20), fb(i, 70, 20), "index {i}");
        assert_eq!(fb(i, 70, 20), if matches!(i, 0 | 1 | 3) { 21 } else { 70 });
    }
}

#[test]
fn dense_singleton_switches_stay_tables() {
    // Eight singleton destinations in eight entries: a linear direct chain
    // is not a bit-test win and dense enum dispatch is brchain/table territory.
    let arms = (2..=9).map(|k| format!("block{k}")).collect::<Vec<_>>().join(", ");
    let mut src = format!("function %f(i32, i64) -> i64 {{\nblock0(v0: i32, v1: i64):\n    br_table v0, block1, [{arms}]\nblock1:\n    return v1\n");
    for k in 2..=9 {
        src += &format!("block{k}:\n    v{k} = iconst.i64 {k}\n    v1{k} = iadd v1, v{k}\n    return v1{k}\n");
    }
    src += "}\n";
    let mut f = parse(&src);
    assert_eq!(pliron_clif_opt::switchadapt::run(&mut f), 0);
    assert!(has_table(&f));
}

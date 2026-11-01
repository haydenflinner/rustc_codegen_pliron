//! demand: under serde_json's SWAR mask (`0x8080..`), `c ^ 0xdd..` and
//! `c ^ 0xa3..` (folded `!(c ^ 0x22..)`, `!(c ^ 0x5c..)`) read only bits
//! where they equal `!c`; they become `bnot c` and the three `band`s
//! factor into one. A constant that differs from `!c`/`c` on a demanded
//! bit stays. Differentially checked.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 0xdfdf_dfdf_dfdf_dfe0
    v2 = iadd v0, v1
    v3 = bnot v0
    v4 = band v2, v3
    v5 = iconst.i64 0x2222_2222_2222_2222
    v6 = bxor v0, v5
    v7 = iconst.i64 0xfefe_fefe_fefe_feff
    v8 = iadd v6, v7
    v9 = iconst.i64 0xdddd_dddd_dddd_dddd
    v10 = bxor v0, v9
    v11 = band v8, v10
    v12 = iconst.i64 0x5c5c_5c5c_5c5c_5c5c
    v13 = bxor v0, v12
    v14 = iadd v13, v7
    v15 = iconst.i64 0xa3a3_a3a3_a3a3_a3a3
    v16 = bxor v0, v15
    v17 = band v14, v16
    v18 = bor v4, v11
    v19 = bor v17, v18
    v20 = iconst.i64 0x8080_8080_8080_8080
    v21 = band v19, v20
    return v21
}";

/// Live `op` insts (result used); the rewrite leaves dead ones for DCE.
fn count(f: &Function, op: Opcode) -> usize {
    let used: std::collections::HashSet<_> = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .flat_map(|i| f.dfg.inst_values(i).map(|v| f.dfg.resolve_aliases(v)).collect::<Vec<_>>())
        .collect();
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op && used.contains(&f.dfg.first_result(i)))
        .count()
}

fn run(src: &str) -> (Function, Function) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    pliron_clif_opt::demand::run(&mut after);
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder())).unwrap();
    let s = check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", after.display()));
    assert!(s.compared > 0);
    (before, after)
}

#[test]
fn swar_nots_share_one_band() {
    let (before, after) = run(SRC);
    assert_eq!(count(&before, Opcode::Band), 4);
    // The three `x & !c` terms factor: one `band` with `!c`, plus the mask.
    assert_eq!(count(&after, Opcode::Band), 2, "{}", after.display());
}

#[test]
fn constant_clear_on_demanded_bits_is_the_value() {
    // 0x5d.. has bit 7 clear in every byte: under the mask `c ^ 0x5d..`
    // reads like `c` itself (and `c ^ 0xa3..` still becomes `!c`).
    let src = SRC.replace("0xdddd_dddd_dddd_dddd", "0x5d5d_5d5d_5d5d_5d5d");
    let (before, after) = run(&src);
    assert_eq!(count(&after, Opcode::Bxor), count(&before, Opcode::Bxor) - 2, "{}", after.display());
}

#[test]
fn constant_mixed_on_demanded_bits_stays() {
    // Bit 7 set in some bytes only: neither `c` nor `!c` on those bits;
    // only `c ^ 0xa3..` goes.
    let src = SRC.replace("0xdddd_dddd_dddd_dddd", "0xdd5d_dddd_dddd_dddd");
    let (before, after) = run(&src);
    assert_eq!(count(&after, Opcode::Bxor), count(&before, Opcode::Bxor) - 1, "{}", after.display());
}

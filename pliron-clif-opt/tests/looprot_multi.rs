//! `PLIRON_LOOPROT_MULTI=<k>`: a loop whose header only tests the exit and
//! that has several latches (one per `match` arm, lol_html's
//! `LocalNameHash::update`) takes a copy of the test in every latch, so each
//! arm ends in one conditional branch back instead of `jump header`.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use pliron_clif_opt::vmap::VMap;
use rustc_hash::FxHashMap;

fn rotate(src: &str) -> (Function, Function, usize) {
    unsafe { std::env::set_var("PLIRON_LOOPROT_MULTI", "4") };
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let k = pliron_clif_opt::looprot::run_with(&mut after, &mut Analysis::default(), &mut VMap::default(), false, &FxHashMap::default());
    (before, after, k)
}

/// Jumps back to block1 (the header) left in the function.
fn jumps_to_header(f: &Function) -> usize {
    let h = f.layout.blocks().nth(1).unwrap();
    f.layout
        .blocks()
        .filter_map(|b| f.layout.last_inst(b))
        .filter(|&t| f.dfg.insts[t].opcode() == Opcode::Jump)
        .filter(|&t| {
            f.dfg.insts[t].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)[0].block(&f.dfg.value_lists) == h
        })
        .count()
}

/// `for &c in s { h = match c { alpha => h*32 + (c & 31) + 5, digit => .., _ => !0 } }`
const HASH: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    v3 = iadd v0, v1
    jump block1(v0, v2)
block1(v4: i64, v5: i64):
    v6 = icmp eq v4, v3
    brif v6, block9(v5), block2
block2:
    v7 = uload8.i64 notrap v4
    v8 = iconst.i64 1
    v9 = iadd v4, v8
    v10 = iconst.i64 32
    v11 = bor v7, v10
    v12 = iconst.i64 97
    v13 = isub v11, v12
    v14 = iconst.i64 26
    v15 = icmp ult v13, v14
    brif v15, block3, block4
block3:
    v16 = iconst.i64 31
    v17 = band v7, v16
    v18 = iconst.i64 5
    v19 = iadd v17, v18
    v20 = ishl v5, v18
    v21 = bor v20, v19
    jump block1(v9, v21)
block4:
    v22 = iconst.i64 49
    v23 = isub v7, v22
    v24 = iconst.i64 6
    v25 = icmp ult v23, v24
    brif v25, block5, block6
block5:
    v26 = iconst.i64 5
    v27 = ishl v5, v26
    v28 = bor v27, v23
    jump block1(v9, v28)
block6:
    v29 = iconst.i64 -1
    jump block1(v9, v29)
block9(v30: i64):
    return v30
}";

#[test]
fn every_latch_takes_the_test() {
    let (before, after, k) = rotate(HASH);
    assert_eq!(k, 1, "{}", after.display());
    // The entry's jump plus three latches; afterwards only the entry.
    assert_eq!(jumps_to_header(&before), 4);
    assert_eq!(jumps_to_header(&after), 1, "latch left jumping to the header:\n{}", after.display());
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

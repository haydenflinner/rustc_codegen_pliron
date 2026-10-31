//! ivptr: a loop pointer rebuilt from the index on the back edge
//! (`p' = B + 8*i`, `i' = i + 1`, entry `p0 = B + 8*(i0 - 1)`) becomes
//! `p + 8`. Differentially checked; a mismatched entry is left alone.

use cranelift_codegen::ir::Opcode;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

const SRC: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 1
    v3 = iconst.i64 0
    v4 = iadd v0, v3
    jump block1(v3, v4, v2)
block1(v10: i64, v11: i64, v12: i64):
    v13 = uload8.i64 notrap v11
    v14 = iadd v10, v13
    v15 = iconst.i64 3
    v16 = ishl v12, v15
    v17 = iadd v0, v16
    v18 = iconst.i64 1
    v19 = iadd v12, v18
    v20 = icmp ult v19, v1
    brif v20, block1(v14, v17, v19), block2
block2:
    return v14
}";

/// The back edge's pointer arg is `iadd v11, k`.
fn back_iadd_imm(f: &cranelift_codegen::ir::Function) -> bool {
    let b1 = cranelift_codegen::ir::Block::from_u32(1);
    let t = f.layout.last_inst(b1).unwrap();
    let dests = f.dfg.insts[t].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables);
    let Some(cranelift_codegen::ir::BlockArg::Value(v)) = dests[0].args(&f.dfg.value_lists).nth(1) else { return false };
    let Some(i) = f.dfg.value_def(v).inst() else { return false };
    f.dfg.insts[i].opcode() == Opcode::Iadd && f.dfg.inst_args(i).contains(&cranelift_codegen::ir::Value::from_u32(11))
}

#[test]
fn index_rebuilt_pointer_becomes_increment() {
    let before = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::ivptr::run(&mut after, &mut Analysis::default()), 1, "{}", after.display());
    assert!(back_iadd_imm(&after), "{}", after.display());
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

#[test]
fn mismatched_entry_is_left_alone() {
    // p0 = B + 8 does not equal B + 8*(i0 - 1) = B.
    let src = SRC.replace("v4 = iadd v0, v3", "v4 = iadd v0, v15b").replace("    v4 = iadd v0, v15b\n", "    v40 = iconst.i64 8\n    v4 = iadd v0, v40\n");
    let mut f = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    assert_eq!(pliron_clif_opt::ivptr::run(&mut f, &mut Analysis::default()), 0, "{}", f.display());
}

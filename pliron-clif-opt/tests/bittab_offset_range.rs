//! `bittab` folds offset range tests (`x - lo <=u w`), the form
//! `PLIRON_SWITCH_CHAIN` and brchain emit for a run of cases.
//! serde_json's `parse_whitespace` (`b' ' | b'\n' | b'\t' | b'\r'`)
//! became `x - 9 <=u 1`, `x == 13`, `x == 32` under the switch chain. The
//! tree no longer folded into one mask test, and the hot `has_next_*`
//! loops grew (json +2.8%).

use cranelift_codegen::ir::{Function, InstructionData, Opcode};
use pliron_clif_opt::bittab;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const TREE: &str = "function %f(i8) -> i32 {
    block0(v0: i8):
        v1 = iconst.i8 -9
        v2 = iadd v0, v1
        v3 = iconst.i8 1
        v4 = icmp ule v2, v3
        brif v4, block9, block1

    block1:
        v5 = iconst.i8 13
        v6 = icmp eq v0, v5
        brif v6, block9, block2

    block2:
        v7 = iconst.i8 32
        v8 = icmp eq v0, v7
        brif v8, block9, block8

    block8:
        v11 = iconst.i32 0
        return v11

    block9:
        v12 = iconst.i32 1
        return v12
    }";

#[test]
fn offset_range_test_joins_the_mask() {
    unsafe { std::env::set_var("PLIRON_SWITCH_CHAIN", "1") };
    let before = cranelift_reader::parse_functions(TREE).unwrap().remove(0);
    let mut after: Function = before.clone();
    assert!(bittab::run(&mut after) > 0, "the tree should fold:\n{}", after.display());
    // One mask over bits 9, 10, 13 and 32.
    let mask = after
        .layout
        .blocks()
        .flat_map(|b| after.layout.block_insts(b))
        .any(|i| matches!(after.dfg.insts[i], InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if imm.bits() == 0x1_0000_2600));
    assert!(mask, "{}", after.display());
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(&after, &flags).unwrap();
    // i8: 1024 trials cover the 256 inputs.
    check(&before, &after, &Facts::default(), &Config { trials: 1024, ..Config::default() }).unwrap();
}

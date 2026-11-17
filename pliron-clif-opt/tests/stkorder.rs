//! `stkorder`: one SipHash half-round, `v0 += v1; v1 = rotl(v1, 13);
//! v1 ^= v0`, as the egraph leaves it: the sum (used twice) first, the
//! rotate between it and the `bxor` that reads it.

use cranelift_codegen::ir::{Function, Inst, InstructionData, Opcode};
use cranelift_codegen::settings;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const ROUND: &str = "function %f(i64, i64) -> i64, i64 {
block0(v0: i64, v1: i64):
    v2 = iadd v0, v1
    v3 = iconst.i64 13
    v4 = rotl v1, v3
    v5 = bxor v4, v2
    v6 = iconst.i64 32
    v7 = rotl v2, v6
    return v7, v5
}";

fn insts(f: &Function) -> Vec<Inst> {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).collect()
}

#[test]
fn sum_moves_next_to_its_first_use_and_leads_it() {
    let before = cranelift_reader::parse_functions(ROUND).unwrap().remove(0);
    let mut f = before.clone();
    assert!(pliron_clif_opt::stkorder::run(&mut f) >= 2, "{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let is = insts(&f);
    let add = is.iter().position(|&i| f.dfg.insts[i].opcode() == Opcode::Iadd).unwrap();
    let xor = is[add + 1];
    // The sum is right before the `bxor` and is its first operand.
    match f.dfg.insts[xor] {
        InstructionData::Binary { opcode: Opcode::Bxor, args } => {
            assert_eq!(args[0], f.dfg.first_result(is[add]), "{}", f.display())
        }
        _ => panic!("the sum is not next to the bxor:\n{}", f.display()),
    }
    check(&before, &f, &Facts::default(), &Config::default()).unwrap_or_else(|m| panic!("{m}"));
}

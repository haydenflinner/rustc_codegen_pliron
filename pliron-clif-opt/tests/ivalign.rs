//! ivalign: re-time a rotated loop's tested induction variable so the
//! latch tests the `iadd p, s` result — `add; cmp; b.cond` instead of
//! `cmp; mov; b` — differentially checked against the interpreter.

use cranelift_codegen::ir::{Function, InstructionData, Opcode, ValueDef};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// The paramsink-produced shape from sieve's marking loop: the loop
/// param is pre-incremented at the entry edge and tested directly while
/// the in-block iadd is carried on the backedge.
const SHIFTED: &str = "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iadd v2, v1
    jump block1(v2, v3)

block1(v4: i64, v5: i64):
    v6 = iadd v4, v1
    v7 = icmp ult v5, v0
    v8 = iadd v5, v1
    brif v7, block1(v6, v8), block2(v5)

block2(v9: i64):
    return v9
}
";

/// Does some icmp read an `iadd` result — the retimed latch shape?
fn tested_is_iadd(f: &Function) -> bool {
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                args,
                ..
            } = f.dfg.insts[i]
            else {
                continue;
            };
            for a in args {
                if let ValueDef::Result(di, _) = f.dfg.value_def(a)
                    && matches!(
                        f.dfg.insts[di],
                        InstructionData::Binary {
                            opcode: Opcode::Iadd,
                            ..
                        }
                    )
                {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn retimes_shifted_param() {
    let before = parse(SHIFTED);
    let mut after = before.clone();
    let k = pliron_clif_opt::ivalign::run(&mut after, &mut Analysis::default());
    assert!(k > 0, "no shift:\n{}", after.display());
    assert!(tested_is_iadd(&after), "icmp still reads param:\n{}", after.display());
    let cfg = Config::default();
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

/// The backedge iadd feeding a second consumer pins its value at the
/// old timing — the shift would observe `p` off by one step.
#[test]
fn skips_multi_use_iadd() {
    let src = "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iadd v2, v1
    jump block1(v2, v3)

block1(v4: i64, v5: i64):
    v6 = iadd v4, v1
    v7 = icmp ult v5, v0
    v8 = iadd v5, v1
    v10 = imul v8, v1
    brif v7, block1(v6, v8), block2(v10)

block2(v9: i64):
    return v9
}
";
    let mut after = parse(src);
    let k = pliron_clif_opt::ivalign::run(&mut after, &mut Analysis::default());
    assert_eq!(k, 0, "shifted despite extra iadd use:\n{}", after.display());
}

/// An entry that does not pass `iadd a, s` cannot be retimed.
#[test]
fn skips_unmatched_entry() {
    let src = "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    jump block1(v2, v2)

block1(v4: i64, v5: i64):
    v6 = iadd v4, v1
    v7 = icmp ult v5, v0
    v8 = iadd v5, v1
    brif v7, block1(v6, v8), block2(v5)

block2(v9: i64):
    return v9
}
";
    let mut after = parse(src);
    let k = pliron_clif_opt::ivalign::run(&mut after, &mut Analysis::default());
    assert_eq!(k, 0, "shifted despite unmatched entry:\n{}", after.display());
}

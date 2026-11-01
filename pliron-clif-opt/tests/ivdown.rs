//! ivdown: an index used only by its step and an `eq`/`ne` exit test
//! against an invariant `n` counts down to zero instead (entry `n - i0`),
//! and the test becomes the `brif` on the counter. Differentially checked
//! for `ne` on `i'`, `eq` on `i` (targets swapped), and a non-unit step; an
//! index with another use is left alone.

use cranelift_codegen::ir::{Block, InstructionData, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

/// Trip count 1..=16 from the inputs: `n = i0 + c*((v1 & 15) + 1)`.
fn src(step: i64, test: &str) -> String {
    format!(
        "function %f(i64, i64, i64) -> i64 {{
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 15
    v4 = band v1, v3
    v5 = iconst.i64 1
    v6 = iadd v4, v5
    v7 = iconst.i64 {step}
    v8 = imul v6, v7
    v9 = iadd v2, v8
    v30 = iconst.i64 0
    jump block1(v30, v0, v2)
block1(v10: i64, v11: i64, v12: i64):
    v13 = uload8.i64 notrap v11
    v14 = iadd v10, v13
    v15 = iconst.i64 1
    v16 = iadd v11, v15
    v17 = iconst.i64 {step}
    v19 = iadd v12, v17
{test}
block2:
    return v14
}}"
    )
}

const NE_NEXT: &str = "    v20 = icmp ne v19, v9\n    brif v20, block1(v14, v16, v19), block2";
const EQ_CUR: &str = "    v20 = icmp eq v9, v12\n    brif v20, block2, block1(v14, v16, v19)";

/// The loop's `brif` tests a block-1 param-derived counter directly.
fn brif_on_counter(f: &cranelift_codegen::ir::Function) -> bool {
    let b1 = Block::from_u32(1);
    let t = f.layout.last_inst(b1).unwrap();
    let InstructionData::Brif { arg, .. } = f.dfg.insts[t] else { return false };
    f.dfg.value_def(arg).inst().is_none_or(|i| f.dfg.insts[i].opcode() != Opcode::Icmp)
}

fn check_rewrite(src: &str) {
    // The loop reads bytes at `v0..`: give it a buffer.
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    assert_eq!(pliron_clif_opt::ivdown::run(&mut after, &mut Analysis::default()), 1, "{}", after.display());
    assert!(brif_on_counter(&after), "{}", after.display());
    cranelift_codegen::verify_function(&after, &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder())).unwrap();
    let cfg = Config { trials: 64, ..Config::default() };
    match check(&before, &after, &Facts::default(), &cfg) {
        Err(m) => panic!("{m}\n{}", after.display()),
        Ok(s) => assert!(s.compared > 0, "nothing compared: {:?}", s.why),
    }
}

#[test]
fn ne_on_next_counts_down() {
    check_rewrite(&src(1, NE_NEXT));
}

#[test]
fn eq_on_current_counts_down() {
    check_rewrite(&src(1, EQ_CUR));
}

#[test]
fn non_unit_step_counts_down() {
    check_rewrite(&src(3, NE_NEXT));
}

#[test]
fn index_with_another_use_stays() {
    let s = src(1, NE_NEXT).replace("v14 = iadd v10, v13", "v40 = iadd v10, v13\n    v14 = iadd v40, v12");
    let mut f = cranelift_reader::parse_functions(&s).unwrap().remove(0);
    assert_eq!(pliron_clif_opt::ivdown::run(&mut f, &mut Analysis::default()), 0, "{}", f.display());
}

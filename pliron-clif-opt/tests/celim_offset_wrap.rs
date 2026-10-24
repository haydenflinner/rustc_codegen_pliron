//! celim must not decide `y - 1 <u x` from a dominating `x >u y`: it holds
//! unless `y == 0`, where `y - 1` wraps to `u64::MAX` and it is false —
//! so it is undecided, never "always false". celim folded it to false in
//! stage-2 rustc's `partitioning::partition` (`while cgus.len() > max {
//! cgus[max - 1] }`), and the bounds check fired with "the len is 110 but
//! the index is 15" compiling core (2026-10-24).

use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::ir::Function;
use cranelift_interpreter::environment::FunctionStore;
use cranelift_interpreter::interpreter::{Interpreter, InterpreterState};
use cranelift_interpreter::step::ControlFlow;

const SRC: &str = "function %t(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = icmp ugt v0, v1
    brif v2, block1, block2
block1:
    v3 = iconst.i64 -1
    v4 = iadd v1, v3
    v5 = icmp ult v4, v0
    return v5
block2:
    v6 = iconst.i8 7
    return v6
}";

fn eval(f: &Function, len: i64, max: i64) -> i8 {
    let mut fs = FunctionStore::default();
    fs.add(f.name.to_string(), f);
    let mut it = Interpreter::new(InterpreterState::default().with_function_store(fs));
    match it.call_by_name(&f.name.to_string(), &[DataValue::I64(len), DataValue::I64(max)]).unwrap() {
        ControlFlow::Return(r) => match r[0] {
            DataValue::I8(x) => x,
            ref o => panic!("{o:?}"),
        },
        _ => panic!("no return"),
    }
}

#[test]
fn max_minus_one_below_len_is_not_folded_false() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let cases = [(110, 16), (5, 0), (2, 1), (3, 3)];
    let before: Vec<i8> = cases.iter().map(|&(l, m)| eval(&f, l, m)).collect();
    pliron_clif_opt::celim::run(&mut f);
    let after: Vec<i8> = cases.iter().map(|&(l, m)| eval(&f, l, m)).collect();
    assert_eq!(after, before, "celim changed the result:\n{}", f.display());
}

/// The sound swapped-operand fold still happens: `x >u y` gives
/// `y + 1 <=u x` (`y < x` rules out the wrap).
#[test]
fn swapped_sound_offset_query_still_folds() {
    let src = "function %t(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = icmp ugt v0, v1
    brif v2, block1, block2
block1:
    v3 = iconst.i64 1
    v4 = iadd v1, v3
    v5 = icmp ule v4, v0
    return v5
block2:
    v6 = iconst.i8 7
    return v6
}";
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    pliron_clif_opt::celim::run(&mut f);
    let b1 = cranelift_codegen::ir::Block::from_u32(1);
    let folded = f.layout.block_insts(b1).any(|i| {
        matches!(f.dfg.insts[i], cranelift_codegen::ir::InstructionData::UnaryImm { opcode: cranelift_codegen::ir::Opcode::Iconst, imm }
            if f.dfg.value_type(f.dfg.first_result(i)) == cranelift_codegen::ir::types::I8 && imm.bits() == 1)
    });
    assert!(folded, "expected `y + 1 <=u x` folded to true:\n{}", f.display());
}

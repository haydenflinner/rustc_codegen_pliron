//! post_sweep's `selord` must not fold a compare whose result is constant
//! into the compare it selects on. Stage-2 rustc's `collect_and_apply`
//! (fold_captured_lifetime_args) had, on an `Option<Ordering>`-shaped
//! byte (2 = `None` niche, -1 = `Less`):
//!
//!     x  = select (a <u b), 2, -1
//!     c  = icmp eq x, -1              ; !(a <u b)
//!     t  = icmp eq (uextend x), 1     ; 2 == 1 / 255 == 1: always false
//!     r  = select c, 0, t             ; always false
//!
//! and selord rewrote `r` to `icmp ult a, b`, so lifetimes kept their NLL
//! vars and core failed to compile with 28 bogus "non-defining use of
//! `impl Trait`" errors (2026-10-24).

use cranelift_codegen::ir::Function;
use cranelift_interpreter::environment::FunctionStore;
use cranelift_interpreter::interpreter::{Interpreter, InterpreterState};
use cranelift_interpreter::step::ControlFlow;
use cranelift_codegen::data_value::DataValue;

const SRC: &str = "function %t(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = icmp ult v0, v1
    v3 = iconst.i8 2
    v4 = iconst.i8 -1
    v5 = select v2, v3, v4
    v6 = icmp eq v5, v4
    v7 = uextend.i32 v5
    v8 = iconst.i32 1
    v9 = icmp eq v7, v8
    v10 = iconst.i8 0
    v11 = select v6, v10, v9
    return v11
}";

fn run(f: &Function, a: i64, b: i64) -> i8 {
    let mut fs = FunctionStore::default();
    fs.add(f.name.to_string(), f);
    let mut it = Interpreter::new(InterpreterState::default().with_function_store(fs));
    match it.call_by_name(&f.name.to_string(), &[DataValue::I64(a), DataValue::I64(b)]).unwrap() {
        ControlFlow::Return(r) => match r[0] {
            DataValue::I8(x) => x,
            ref o => panic!("{o:?}"),
        },
        _ => panic!("no return"),
    }
}

#[test]
fn constant_false_select_chain_stays_false() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let before: Vec<i8> = [(1, 2), (2, 1), (3, 3)].iter().map(|&(a, b)| run(&f, a, b)).collect();
    assert_eq!(before, [0, 0, 0]);
    pliron_clif_opt::clifpeep::iconst128(&mut f, true);
    let after: Vec<i8> = [(1, 2), (2, 1), (3, 3)].iter().map(|&(a, b)| run(&f, a, b)).collect();
    assert_eq!(after, before, "post_sweep changed the result:\n{}", f.display());
}

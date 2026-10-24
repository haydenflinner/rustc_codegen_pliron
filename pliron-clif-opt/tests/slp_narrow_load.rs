//! slp's store pairing must not move a store past a narrow load of its
//! bytes. `plan_store` merges two address-adjacent stores into one vector
//! store at the later one's position; anything in between that may read
//! the earlier store's bytes blocks the merge. A `uload32` (which
//! `mem_range` doesn't describe) passed as harmless, so it read the old
//! word (rustlantis seed 4940: wrong hash at -O, 2026-10-24).

use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::ir::Function;
use cranelift_interpreter::environment::FunctionStore;
use cranelift_interpreter::interpreter::{Interpreter, InterpreterState};
use cranelift_interpreter::step::ControlFlow;
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;

// ss0+0 = -1, then the pair `ss0+0 = 0` / `ss0+4 = 0` with a `uload32`
// of ss0+0 between them: it must see 0.
const SRC: &str = "function %t() -> i64 {
    ss0 = explicit_slot 8, align = 8
block0:
    v0 = stack_addr.i64 ss0
    v1 = iconst.i32 -1
    store notrap v1, v0
    v2 = iconst.i32 0
    store notrap v2, v0
    v3 = uload32.i64 notrap v0
    v6 = iconst.i64 4
    v4 = iadd v0, v6
    v5 = iconst.i32 0
    store notrap v5, v4
    return v3
}";

fn eval(f: &Function) -> i64 {
    let mut fs = FunctionStore::default();
    fs.add(f.name.to_string(), f);
    let mut it = Interpreter::new(InterpreterState::default().with_function_store(fs));
    match it.call_by_name(&f.name.to_string(), &[]).unwrap() {
        ControlFlow::Return(r) => match r[0] {
            DataValue::I64(x) => x,
            ref o => panic!("{o:?}"),
        },
        _ => panic!("no return"),
    }
}

#[test]
fn store_pair_not_moved_past_narrow_load() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    assert_eq!(eval(&f), 0);
    pliron_clif_opt::slp::run(&mut f, &mut Analysis::default(), &FxHashSet::default(), true, false);
    assert_eq!(eval(&f), 0, "slp moved the store past the uload32:\n{}", f.display());
}

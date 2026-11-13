// `phisimp` in code the entry cannot reach (an unwind path whose invoke
// became a call): a block with one incoming edge and no path back to the
// block that edge leaves takes its parameters' values directly, so a
// local's address passed into an inlined callee there no longer reaches
// a `br` operand (which kept SROA off the local). A dead loop is left
// alone: its parameter's value can be computed from the parameter.

use pliron::builtin::ops::ModuleOp;
use pliron::context::{Context, Ptr};
use pliron::irfmt::parsers::spaced;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::parsable::parse_from_str;
use pliron_ir_opt::state::State;
use pliron_llvm::ops::FuncOp;

const SRC: &str = r#"
builtin.module @m {
^block_0_0():
  llvm.func @f: llvm.func <builtin.integer i32(builtin.integer i32) variadic = false> [] {
  ^entry_block_1_0(x: builtin.integer i32):
    llvm.return x
  ^dead_2_0():
    one = llvm.constant <builtin.integer <1: i32>> : builtin.integer i32;
    g = llvm.add x, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.br ^callee_3_0(g)
  ^callee_3_0(p: builtin.integer i32):
    r = llvm.add p, p <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.return r
  ^loop_4_0(q: builtin.integer i32):
    two = llvm.constant <builtin.integer <2: i32>> : builtin.integer i32;
    n = llvm.add q, two <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.br ^loop_4_0(n)
  }
}
"#;

fn func(ctx: &Context, m: Ptr<Operation>) -> Ptr<Operation> {
    pliron_ir_opt::inline::blocks(ctx, m)
        .into_iter()
        .flat_map(|b| pliron_ir_opt::inline::ops(ctx, b))
        .find(|&op| Operation::is_op::<FuncOp>(op, ctx))
        .unwrap()
}

#[test]
fn dead_single_edge_block_params_fold_but_dead_loops_do_not() {
    let ctx = &mut Context::new();
    let m = parse_from_str(spaced(Operation::top_level_parser()), ctx, SRC).expect("parse");
    let m = Operation::get_op::<ModuleOp>(m, ctx).unwrap().get_operation();
    let f = func(ctx, m);
    let st = State::default();
    let args = |ctx: &Context| -> Vec<usize> {
        pliron_ir_opt::inline::blocks(ctx, f).iter().map(|b| b.deref(ctx).get_num_arguments()).collect()
    };
    assert_eq!(args(ctx), [1, 0, 1, 1]);
    // Only the callee entry's parameter goes.
    assert_eq!(pliron_ir_opt::phisimp::run_fn(ctx, &st, f), 1);
    assert_eq!(args(ctx), [1, 0, 0, 1]);
    // Its use now reads the value the dead block computed.
    let bs = pliron_ir_opt::inline::blocks(ctx, f);
    let g = pliron_ir_opt::inline::ops(ctx, bs[1])[1].deref(ctx).get_result(ctx, 0);
    let r = pliron_ir_opt::inline::ops(ctx, bs[2])[0];
    assert!(r.deref(ctx).operands(ctx).all(|o| o == g));
}

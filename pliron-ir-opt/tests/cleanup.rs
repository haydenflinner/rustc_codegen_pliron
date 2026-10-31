// `cleanup::clean_fn` (`PLIRON_INLINE_CLEAN`): unused side-effect-free ops
// go, with the operands they alone kept alive, and a block whose only
// predecessor edge is an unconditional `br` is spliced onto it with its
// arguments replaced by the branch operands. Blocks with two predecessors
// or a `cond_br` predecessor stay.

use pliron::builtin::ops::ModuleOp;
use pliron::context::{Context, Ptr};
use pliron::irfmt::parsers::spaced;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::parsable::parse_from_str;
use pliron::printable::Printable;
use pliron_ir_opt::cleanup::{Pure, clean_fn};
use pliron_ir_opt::state::State;
use pliron_llvm::ops::FuncOp;

const SRC: &str = r#"
builtin.module @m {
^block_0_0():
  llvm.func @f: llvm.func <builtin.integer i32(builtin.integer i32, builtin.integer i1) variadic = false> [] {
  ^entry_block_1_0(x: builtin.integer i32, c: builtin.integer i1):
    one = llvm.constant <builtin.integer <1: i32>> : builtin.integer i32;
    a = llvm.add x, one <{nsw=false,nuw=false}>: builtin.integer i32;
    dead = llvm.sub x, one <{nsw=false,nuw=false}>: builtin.integer i32;
    deader = llvm.mul dead, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.br ^block_2_0(a)
  ^block_2_0(y: builtin.integer i32):
    b = llvm.add y, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.br ^block_3_0()
  ^block_3_0():
    llvm.cond_br if c ^block_4_0() else ^block_5_0()
  ^block_4_0():
    d = llvm.add b, a <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.br ^block_5_0()
  ^block_5_0():
    g = llvm.add b, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.return g
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

// r[verify iropt.cleanup]
#[test]
fn dead_ops_go_and_single_br_pred_blocks_merge() {
    let ctx = &mut Context::new();
    let m = parse_from_str(spaced(Operation::top_level_parser()), ctx, SRC).expect("parse");
    let m = Operation::get_op::<ModuleOp>(m, ctx).unwrap().get_operation();
    let f = func(ctx, m);
    assert_eq!(pliron_ir_opt::inline::blocks(ctx, f).len(), 5);
    let mut st = State::default();
    let c = clean_fn(ctx, &mut st, &Pure::new(), f);
    // `deader` is unused; erasing it leaves `dead` unused too. `d` is
    // unused as well.
    assert_eq!(c.ops, 3, "dead ops erased");
    // block_2 (one `br` pred, an argument) and block_3 (one `br` pred)
    // join the entry; block_4 has a `cond_br` pred and block_5 two preds.
    assert_eq!(c.blocks, 2, "blocks merged");
    let bs = pliron_ir_opt::inline::blocks(ctx, f);
    assert_eq!(bs.len(), 3);
    let text = f.disp(ctx).to_string();
    assert!(!text.contains("llvm.sub") && !text.contains("llvm.mul"), "dead ops survived:\n{text}");
    // `y` was replaced by `a`: the entry now computes b = a + one directly.
    let entry_ops = pliron_ir_opt::inline::ops(ctx, bs[0]);
    assert_eq!(entry_ops.len(), 4, "constant, a, b, cond_br:\n{text}");
    assert!(Operation::is_op::<pliron_llvm::ops::CondBrOp>(entry_ops[3], ctx));
    // Nothing left refers to a block argument of the merged block.
    assert!(!text.contains("^block_2_0") && !text.contains("^block_3_0"), "{text}");
    // Cleaning again changes nothing.
    let c2 = clean_fn(ctx, &mut st, &Pure::new(), f);
    assert_eq!((c2.ops, c2.blocks), (0, 0));
}

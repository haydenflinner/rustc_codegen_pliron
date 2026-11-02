// `inline_cost::spec_cost` (`PLIRON_INLINE_SPEC`): a callee's size as
// specialized to one callsite. Constant actuals fold the ops they feed and
// the branches they decide, and blocks only reachable through a folded
// branch don't count. Code-free ops (constants, `br`) never count.

use pliron::builtin::ops::ModuleOp;
use pliron::context::{Context, Ptr};
use pliron::irfmt::parsers::spaced;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::parsable::parse_from_str;
use pliron::value::Value;
use pliron_ir_opt::inline_cost::{SpecCost, spec_cost};
use pliron_ir_opt::state::{ConstVal, State};
use pliron_llvm::ops::FuncOp;

const SRC: &str = r#"
builtin.module @m {
^block_0_0():
  llvm.func @f: llvm.func <builtin.integer i32(builtin.integer i32, builtin.integer i1) variadic = false> [] {
  ^entry_block_1_0(x: builtin.integer i32, c: builtin.integer i1):
    one = llvm.constant <builtin.integer <1: i32>> : builtin.integer i32;
    k0 = llvm.constant <builtin.integer <0: i1>> : builtin.integer i1;
    k1 = llvm.constant <builtin.integer <1: i1>> : builtin.integer i1;
    k3 = llvm.constant <builtin.integer <3: i32>> : builtin.integer i32;
    llvm.cond_br if c ^block_2_0() else ^block_3_0()
  ^block_2_0():
    a1 = llvm.add x, one <{nsw=false,nuw=false}>: builtin.integer i32;
    a2 = llvm.mul a1, a1 <{nsw=false,nuw=false}>: builtin.integer i32;
    a3 = llvm.xor a2, x : builtin.integer i32;
    a4 = llvm.sub a3, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.return a4
  ^block_3_0():
    llvm.return x
  }
}
"#;

fn funcs(ctx: &Context, m: Ptr<Operation>) -> Vec<Ptr<Operation>> {
    pliron_ir_opt::inline::blocks(ctx, m)
        .into_iter()
        .flat_map(|b| pliron_ir_opt::inline::ops(ctx, b))
        .filter(|&op| Operation::is_op::<FuncOp>(op, ctx))
        .collect()
}

// r[verify iropt.inline.spec]
#[test]
fn constant_actuals_shrink_the_callee() {
    let ctx = &mut Context::new();
    let m = parse_from_str(spaced(Operation::top_level_parser()), ctx, SRC).expect("parse");
    let m = Operation::get_op::<ModuleOp>(m, ctx).unwrap().get_operation();
    let f = funcs(ctx, m)[0];
    // Unused constants in f's entry stand in for a caller's constant actuals.
    let fops = pliron_ir_opt::inline::ops(ctx, pliron_ir_opt::inline::blocks(ctx, f)[0]);
    let res = |i: usize| -> Value { fops[i].deref(ctx).get_result(ctx, 0) };
    let (k0, k1, k3) = (res(1), res(2), res(3));
    let fparams: Vec<Value> = pliron_ir_opt::inline::blocks(ctx, f)[0].deref(ctx).arguments(ctx).collect();
    let mut st = State::default();
    st.consts.insert(k0, ConstVal::Bits(0));
    st.consts.insert(k1, ConstVal::Bits(1));
    st.consts.insert(k3, ConstVal::Bits(3));
    // f's own `one`.
    st.consts.insert(res(0), ConstVal::Bits(1));

    let (x, c) = (fparams[0], fparams[1]);
    // Nothing known: the cond_br, four ops and two returns.
    assert_eq!(spec_cost(ctx, &st, f, &[x, c]), SpecCost { n: 7, ncalls: 0 });
    // `c` false: only `return x` is left.
    assert_eq!(spec_cost(ctx, &st, f, &[x, k0]), SpecCost { n: 1, ncalls: 0 });
    // `c` true: the arithmetic arm and its return.
    assert_eq!(spec_cost(ctx, &st, f, &[x, k1]), SpecCost { n: 5, ncalls: 0 });
    // `c` true and `x` = 3: the arm folds to a constant.
    assert_eq!(spec_cost(ctx, &st, f, &[k3, k1]), SpecCost { n: 1, ncalls: 0 });
}

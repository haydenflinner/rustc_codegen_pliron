// `inline::erase_body` frees whole blocks, which deallocates their ops
// while still linked. The ops' entries in the dense link table keep the
// old `next`/`prev`; a new op reusing the slot used to inherit the field
// a one-field update (`prev` only) didn't set, and list insertion then
// hit `next.get_prev(ctx).unwrap()` on a stale link (PLIRON_DEADFREE on
// regex-syntax). Parse into the same Context afterwards: the second
// module's ops reuse the freed slots and must come out intact.

use pliron::builtin::ops::ModuleOp;
use pliron::context::{Context, Ptr};
use pliron::irfmt::parsers::spaced;
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::parsable::parse_from_str;
use pliron_llvm::ops::FuncOp;

const BIG: &str = r#"
builtin.module @m {
^block_0_0():
  llvm.func @f: llvm.func <builtin.integer i32(builtin.integer i32, builtin.integer i1) variadic = false> [] {
  ^entry_block_1_0(x: builtin.integer i32, c: builtin.integer i1):
    one = llvm.constant <builtin.integer <1: i32>> : builtin.integer i32;
    a = llvm.add x, one <{nsw=false,nuw=false}>: builtin.integer i32;
    b = llvm.add a, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.cond_br if c ^block_2_0() else ^block_3_0()
  ^block_2_0():
    d = llvm.add b, a <{nsw=false,nuw=false}>: builtin.integer i32;
    e = llvm.add d, b <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.br ^block_3_0()
  ^block_3_0():
    g = llvm.add b, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.return g
  }
}
"#;

const SMALL: &str = r#"
builtin.module @n {
^block_0_0():
  llvm.func @h: llvm.func <builtin.integer i32(builtin.integer i32) variadic = false> [] {
  ^entry_block_1_0(x: builtin.integer i32):
    one = llvm.constant <builtin.integer <1: i32>> : builtin.integer i32;
    a = llvm.add x, one <{nsw=false,nuw=false}>: builtin.integer i32;
    b = llvm.add a, one <{nsw=false,nuw=false}>: builtin.integer i32;
    c = llvm.add b, one <{nsw=false,nuw=false}>: builtin.integer i32;
    llvm.return c
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

// r[verify iropt.inline.dead]
#[test]
fn freed_body_slots_are_reused_cleanly() {
    let ctx = &mut Context::new();
    let m1 = parse_from_str(spaced(Operation::top_level_parser()), ctx, BIG).expect("parse big");
    let m1 = Operation::get_op::<ModuleOp>(m1, ctx).unwrap().get_operation();
    let f = funcs(ctx, m1)[0];
    let freed = pliron_ir_opt::inline::erase_body(ctx, f);
    assert_eq!(freed, 9, "erase_body should free every op of @f");
    assert!(pliron_ir_opt::inline::blocks(ctx, f).is_empty());
    // New ops land in the freed slots through ordinary list insertion.
    let m2 = parse_from_str(spaced(Operation::top_level_parser()), ctx, SMALL).expect("parse small");
    let m2 = Operation::get_op::<ModuleOp>(m2, ctx).unwrap().get_operation();
    let h = funcs(ctx, m2)[0];
    let bs = pliron_ir_opt::inline::blocks(ctx, h);
    assert_eq!(bs.len(), 1);
    let ops = pliron_ir_opt::inline::ops(ctx, bs[0]);
    assert_eq!(ops.len(), 5, "reused slots corrupted @h's op list");
    // Walking backwards must agree with walking forwards.
    let mut back = Vec::new();
    let mut cur = bs[0].deref(ctx).get_tail();
    while let Some(o) = cur {
        back.push(o);
        cur = o.get_prev(ctx);
    }
    back.reverse();
    assert_eq!(back, ops);
}

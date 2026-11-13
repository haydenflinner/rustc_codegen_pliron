//! Egraph elaboration does not move a pure instruction into a loop the
//! incoming function had it outside of (`PLIRON_ELABKEEP`, default on;
//! vendored Cranelift, `Elaborator::keep_out_of_loop`).
//!
//! The elaborator places a value at its first use and hoists it as far
//! as its loop stack reaches, but pops a loop's entry as soon as the
//! domtree preorder visits a block outside the loop. When the exit
//! comes before the body among the header's children, an invariant the
//! pipeline's LICM had put in the preheader came back into the body.
//! `PLIRON_ELABLOOPS=1` hoists every invariant instead, also those the
//! pipeline left on a conditional path of the loop on purpose; this
//! knob only keeps the pipeline's own hoists, and of those the ones
//! with an operand the loop does not otherwise use (the hoisted result
//! then takes that operand's register instead of adding one).
//!
//! Read once per process: this binary runs with the default (on).
//! `PLIRON_ELABKEEP=0 cargo test --test elabkeep` fails
//! `hoisted_value_stays_out`.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{Block, Function, InstructionData, Opcode};
use cranelift_codegen::settings::{self, Configurable};

fn optimized(clif: &str) -> Function {
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.optimize(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.func
}

/// An outer loop (block1) around a byte scan (header block3, body
/// block4) whose exit test compares against `v12 + v13`, both params of
/// the scan's preheader block2. With this order of the header's
/// successors the domtree preorder visits the exit (block5) before the
/// body (block4) and pops the loop. `end` is where the incoming function
/// computes the sum.
fn scan(end_in_preheader: bool) -> String {
    scan_with(end_in_preheader, "v17")
}

/// `sum` is what the body adds up: `v17` (the byte), or an expression
/// that reads the sum's operands inside the loop.
fn scan_with(end_in_preheader: bool, sum: &str) -> String {
    let end = "    v23 = iadd v12, v13\n";
    let (pre, body) = if end_in_preheader { (end, "") } else { ("", end) };
    let extra = if sum == "v17" { "" } else { "    v29 = imul v12, v13\n    v30 = iadd v29, v17\n" };
    format!(
        "
function %f(i64, i64, i64) -> i64 apple_aarch64 {{
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    jump block1(v3, v3)

block1(v4: i64, v5: i64):
    v6 = iconst.i64 1
    v7 = iadd v4, v6
    v8 = imul v4, v1
    v9 = iadd v0, v8
    v10 = load.i64 v9
    v11 = load.i64 v9+8
    v28 = icmp ult v10, v11
    brif v28, block2(v10, v11), block8

block8:
    jump block2(v11, v10)

block2(v12: i64, v13: i64):
{pre}    brif v13, block3(v12, v3), block6(v3)

block3(v14: i64, v15: i64):
    v16 = load.i8 v14
    v17 = uextend.i64 v16
    v18 = iconst.i64 10
    v19 = icmp ult v17, v18
    brif v19, block4, block5

block4:
    v20 = iconst.i64 1
    v21 = iadd v14, v20
{extra}    v22 = iadd v15, {sum}
{body}    v24 = icmp ne v21, v23
    brif v24, block3(v21, v22), block6(v22)

block5:
    jump block6(v15)

block6(v25: i64):
    v26 = iadd v5, v25
    v27 = icmp ult v7, v2
    brif v27, block1(v7, v26), block7

block7:
    return v5
}}"
    )
}

/// The block holding the `iadd` of block2's two params.
fn end_add_block(f: &Function) -> Option<Block> {
    let params = f.dfg.block_params(Block::new(2)).to_vec();
    assert_eq!(params.len(), 2, "{}", f.display());
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if let InstructionData::Binary { opcode: Opcode::Iadd, args } = f.dfg.insts[i] {
                let args = args.map(|a| f.dfg.resolve_aliases(a));
                if params.contains(&args[0]) && params.contains(&args[1]) && args[0] != args[1] {
                    return Some(b);
                }
            }
        }
    }
    None
}

#[test]
fn hoisted_value_stays_out() {
    let f = optimized(&scan(true));
    assert_eq!(end_add_block(&f), Some(Block::new(2)), "{}", f.display());
}

/// A hoisted value whose operands the loop reads anyway takes a register
/// more when it stays out: it goes to its use as before.
#[test]
fn hoisted_value_with_operands_used_in_the_loop_sinks() {
    let f = optimized(&scan_with(true, "v30"));
    assert_eq!(end_add_block(&f), Some(Block::new(4)), "{}", f.display());
}

/// What the pipeline left in the loop is not hoisted by this rule (the
/// stack's entry for the scan loop is gone by the time block4 is
/// visited, as before).
#[test]
fn in_loop_value_stays_in() {
    let f = optimized(&scan(false));
    assert_eq!(end_add_block(&f), Some(Block::new(4)), "{}", f.display());
}

//! Egraph elaboration LICM under `PLIRON_ELABLOOPS=1` (vendored Cranelift
//! patch 66): a loop-invariant `iadd` is placed in the inner loop's hoist
//! block no matter in which order the domtree preorder visits the header's
//! children. Without the knob the elaborator pops a loop's stack entry as
//! soon as it visits a block outside the loop (the exit), and when that
//! exit comes before the body among the header's domtree children, the
//! body's invariants stay in the body.
//!
//! The knob is read once per process (`OnceLock`), so this binary sets it
//! before its first compile and every test here runs with it on.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::{Block, Function, InstructionData, Opcode};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};

fn isa() -> std::sync::Arc<dyn TargetIsa> {
    // SAFETY: every test sets the same value before any compile reads
    // it; nothing else in this binary reads the env.
    unsafe {
        std::env::set_var("PLIRON_ELABLOOPS", "1");
    }
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(b);
    cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(flags)
        .unwrap()
}

/// Run the mid-end (egraph + elaboration) and return the function.
fn optimized(clif: &str) -> Function {
    let isa = isa();
    let f: Function = cranelift_reader::parse_functions(clif).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.optimize(&*isa, &mut ControlPlane::default()).unwrap();
    ctx.func
}

/// parse_only's shape: an outer loop over words; per word a byte-scan
/// inner loop (header block3, body block4) whose exit test is
/// `p + 1 != base + len`, with `base + len` depending only on the inner
/// loop's preheader params (block2). `exit_first` lists the exit block
/// before the body in the header's `brif`, which puts the body after the
/// exit in the domtree children order.
fn scan(exit_first: bool) -> String {
    let (a, b) = if exit_first {
        ("block5", "block4")
    } else {
        ("block4", "block5")
    };
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
    brif v13, block3(v12, v3), block6(v3)

block3(v14: i64, v15: i64):
    v16 = load.i8 v14
    v17 = uextend.i64 v16
    v18 = iconst.i64 10
    v19 = icmp ult v17, v18
    brif v19, {a}, {b}

block4:
    v20 = iconst.i64 1
    v21 = iadd v14, v20
    v22 = iadd v15, v17
    v23 = iadd v12, v13
    v24 = icmp ne v21, v23
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

/// The inner loop's preheader: block numbers survive the mid-end.
fn pre(_f: &Function) -> Block {
    use cranelift_codegen::entity::EntityRef;
    Block::new(2)
}

/// The block holding the `iadd` of block2's two params, if any.
fn end_add_block(f: &Function) -> Option<Block> {
    let params = f.dfg.block_params(pre(f)).to_vec();
    assert_eq!(params.len(), 2, "{}", f.display());
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if let InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } = f.dfg.insts[i]
            {
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
fn end_hoisted_exit_first() {
    let f = optimized(&scan(true));
    assert_eq!(end_add_block(&f), Some(pre(&f)), "{}", f.display());
}

#[test]
fn end_hoisted_body_first() {
    let f = optimized(&scan(false));
    assert_eq!(end_add_block(&f), Some(pre(&f)), "{}", f.display());
}

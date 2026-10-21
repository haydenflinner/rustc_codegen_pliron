//! ivretro: literal `B + arg_iv*K` backedge args (`imul` + `iadd`)
//! become `p + s*K` once the latch carries `arg_iv = iv + s` —
//! differentially checked against the interpreter.

use cranelift_codegen::ir::{Function, InstructionData, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// The inlined `Big32x40` borrow-subtract zip loop: stepped pointer
/// params whose backedge still pays `imul + iadd` per element.
const ZIP: &str = "
function %f(i32, i32, i32) -> i32 {
block0(v0: i32, v1: i32, v2: i32):
    v3 = iconst.i32 0
    v18 = iconst.i32 7
    v19 = band v0, v18
    jump block1(v3, v1, v2)

block1(v4: i32, v5: i32, v6: i32):
    v7 = load.i32 notrap aligned v5
    v8 = load.i32 notrap aligned v6
    v9 = iadd v7, v8
    store.i32 notrap aligned v9, v5
    v10 = iconst.i32 1
    v11 = iadd v4, v10
    v12 = icmp ult v11, v19
    brif v12, block2, block3(v9)

block2:
    v13 = iconst.i32 4
    v14 = imul v11, v13
    v15 = iadd v14, v1
    v16 = iadd v14, v2
    jump block1(v11, v15, v16)

block3(v17: i32):
    return v17
}
";

/// No edge arg on a `block1` backedge may still be an `imul`/`iadd`
/// literal — they must forward `iadd p, K` on the param.
fn literal_args(f: &Function) -> usize {
    let mut n = 0;
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            for bc in f
                .dfg
                .insts[i]
                .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                if bc.block(&f.dfg.value_lists).as_u32() != 1 {
                    continue;
                }
                for a in bc.args(&f.dfg.value_lists) {
                    let cranelift_codegen::ir::BlockArg::Value(v) = a else {
                        continue;
                    };
                    if let cranelift_codegen::ir::ValueDef::Result(di, _) =
                        f.dfg.value_def(f.dfg.resolve_aliases(v))
                        && matches!(
                            f.dfg.insts[di],
                            InstructionData::Binary {
                                opcode: Opcode::Imul,
                                ..
                            }
                        )
                    {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

#[test]
fn backedge_affine_args_become_param_steps() {
    let before = parse(ZIP);
    let mut after = before.clone();
    let k = pliron_clif_opt::ivretro::run(&mut after, &mut Analysis::default());
    assert_eq!(k, 2, "expected two ptr retrofits:\n{}", after.display());
    assert_eq!(
        literal_args(&after),
        0,
        "imul still feeds a backedge arg:\n{}",
        after.display()
    );
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}

/// An entry edge that breaks `p = B + iv*K` (`v3` instead of the base)
/// invalidates the induction — `p` is not `B + iv*K` on iteration 0.
#[test]
fn skips_when_entry_breaks_invariant() {
    let src = "
function %f(i32, i32, i32) -> i32 {
block0(v0: i32, v1: i32, v2: i32):
    v3 = iconst.i32 0
    jump block1(v3, v3, v3)

block1(v4: i32, v5: i32, v6: i32):
    v10 = iconst.i32 1
    v11 = iadd v4, v10
    v12 = icmp ult v11, v0
    brif v12, block2, block3(v5)

block2:
    v13 = iconst.i32 4
    v14 = imul v11, v13
    v15 = iadd v14, v1
    v16 = iadd v14, v2
    jump block1(v11, v15, v16)

block3(v17: i32):
    return v17
}
";
    let mut after = parse(src);
    let k = pliron_clif_opt::ivretro::run(&mut after, &mut Analysis::default());
    assert_eq!(
        k,
        0,
        "rewrote despite broken entry invariant:\n{}",
        after.display()
    );
}

/// `arg_iv` not of the form `iv + s` (a second use of `iv` itself is
/// fine — `s = 0` forwards `p`) — but a non-step arg leaves no hook.
#[test]
fn skips_non_step_iv_arg() {
    let src = "
function %f(i32, i32, i32) -> i32 {
block0(v0: i32, v1: i32, v2: i32):
    v3 = iconst.i32 0
    jump block1(v3, v1, v2)

block1(v4: i32, v5: i32, v6: i32):
    v10 = iconst.i32 1
    v11 = iadd v4, v10
    v12 = icmp ult v11, v0
    brif v12, block2, block3(v5)

block2:
    v13 = iconst.i32 4
    v14 = imul v0, v13
    v15 = iadd v14, v1
    v16 = iadd v14, v2
    jump block1(v0, v15, v16)

block3(v17: i32):
    return v17
}
";
    let mut after = parse(src);
    // `arg_iv = v0` is not `iv + s` — p's args do match the invariant
    // (`B + v0*K`) but no stepped edge exists to rewrite.
    let k = pliron_clif_opt::ivretro::run(&mut after, &mut Analysis::default());
    assert_eq!(k, 0, "rewrote without a stepped edge:\n{}", after.display());
}

//! `clifpeep::ldnarrow` (`PLIRON_LDNARROW=1`): an integer load whose every
//! use is an `ireduce` becomes a load of the widest reduced type at the
//! same address. Shapes: the enum-tag read (`ireduce.i8` of a `load.i32`,
//! then `uextend` and a compare), a load reduced to two widths (narrows
//! to the wider), a load with one use that is not a reduce (stays wide),
//! and a trapping load (stays wide). The slot holds a different value in
//! every byte, so a load at the wrong offset or of the wrong width shows
//! in the checker.

use cranelift_codegen::ir::{Function, InstructionData, Opcode, types};
use pliron_clif_opt::clifpeep;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// Types of the plain loads left in `f`, in layout order.
fn load_types(f: &Function) -> Vec<String> {
    let mut out = Vec::new();
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            if let InstructionData::Load { opcode: Opcode::Load, .. } = f.dfg.insts[i] {
                out.push(f.dfg.value_type(f.dfg.first_result(i)).to_string());
            }
        }
    }
    out
}

fn run(src: &str) -> (Function, usize) {
    let before = parse(src);
    let mut after = before.clone();
    let n = clifpeep::ldnarrow(&mut after);
    check(&before, &after, &Facts::default(), &Config { trials: 64, ..Config::default() })
        .unwrap_or_else(|m| panic!("ldnarrow changed behaviour: {m:?}\n{after}"));
    (after, n)
}

const PRELUDE: &str = "
    ss0 = explicit_slot 16, align = 8
    block0(v0: i64, v1: i32):
        v5 = stack_addr.i64 ss0
        v2 = iconst.i64 0x0807060504030201
        v3 = iadd v2, v0
        store notrap aligned v3, v5
        v4 = iconst.i64 0x100f0e0d0c0b0a09
        store notrap aligned v4, v5+8
";

#[test]
fn tag_read_loads_the_byte() {
    let src = format!(
        "function %f(i64, i32) -> i32 {{{PRELUDE}
        v6 = load.i32 notrap aligned v5
        v7 = ireduce.i8 v6
        v8 = uextend.i32 v7
        v12 = iconst.i32 2
        v9 = icmp eq v8, v12
        v10 = uextend.i32 v9
        v11 = iadd v8, v10
        return v11
    }}"
    );
    let (after, n) = run(&src);
    assert_eq!(n, 1);
    assert_eq!(load_types(&after), vec!["i8"]);
}

#[test]
fn two_widths_narrow_to_the_wider() {
    let src = format!(
        "function %f(i64, i32) -> i32 {{{PRELUDE}
        v6 = load.i64 notrap aligned v5+8
        v7 = ireduce.i8 v6
        v8 = ireduce.i16 v6
        v9 = uextend.i32 v7
        v10 = uextend.i32 v8
        v11 = iadd v9, v10
        return v11
    }}"
    );
    let (after, n) = run(&src);
    assert_eq!(n, 1);
    assert_eq!(load_types(&after), vec!["i16"]);
}

#[test]
fn wide_use_keeps_the_load() {
    let src = format!(
        "function %f(i64, i32) -> i64 {{{PRELUDE}
        v6 = load.i64 notrap aligned v5
        v7 = ireduce.i8 v6
        v8 = uextend.i64 v7
        v9 = iadd v6, v8
        return v9
    }}"
    );
    let (after, n) = run(&src);
    assert_eq!(n, 0);
    assert_eq!(load_types(&after), vec!["i64"]);
}

#[test]
fn trapping_load_stays() {
    let src = format!(
        "function %f(i64, i32) -> i32 {{{PRELUDE}
        v6 = load.i32 aligned v5+4
        v7 = ireduce.i8 v6
        v8 = uextend.i32 v7
        return v8
    }}"
    );
    let (after, n) = run(&src);
    assert_eq!(n, 0);
    assert_eq!(load_types(&after), vec!["i32"]);
    let _ = types::I8;
}

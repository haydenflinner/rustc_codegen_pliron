//! `bool_mask`'s operand pair must be alias-resolved: an `icmp` whose
//! args are spelled through aliases (`icmp ne v_x, v_alias`) feeds
//! `sel_cmp`'s pair comparisons raw-arg-vs-resolved — the pair match
//! then misses and the whole `Ordering`-decode collapse
//! (`icmp eq (lt ? -1 : ne), 1` → `ugt`) bails. Regressed on the
//! unrolled `binary_search` chain in `unicode_data::skip_search` where
//! loadfwd spelled one icmp's arg as an alias.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Function, InstBuilder, InstructionData, Opcode, types};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

#[test]
fn selord_pair_through_alias() {
    // `icmp eq (lt ? -1 : ne), 1` — binary_search's `cmp == Greater`
    // — collapses to `icmp ugt` on the pair.
    let mut f = parse(
        "function u0:0(i32, i32) -> i32 fast {
block0(v0: i32, v1: i32):
    v2 = icmp ult v0, v1
    v3 = icmp ne v0, v1
    v4 = iconst.i8 -1
    v5 = select v2, v4, v3
    v6 = iconst.i8 1
    v7 = icmp eq v5, v6
    v8 = iconst.i32 0
    v9 = iconst.i32 27
    v10 = select v7, v8, v9
    return v10
}",
    );
    let v1 = f.dfg.block_params(f.layout.entry_block().unwrap())[1];
    // Detach a scratch value and alias it to `v1` — then re-spell the
    // `icmp ne`'s operand through the alias, the way forwarding passes
    // leave args written.
    let b0 = f.layout.entry_block().unwrap();
    let v_alias = FuncCursor::new(&mut f)
        .at_bottom(b0)
        .ins()
        .iconst(types::I32, 0);
    let tmp = f.dfg.value_def(v_alias).inst().unwrap();
    f.dfg.clear_results(tmp);
    f.layout.remove_inst(tmp);
    f.dfg.change_to_alias(v_alias, v1);
    // The pair only mismatches when the *cond*'s `bool_mask` pair
    // carries the alias spelling too — rewrite both icmps' `v1` args.
    for i in f.layout.block_insts(b0).collect::<Vec<_>>() {
        if !matches!(
            f.dfg.insts[i],
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                ..
            }
        ) {
            continue;
        }
        let args: Vec<_> = f
            .dfg
            .inst_values(i)
            .map(|v| if v == v1 { v_alias } else { v })
            .collect();
        f.dfg.overwrite_inst_values(i, args.into_iter());
    }
    let ne = f
        .layout
        .block_insts(b0)
        .find(|&i| {
            matches!(
                f.dfg.insts[i],
                InstructionData::IntCompare {
                    opcode: Opcode::Icmp,
                    cond: cranelift_codegen::ir::condcodes::IntCC::NotEqual,
                    ..
                }
            )
        })
        .unwrap();
    assert_eq!(
        f.dfg.resolve_aliases(
            match f.dfg.insts[ne] {
                InstructionData::IntCompare { args, .. } => args[1],
                _ => unreachable!(),
            }
        ),
        v1
    );
    pliron_clif_opt::clifpeep::iconst128(&mut f);
    let text = f.display().to_string();
    assert!(
        text.contains("icmp ugt v0, v1"),
        "Ordering decode must collapse through the alias-spelled pair:\n{text}"
    );
    assert!(!text.contains("icmp eq"), "residual compare:\n{text}");
}

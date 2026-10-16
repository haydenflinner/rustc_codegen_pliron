//! splices() redirects a decided pred's edge J -> S using the pred's
//! captured edge args. A pred that is itself a spliced successor had
//! those args remapped onto fresh params by `map_inst_values`, so the
//! snapshot emits a raw value whose def dies when J is swept. Found by
//! PLIRON_VERIFY on rustc_mir_transform's `SmallVec::extend_one`
//! (v1325, a block15 param, emitted on the block31 -> block32 edge
//! after block15 lost all preds).

use cranelift_codegen::ir::Function;
use cranelift_codegen::ir::{BlockArg, ValueDef};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

/// Every edge arg must resolve to a def that still exists.
fn assert_no_dangling_args(f: &Function) {
    for b in f.layout.blocks().collect::<Vec<_>>() {
        for i in f.layout.block_insts(b) {
            for bc in f.dfg.insts[i].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                for a in bc.args(&f.dfg.value_lists) {
                    let BlockArg::Value(v) = a else { continue };
                    let v = f.dfg.resolve_aliases(v);
                    let ok = match f.dfg.value_def(v) {
                        ValueDef::Param(db, _) => f.layout.is_block_inserted(db),
                        ValueDef::Result(di, _) => f.layout.inst_block(di).is_some(),
                        _ => false,
                    };
                    assert!(
                        ok,
                        "edge arg {v} on inst {i} resolves to a def whose block is gone\n{f:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn spliced_pred_jargs_replay_remap() {
    // J (block15) is a param join with a decidable cond and three preds:
    // an entry edge and two back-edges that pass J's own params back.
    // All three are decided, so splices redirects every one and J is
    // swept. The back-edge preds are also the decided successors (block31
    // decides block32, block32 decides block31), so in either sset
    // iteration order one redirect replays jargs whose raw values were
    // remapped by the pred's own splice — the stale arg dangles with J.
    let before = parse(
        "function %f(i64) -> i64 {
        block0(v0: i64):
            v1 = iconst.i8 1
            v2 = iconst.i64 100
            v3 = iconst.i64 3
            v4 = iconst.i64 7
            jump block15(v1, v0, v2, v3, v4)

        block15(v10: i8, v11: i64, v12: i64, v13: i64, v14: i64):
            brif v10, block31(v14, v11), block32(v13)

        block31(v30: i64, v31: i64):
            v33 = iadd v31, v14
            v34 = icmp ult v33, v12
            v35 = iconst.i8 0
            brif v34, block15(v35, v33, v12, v13, v14), block40(v33)

        block32(v40: i64):
            v41 = iadd v40, v13
            v42 = icmp ult v41, v12
            v43 = iconst.i8 1
            brif v42, block15(v43, v41, v12, v13, v14), block40(v41)

        block40(v70: i64):
            return v70
        }",
    );
    let mut after = before.clone();
    let j = after.layout.blocks().nth(1).unwrap();
    pliron_clif_opt::edgespec::run(&mut after);
    // The splice ran to completion: all of J's preds were redirected and
    // the join block itself is gone.
    assert!(
        !after.layout.is_block_inserted(j),
        "expected join block to be swept\n{after:?}"
    );
    assert_no_dangling_args(&after);
    let cfg = Config {
        trials: 128,
        ..Config::default()
    };
    match check(&before, &after, &Facts::default(), &cfg) {
        Ok(s) => assert!(s.compared > 0, "{s:?}"),
        Err(m) => panic!("{m}\n---- after\n{}", after.display()),
    }
}

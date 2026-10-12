//! Small `br_table` → compare-chain conversion. Cranelift lowers every
//! `br_table` to an indirect branch (`adr; ldrsw; add; br xN`) plus a
//! bounds test — roughly eight instructions and an indirect-branch
//! prediction on every execution — where LLVM turns small or sparse
//! switches into ordinary conditional branches. Iterator state machines
//! (`Option`/`match` discriminants in `chars()`/`split_whitespace()`-style
//! loops) hit this in the per-character path: the dispatch alone was ~25%
//! of the hot loop body and the `br` relies on indirect prediction.
//!
//! For at most `MAX_ENTRIES` live table entries the chain is shorter,
//! predictable, and semantically exact: `br_table` sends index `v >= n`
//! to the default block, and `icmp eq v, k` matches no index past the
//! last test, so the final `brif`'s else edge reproduces the default.
//! Entries identical to the default (same block, same args) are skipped —
//! matching them would be equivalent to taking the default.
//!
//! Table indices are tested in order; duplicate target blocks with
//! different args stay distinct tests since each edge keeps its own
//! arguments.
//!
//! `PLIRON_BRCHAIN=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, BlockArg, BlockCall, Function, InstBuilder, InstructionData};

/// Maximum number of live (non-default-equivalent) table entries that
/// get a compare chain. Above that the indirect branch wins on code
/// size: the chain costs `icmp`+`brif` per entry vs ~8 instructions
/// total for the table dispatch.
const MAX_ENTRIES: usize = 4;

/// Whether `a` and `b` name the same block with the same arguments —
/// then an explicit test for that index is redundant with the default.
fn same_target(func: &Function, a: BlockCall, b: BlockCall) -> bool {
    a.block(&func.dfg.value_lists) == b.block(&func.dfg.value_lists)
        && a.args(&func.dfg.value_lists).eq(b.args(&func.dfg.value_lists))
}

fn dest(func: &Function, bc: BlockCall) -> (Block, Vec<BlockArg>) {
    (
        bc.block(&func.dfg.value_lists),
        bc.args(&func.dfg.value_lists).collect(),
    )
}

pub fn run(func: &mut Function) -> usize {
    let mut todo = Vec::new();
    for b in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[term] else {
            continue;
        };
        let jt = &func.dfg.jump_tables[table];
        let dflt = jt.default_block();
        // Live entries: drop ones equal to the default — those indices
        // fold into the default edge.
        let mut live = Vec::new();
        for (i, &entry) in jt.as_slice().iter().enumerate() {
            if !same_target(func, entry, dflt) {
                live.push((i as u64, dest(func, entry)));
            }
        }
        if live.is_empty() {
            // Every index lands on the default: plain jump.
            todo.push((b, term, arg, live, dest(func, dflt)));
            continue;
        }
        if live.len() > MAX_ENTRIES {
            continue;
        }
        todo.push((b, term, arg, live, dest(func, dflt)));
    }
    let n = todo.len();
    for (b, term, arg, live, (db, dargs)) in todo {
        let ty = func.dfg.value_type(arg);
        func.layout.remove_inst(term);
        let mut cur = b;
        for (pos, (index, (eb, eargs))) in live.iter().enumerate() {
            // Last live test's else edge is the default; earlier tests
            // chain through a fresh compare block.
            let next = if pos + 1 == live.len() {
                None
            } else {
                let nb = func.dfg.make_block();
                func.layout.insert_block_after(nb, cur);
                Some(nb)
            };
            {
                let mut pos_c = FuncCursor::new(func).at_bottom(cur);
                let k = pos_c.ins().iconst(ty, *index as i64);
                let c = pos_c.ins().icmp(IntCC::Equal, arg, k);
                match next {
                    Some(nb) => pos_c.ins().brif(c, *eb, eargs, nb, &[]),
                    None => pos_c.ins().brif(c, *eb, eargs, db, &dargs),
                };
            }
            if let Some(nb) = next {
                cur = nb;
            }
        }
        if live.is_empty() {
            let mut pos_c = FuncCursor::new(func).at_bottom(b);
            pos_c.ins().jump(db, &dargs);
        }
    }
    n
}

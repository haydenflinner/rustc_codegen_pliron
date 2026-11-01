//! Branch-argument selects on the branch's own condition
//! (`PLIRON_BRIFSEL`, `=0` disables).
//!
//! `brif c, T(.., select c, a, b, ..), E(.., select c, a, b, ..)`: on the
//! edge to `T` the select is `a`, on the edge to `E` it is `b`. The
//! `TrustedRandomAccess` zip leaves exactly this in its loop header after
//! if-conversion: `index' = select (index <u len), index + 1, index` (and
//! the element pointers likewise) passed to the body on `brif (index <u
//! len)`. Seeing `index + 1` there makes the index an affine induction
//! variable for the IV passes. The select stays for its other users (DCE
//! removes it when the branch was the last).

use cranelift_codegen::ir::{BlockArg, Function, InstructionData, Opcode, Value};

/// `v` is `select c, a, b` on exactly `c`: `Some((a, b))`.
fn sel_on(func: &Function, v: Value, c: Value) -> Option<(Value, Value)> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Ternary { opcode: Opcode::Select, args }
            if func.dfg.resolve_aliases(args[0]) == c =>
        {
            Some((args[1], args[2]))
        }
        _ => None,
    }
}

pub fn run(func: &mut Function) -> usize {
    // Uses per value: a select with users beyond the branch stays live, and
    // rewriting the edge would keep its arms alive next to it (pulldown's
    // parser lost 20% that way).
    let mut uses: rustc_hash::FxHashMap<Value, u32> = rustc_hash::FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *uses.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    let mut n = 0;
    let blocks: Vec<_> = func.layout.blocks().collect();
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let InstructionData::Brif { arg, blocks: dests, .. } = func.dfg.insts[t] else { continue };
        let c = func.dfg.resolve_aliases(arg);
        // Uses of each value by this branch alone.
        let mut here: rustc_hash::FxHashMap<Value, u32> = rustc_hash::FxHashMap::default();
        for v in func.dfg.inst_values(t) {
            *here.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
        }
        for (k, bc) in dests.iter().enumerate() {
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            let mut new = args.clone();
            let mut hit = false;
            for (slot, a) in new.iter_mut().enumerate() {
                let BlockArg::Value(v) = args[slot] else { continue };
                let v = func.dfg.resolve_aliases(v);
                if uses.get(&v) != here.get(&v) {
                    continue;
                }
                if let Some((x, y)) = sel_on(func, v, c) {
                    *a = BlockArg::Value(if k == 0 { x } else { y });
                    hit = true;
                }
            }
            if !hit || !pliron_cfg::bisect("brifsel") {
                continue;
            }
            let blk = bc.block(&func.dfg.value_lists);
            let dfg = &mut func.dfg;
            let d = &mut dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[k];
            *d = cranelift_codegen::ir::BlockCall::new(blk, new, &mut dfg.value_lists);
            n += 1;
        }
    }
    n
}

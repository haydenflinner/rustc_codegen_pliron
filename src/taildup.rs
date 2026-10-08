//! Duplicate small return blocks into their `jump` predecessors (LLVM's
//! tail duplication of return blocks). Helpers like `derive(PartialEq)`
//! end every arm with `jump ret(v)`; copying the `return` into each arm
//! removes the jump and the block-parameter move.

use cranelift_codegen::ir::{Block, BlockArg, Function, InstructionData, Value};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 4;

fn candidate(func: &Function, b: Block) -> bool {
    let Some(t) = func.layout.last_inst(b) else {
        return false;
    };
    if !func.dfg.insts[t].opcode().is_return() {
        return false;
    }
    let mut n = 0;
    for i in func.layout.block_insts(b) {
        if i == t {
            continue;
        }
        let op = func.dfg.insts[i].opcode();
        if op.is_call()
            || op.can_store()
            || op.can_load()
            || op.can_trap()
            || op.other_side_effects()
            || op.is_branch()
            || op.is_terminator()
        {
            return false;
        }
        n += 1;
    }
    n <= MAX_INSTS
}

/// Returns the number of jumps replaced by a copy of the return block.
pub fn run(func: &mut Function) -> usize {
    let entry = func.layout.entry_block();
    let rets: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| Some(b) != entry && candidate(func, b))
        .collect();
    if rets.is_empty() {
        return 0;
    }
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for p in blocks {
        let Some(t) = func.layout.last_inst(p) else {
            continue;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else {
            continue;
        };
        let r = destination.block(&func.dfg.value_lists);
        if r == p || !rets.contains(&r) {
            continue;
        }
        let mut map: FxHashMap<Value, Value> = FxHashMap::default();
        let args: Vec<BlockArg> = destination.args(&func.dfg.value_lists).collect();
        let mut ok = true;
        for (&prm, a) in func.dfg.block_params(r).iter().zip(&args) {
            match a {
                BlockArg::Value(v) => {
                    map.insert(prm, *v);
                }
                _ => ok = false,
            }
        }
        if !ok {
            continue;
        }
        let body: Vec<_> = func.layout.block_insts(r).collect();
        for i in body {
            let ni = func.dfg.clone_inst(i);
            let mut data = func.dfg.insts[ni];
            let dfg = &mut func.dfg;
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |v| *map.get(&v).unwrap_or(&v),
            );
            func.dfg.insts[ni] = data;
            for (&o, &nv) in func
                .dfg
                .inst_results(i)
                .iter()
                .zip(func.dfg.inst_results(ni))
            {
                map.insert(o, nv);
            }
            func.layout.insert_inst(ni, t);
        }
        func.layout.remove_inst(t);
        n += 1;
    }
    n
}

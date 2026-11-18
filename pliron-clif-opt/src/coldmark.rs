//! `coldmark` (`PLIRON_COLDMARK=1`, opt-in): mark cold every block from
//! which no path reaches a function exit — the code between a failed check
//! and its `call panic_*; trap`, including loops on that path (formatting
//! a panic message, drop glue before an abort). The lowering marks only the
//! trap blocks themselves, so the duplicating passes that skip cold code
//! (`punroll`'s unroll and peel, `taildup`, `looprot` under
//! `PLIRON_LOOPROT_COLD`, `jumpthread`'s targets with this knob) still
//! copied these regions, and block placement kept them inline.
//!
//! A block reaches an exit when some path leads to a terminator with no
//! successors that is not a `trap` (`return`, a tail call). Under emulated
//! EH a panic returns, so nothing on its path is marked (conservative). A
//! function with no exit at all (`-> !`, a server loop) marks nothing.
//! Linear: one predecessor build, one backward walk, one forward walk.

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{Block, Function, Opcode};

/// Blocks reachable from the entry from which no path reaches an exit.
pub fn doomed(func: &Function) -> Vec<Block> {
    let Some(entry) = func.layout.entry_block() else {
        return Vec::new();
    };
    let n = func.dfg.num_blocks();
    let mut preds: Vec<Vec<Block>> = vec![Vec::new(); n];
    let mut live = vec![false; n];
    let mut stack: Vec<Block> = Vec::new();
    for b in func.layout.blocks() {
        let mut any = false;
        for s in func.block_successors(b) {
            preds[s.index()].push(b);
            any = true;
        }
        let exits = match func.layout.last_inst(b) {
            Some(t) => !any && func.dfg.insts[t].opcode() != Opcode::Trap,
            None => true,
        };
        if exits {
            live[b.index()] = true;
            stack.push(b);
        }
    }
    if stack.is_empty() {
        return Vec::new();
    }
    while let Some(b) = stack.pop() {
        for &p in &preds[b.index()] {
            if !live[p.index()] {
                live[p.index()] = true;
                stack.push(p);
            }
        }
    }
    let mut seen = vec![false; n];
    let mut out = Vec::new();
    seen[entry.index()] = true;
    stack.push(entry);
    while let Some(b) = stack.pop() {
        if !live[b.index()] {
            out.push(b);
        }
        for s in func.block_successors(b) {
            if !seen[s.index()] {
                seen[s.index()] = true;
                stack.push(s);
            }
        }
    }
    out
}

/// Mark every [`doomed`] block cold; returns how many were not already.
pub fn run(func: &mut Function) -> usize {
    let mut k = 0;
    for b in doomed(func) {
        if !func.layout.is_cold(b) {
            func.layout.set_cold(b);
            k += 1;
        }
    }
    k
}

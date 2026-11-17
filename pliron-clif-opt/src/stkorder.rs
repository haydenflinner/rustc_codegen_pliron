//! Wasm operand-stack order (`PLIRON_WASM_STKORDER=1`, wasm only).
//!
//! The wasm emitter keeps a value used more than once in a local and
//! computes a single-use value inline under its consumer, in the CLIF
//! order of the block. `wpeep` turns `local.set x; local.get x` into
//! `local.tee x` only when the value's first use comes right after it.
//! SipHash's rounds (`v0 += v1; v1 = rotl(v1, 13); v1 ^= v0`) are written
//! the other way: the sum (used twice) is computed first, then the
//! rotate, then `bxor rot, sum` reads the sum back from its local
//! (`hmget`'s lookup loop: 37 `local.set` and 98 `local.get` against
//! LLVM's 6 and 64; LLVM's `WebAssemblyRegStackify` reorders and
//! commutes for exactly this).
//!
//! Two rewrites, per block:
//! - a pure arithmetic value with more than one use moves to just before
//!   its first use in the block (its operands are defined earlier, and a
//!   pure op can pass any instruction), so nothing separates the value
//!   from that use;
//! - when that first use is a commutative binary op and the value is its
//!   second operand, the operands swap, so the value is the first thing
//!   the op pops: `sum; local.tee x; <rotate>; bxor`.
//!
//! Each instruction moves at most once, last first, so a value whose
//! first user moves follows it; the first use is looked up in the current
//! order, at most `SCAN` instructions on (a value used further away
//! stays).

use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Opcode, Value};
use rustc_hash::FxHashMap;

/// How far a value's first use is looked for (instructions).
const SCAN: usize = 64;

fn movable(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Iadd
            | Opcode::Isub
            | Opcode::Imul
            | Opcode::Band
            | Opcode::Bor
            | Opcode::Bxor
            | Opcode::Bnot
            | Opcode::Rotl
            | Opcode::Rotr
            | Opcode::Ishl
            | Opcode::Ushr
            | Opcode::Sshr
            | Opcode::Uextend
            | Opcode::Sextend
            | Opcode::Ireduce
    )
}

fn commutative(op: Opcode) -> bool {
    matches!(op, Opcode::Iadd | Opcode::Imul | Opcode::Band | Opcode::Bor | Opcode::Bxor)
}

/// Reorders every block; returns the instructions moved or commuted.
pub fn run(func: &mut Function) -> usize {
    // Use counts over the whole function.
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *uses.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        // Last first: a value whose first user itself moves follows that
        // user to its new place.
        for &i in insts.iter().rev() {
            let op = func.dfg.insts[i].opcode();
            if !movable(op) || func.dfg.inst_results(i).len() != 1 {
                continue;
            }
            let v = func.dfg.first_result(i);
            if uses.get(&v).copied().unwrap_or(0) < 2 {
                continue;
            }
            // The first user in the current order (a user moved by an
            // earlier step of this walk may now follow other users), at
            // most `SCAN` instructions on.
            let mut cur = func.layout.next_inst(i);
            let mut u = None;
            for _ in 0..SCAN {
                let Some(c) = cur else { break };
                if func.dfg.inst_values(c).any(|x| func.dfg.resolve_aliases(x) == v) {
                    u = Some(c);
                    break;
                }
                cur = func.layout.next_inst(c);
            }
            let Some(u) = u else { continue };
            if func.layout.next_inst(i) != Some(u) {
                func.layout.remove_inst(i);
                func.layout.insert_inst(i, u);
                n += 1;
            }
            if let InstructionData::Binary { opcode, args } = func.dfg.insts[u]
                && commutative(opcode)
                && func.dfg.resolve_aliases(args[1]) == v
                && func.dfg.resolve_aliases(args[0]) != v
            {
                func.dfg.insts[u] = InstructionData::Binary { opcode, args: [args[1], args[0]] };
                n += 1;
            }
        }
    }
    n
}

//! Edge args a deciding branch already fixes (`PLIRON_EQEDGE`, opt-in).
//!
//! `brif (icmp eq(v, K)), T(..., K, ...)` — on the taken edge `v == K`,
//! so a block arg that re-materializes the constant can pass `v`
//! instead: a coalescable vreg copy (often nothing after regalloc) where
//! the constant forces a `mov rd, #K`. Unrolled counted loops produce
//! this shape: every arm's exit passes that arm's position as a constant
//! while the same constant already sits in the register the `b.eq`
//! compares (encoding_rs `utf8_valid_up_to`'s ASCII arms — `mov x30, #K;
//! cmp w12, #K; b.eq` per unrolled position).
//!
//! The `ne` mirror: `icmp ne`'s not-taken edge is the equality edge.
//! Only constants substitute — an `eq(v, w)` edge could pass either vreg
//! for the other, but which survives register allocation better is not
//! decidable locally.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, Value};

fn iconst(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

pub fn run(func: &mut Function) -> usize {
    let blocks: Vec<cranelift_codegen::ir::Block> = func.layout.blocks().collect();
    let mut edits: Vec<(Inst, usize, BlockCall)> = vec![];
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg: c, blocks, .. } = func.dfg.insts[t] else {
            continue;
        };
        let c = func.dfg.resolve_aliases(c);
        let Some(ci) = func.dfg.value_def(c).inst() else {
            continue;
        };
        let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[ci] else {
            continue;
        };
        // The edge on which `v == K` is decided: `eq` taken, `ne` not.
        let di = match cond {
            IntCC::Equal => 0,
            IntCC::NotEqual => 1,
            _ => continue,
        };
        let (v, k) = match (iconst(func, args[0]), iconst(func, args[1])) {
            (None, Some(k)) => (func.dfg.resolve_aliases(args[0]), k),
            (Some(k), None) => (func.dfg.resolve_aliases(args[1]), k),
            _ => continue,
        };
        let bc = blocks[di];
        let dst = bc.block(&func.dfg.value_lists);
        let mut changed = false;
        let new_args: Vec<BlockArg> = bc
            .args(&func.dfg.value_lists)
            .map(|a| {
                let BlockArg::Value(av) = a else { return a };
                if av != v
                    && iconst(func, av) == Some(k)
                    && func.dfg.value_type(av) == func.dfg.value_type(v)
                {
                    changed = true;
                    BlockArg::Value(v)
                } else {
                    a
                }
            })
            .collect();
        if changed {
            edits.push((t, di, BlockCall::new(dst, new_args, &mut func.dfg.value_lists)));
        }
    }
    let n = edits.len();
    let dfg = &mut func.dfg;
    for (t, di, nbc) in edits {
        dfg.insts[t]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] = nbc;
    }
    n
}

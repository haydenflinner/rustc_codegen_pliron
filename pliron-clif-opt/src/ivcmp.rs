//! Offset exit tests (`PLIRON_IVCMP=1`).
//!
//! `icmp eq|ne (iadd x, a), b` in a loop, with `a` and `b` defined outside
//! it, becomes `icmp eq|ne x, (b - a)`, the subtraction emitted once at the
//! end of the header's immediate dominator (constants count as invariant
//! wherever they sit, and are re-emitted there). Equality is offset-invariant
//! in wrapping arithmetic; ordered compares are not and never move.
//!
//! The shape is a zip's second end test after IV strength reduction:
//! num-bigint-dig's `add_mul_vvw` (`z.iter_mut().zip(x)`) walks one
//! pointer `p` and tests `p < x_end` and `p + (z - x) == z_end` every
//! limb: two `add`s, a `cmp` and a `b.eq` per iteration where LLVM has a
//! single counter. `zipiv` handles two lockstep header params; here the
//! second pointer is already `p + delta`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, Function, InstBuilder, InstructionData, Opcode, Value, ValueDef};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

pub fn run(func: &mut Function, dt: &DominatorTree, la: &LoopAnalysis) -> usize {
    let mut n = 0;
    let loops: Vec<Loop> = la.loops().collect();
    for lp in loops {
        let h = la.loop_header(lp);
        let Some(pre) = dt.idom(h) else { continue };
        if la.is_in_loop(pre, lp) {
            continue;
        }
        let Some(pre_term) = func.layout.last_inst(pre) else {
            continue;
        };
        let iconst = |func: &Function, v: Value| match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
            ValueDef::Result(d, _) => match func.dfg.insts[d] {
                InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
                _ => None,
            },
            _ => None,
        };
        // Defined outside the loop, or a constant (re-emitted where needed).
        let inv = |func: &Function, v: Value| {
            iconst(func, v).is_some() || def_block(func, v).is_some_and(|b| !la.is_in_loop(b, lp))
        };
        let blocks: Vec<Block> = func.layout.blocks().filter(|&b| la.is_in_loop(b, lp)).collect();
        for b in blocks {
            for i in func.layout.block_insts(b).collect::<Vec<_>>() {
                let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] else {
                    continue;
                };
                if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
                    continue;
                }
                let mut hit = None;
                for (s, other) in [(args[0], args[1]), (args[1], args[0])] {
                    if !inv(func, other) {
                        continue;
                    }
                    let s = func.dfg.resolve_aliases(s);
                    let ValueDef::Result(d, _) = func.dfg.value_def(s) else {
                        continue;
                    };
                    let InstructionData::Binary { opcode: Opcode::Iadd, args: ia } = func.dfg.insts[d] else {
                        continue;
                    };
                    for (x, a) in [(ia[0], ia[1]), (ia[1], ia[0])] {
                        if inv(func, a) && !inv(func, x) {
                            hit = Some((x, a, other));
                            break;
                        }
                    }
                    if hit.is_some() {
                        break;
                    }
                }
                let Some((x, a, other)) = hit else { continue };
                let ty = func.dfg.value_type(other);
                // No i128 `iconst`; the shape is pointer/index width anyway.
                if ty.bits() > 64 {
                    continue;
                }
                let mut cur = FuncCursor::new(func).at_inst(pre_term);
                let here = |v: Value, cur: &mut FuncCursor| match iconst(cur.func, v) {
                    Some(k) if def_block(cur.func, v).is_none_or(|b| la.is_in_loop(b, lp)) => cur.ins().iconst(ty, k),
                    _ => v,
                };
                let (o, a) = (here(other, &mut cur), here(a, &mut cur));
                let t = cur.ins().isub(o, a);
                if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[i] {
                    *args = [x, t];
                }
                n += 1;
            }
        }
    }
    n
}

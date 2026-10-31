//! Offset exit tests (`PLIRON_IVCMP=0` disables).
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
//! second pointer is already `p + delta`. Only when the sum's other uses
//! are memory addresses (otherwise the add stays and the move adds work).

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
    // Non-address uses per value. The rewrite pays when the sum's other
    // uses are load/store addresses (they fold `p + d` into a reg+reg
    // amode for free; num-bigint-dig's `z` pointer). When the sum is also
    // the loop's carried pointer (parse_only's post-incremented digit
    // pointer goes to the back edge), the add stays and the moved test
    // only adds work. So: the compare must be the one non-address use.
    let mut nonaddr: rustc_hash::FxHashMap<Value, u32> = rustc_hash::FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let addr = match func.dfg.insts[i] {
                InstructionData::Load { arg, .. } => Some(arg),
                InstructionData::Store { args, .. } => Some(args[1]),
                _ => None,
            };
            let vals: Vec<Value> = func.dfg.inst_values(i).collect();
            for (k, &v) in vals.iter().enumerate() {
                let is_addr = addr == Some(v) && !(matches!(func.dfg.insts[i], InstructionData::Store { .. }) && k == 0);
                if !is_addr {
                    *nonaddr.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
                }
            }
        }
    }
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
                    if nonaddr.get(&s).copied().unwrap_or(0) != 1 {
                        continue;
                    }
                    for (x, a) in [(ia[0], ia[1]), (ia[1], ia[0])] {
                        // `x` must be an induction variable (a loop block
                        // param): moving the test onto a derived value
                        // (`-p`, `p + k`) cascades and hides the plain
                        // pointer compare later passes form (parse_only).
                        let x_iv = matches!(func.dfg.value_def(func.dfg.resolve_aliases(x)),
                            ValueDef::Param(pb, _) if la.is_in_loop(pb, lp));
                        if x_iv && inv(func, a) {
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

//! One cheap instruction out of a branch arm (`PLIRON_ARMHOIST`, wasm).
//!
//! The egraph's elaboration places a pure value in the only arm that
//! uses it. For an arm that then holds just that one instruction and a
//! jump to the join,
//!
//! ```text
//! A:  brif c, M, B          A:  v = iadd p, 1
//! B:  v = iadd p, 1             brif c, M, B
//!     jump J(v)             B:  jump J(v)
//! ```
//!
//! the placement costs more than it saves once the code reaches an
//! engine: a block that only passes values is a branch straight to the
//! join, a block with an instruction is a block and a jump of its own.
//! tokenize's ASCII arm was `next = p + 1`: with the add in the arm V8
//! emitted `tbnz; add; b join` per character, with it ahead of the test
//! `tbz join` (about 3% of the kernel). This is LLVM SimplifyCFG's
//! speculation of one cheap instruction, kept to the case where the arm
//! becomes move-only.
//!
//! An arm qualifies when the `brif` edge is its only way in, it takes
//! no params, it ends in a `jump`, it is not cold, it is in the same
//! innermost loop as the branch (nothing is moved into a loop), and
//! apart from `iconst`s it holds one instruction from the list in
//! [`cheap`]: integer, no trap, no memory. The instructions move ahead
//! of the `brif`; nothing else changes. One pass over the blocks.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Opcode};
use cranelift_codegen::loop_analysis::LoopAnalysis;

/// Speculating `i` costs at most one ALU operation and cannot trap.
fn cheap(func: &Function, i: Inst) -> bool {
    let ok_ty = func
        .dfg
        .inst_results(i)
        .iter()
        .all(|&r| func.dfg.value_type(r).is_int() && func.dfg.value_type(r).bits() <= 64);
    ok_ty
        && matches!(
            func.dfg.insts[i].opcode(),
            Opcode::Iadd
                | Opcode::Isub
                | Opcode::Band
                | Opcode::Bor
                | Opcode::Bxor
                | Opcode::Ishl
                | Opcode::Ushr
                | Opcode::Sshr
                | Opcode::Ineg
                | Opcode::Bnot
                | Opcode::Uextend
                | Opcode::Sextend
                | Opcode::Ireduce
                | Opcode::Icmp
        )
}

fn is_iconst(func: &Function, i: Inst) -> bool {
    matches!(func.dfg.insts[i], InstructionData::UnaryImm { opcode: Opcode::Iconst, .. })
}

/// Returns the number of arms emptied.
pub fn run(func: &mut Function, cfg: &ControlFlowGraph, la: &LoopAnalysis) -> usize {
    // (branch block, arm, the arm's non-terminator instructions)
    let mut todo: Vec<(Block, Vec<Inst>)> = Vec::new();
    for a in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(a) else { continue };
        let InstructionData::Brif { blocks, .. } = func.dfg.insts[t] else { continue };
        let (tb, eb) = (
            blocks[0].block(&func.dfg.value_lists),
            blocks[1].block(&func.dfg.value_lists),
        );
        if tb == eb {
            continue;
        }
        for b in [tb, eb] {
            if b == a
                || !func.dfg.block_params(b).is_empty()
                || func.layout.is_cold(b)
                || la.innermost_loop(b) != la.innermost_loop(a)
            {
                continue;
            }
            let mut preds = cfg.pred_iter(b);
            let (Some(p), None) = (preds.next(), preds.next()) else { continue };
            if p.inst != t {
                continue;
            }
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            let Some((&last, body)) = insts.split_last() else { continue };
            if !matches!(func.dfg.insts[last], InstructionData::Jump { .. }) {
                continue;
            }
            let work: Vec<Inst> = body.iter().copied().filter(|&i| !is_iconst(func, i)).collect();
            if work.len() != 1 || !cheap(func, work[0]) {
                continue;
            }
            todo.push((a, body.to_vec()));
        }
    }
    let n = todo.len();
    for (a, body) in todo {
        let t = func.layout.last_inst(a).unwrap();
        for i in body {
            func.layout.remove_inst(i);
            func.layout.insert_inst(i, t);
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::dominator_tree::DominatorTree;

    fn go(src: &str) -> (Function, usize) {
        let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
        let cfg = ControlFlowGraph::with_function(&f);
        let dt = DominatorTree::with_function(&f, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(&f, &cfg, &dt);
        let n = run(&mut f, &cfg, &la);
        (f, n)
    }

    fn insts_in(f: &Function, n: usize) -> usize {
        let b = f.layout.blocks().nth(n).unwrap();
        f.layout.block_insts(b).count()
    }

    /// The ASCII arm of a decode: `next = p + 1`, then the join.
    #[test]
    fn single_add_arm_becomes_move_only() {
        let (f, n) = go("function u0:0(i32, i32) -> i32 fast {
block0(v0: i32, v1: i32):
    brif v1, block2, block1

block1:
    v2 = iconst.i32 1
    v3 = iadd v0, v2
    jump block3(v3)

block2:
    v4 = iconst.i32 4
    v5 = iadd v0, v4
    v6 = imul v5, v5
    jump block3(v6)

block3(v7: i32):
    return v7
}");
        assert_eq!(n, 1);
        // block1 is just its jump; block0 got the const and the add.
        assert_eq!(insts_in(&f, 1), 1);
        assert_eq!(insts_in(&f, 0), 3);
        // block2 has two real instructions: left alone.
        assert_eq!(insts_in(&f, 2), 4);
        cranelift_codegen::verifier::verify_function(
            &f,
            &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
        )
        .unwrap();
    }

    /// A load can trap and reads memory: it stays in its arm.
    #[test]
    fn load_arm_stays() {
        let (_, n) = go("function u0:0(i32, i32) -> i32 fast {
block0(v0: i32, v1: i32):
    brif v1, block2, block1

block1:
    v3 = load.i32 v0
    jump block3(v3)

block2:
    jump block3(v0)

block3(v7: i32):
    return v7
}");
        assert_eq!(n, 0);
    }

    /// The arm leaves the loop: hoisting would put the add on every trip.
    #[test]
    fn exit_arm_is_not_pulled_into_the_loop() {
        let (_, n) = go("function u0:0(i32, i32) -> i32 fast {
block0(v0: i32, v1: i32):
    jump block1(v0)

block1(v2: i32):
    v3 = iconst.i32 1
    v4 = isub v2, v3
    v5 = icmp eq v4, v1
    brif v5, block2, block1(v4)

block2:
    v6 = iconst.i32 7
    v8 = iadd v4, v6
    jump block3(v8)

block3(v7: i32):
    return v7
}");
        assert_eq!(n, 0);
    }
}

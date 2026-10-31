//! Latch speculation (`PLIRON_LATCHSPEC=0` disables): a loop latch that
//! only computes a few pure values and jumps to the header is folded into
//! its single predecessor's `brif`.
//!
//! ```text
//! P:  c = icmp ...                  P:  c = icmp ...
//!     brif c, L, E                      v = iadd p, 1
//! L:  v = iadd p, 1             =>      w = isub v, b
//!     w = isub v, b                     brif c, H(v, w), E
//!     jump H(v, w)
//! ```
//!
//! Laid out, the left form takes two branches per iteration: `brif` jumps
//! over the exit edge block to `L`, and `L` jumps back to `H`.
//! pulldown-cmark's escape scan (`escape_html_scalar`) ran like that, 7
//! instructions and two taken branches against LLVM's bottom-tested
//! `cmn; b.ne`. The right form takes one. The exit path executes the
//! speculated instructions once; they are pure and cannot trap. This is
//! the piece of LLVM's loop rotation plus SimplifyCFG speculation that the
//! block layout cannot recover by itself.
//!
//! Conditions: `L` has one predecessor `P` (so `L`'s values are used only
//! in `L` and its jump: `L` dominates no other block), at most
//! `MAX_INSTS` non-terminator instructions, all pure and non-trapping,
//! and its jump targets a loop header whose loop holds `P` (a back edge).

use cranelift_codegen::ir::{BlockArg, BlockCall, Function, InstructionData, Opcode, Value};

use crate::pipeline::{Analysis, CfgDtLa};

/// Instructions a latch may hold (beyond its jump).
const MAX_INSTS: usize = 3;

fn pure(func: &Function, i: cranelift_codegen::ir::Inst) -> bool {
    let op = func.dfg.insts[i].opcode();
    !op.can_trap()
        && !op.can_load()
        && !op.can_store()
        && !op.is_call()
        && !op.is_branch()
        && !op.other_side_effects()
        && !matches!(op, Opcode::Udiv | Opcode::Sdiv | Opcode::Urem | Opcode::Srem)
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let mut n = 0;
    loop {
        let CfgDtLa { cfg, la, .. } = an.cfg_dt_la(func);
        let mut job = None;
        'find: for p in func.layout.blocks() {
            let Some(t) = func.layout.last_inst(p) else { continue };
            let InstructionData::Brif { blocks, .. } = func.dfg.insts[t] else { continue };
            for (side, bc) in blocks.iter().enumerate() {
                let l = bc.block(&func.dfg.value_lists);
                if l == p || cfg.pred_iter(l).count() != 1 || !func.dfg.block_params(l).is_empty() {
                    continue;
                }
                let insts: Vec<_> = func.layout.block_insts(l).collect();
                let (&j, body) = insts.split_last().unwrap();
                let InstructionData::Jump { destination, .. } = func.dfg.insts[j] else { continue };
                let h = destination.block(&func.dfg.value_lists);
                let Some(lp) = la.innermost_loop(h) else { continue };
                if la.loop_header(lp) != h || !la.is_in_loop(p, lp) || h == l {
                    continue;
                }
                let real = body.iter().filter(|&&i| func.dfg.insts[i].opcode() != Opcode::Iconst).count();
                if real > MAX_INSTS || !body.iter().all(|&i| pure(func, i)) {
                    continue;
                }
                if !destination.args(&func.dfg.value_lists).all(|a| matches!(a, BlockArg::Value(_))) {
                    continue;
                }
                job = Some((p, t, side, l, j, body.to_vec()));
                break 'find;
            }
        }
        let Some((_p, t, side, l, j, body)) = job else { break };
        if !pliron_cfg::bisect("latchspec") {
            break;
        }
        for i in body {
            func.layout.remove_inst(i);
            func.layout.insert_inst(i, t);
        }
        let InstructionData::Jump { destination, .. } = func.dfg.insts[j] else { unreachable!() };
        let h = destination.block(&func.dfg.value_lists);
        let args: Vec<Value> = destination
            .args(&func.dfg.value_lists)
            .map(|a| match a {
                BlockArg::Value(v) => v,
                _ => unreachable!(),
            })
            .collect();
        func.layout.remove_inst(j);
        func.layout.remove_block(l);
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[side];
        *bc = BlockCall::new(h, args.iter().map(|&v| BlockArg::Value(v)), &mut dfg.value_lists);
        an.invalidate(func);
        n += 1;
    }
    n
}

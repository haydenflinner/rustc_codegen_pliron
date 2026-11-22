//! Constants on exception edges move into a landing block of their own
//! (`PLIRON_EXNCONST=1`, opt-in).
//!
//! MIR cleanup paths pass drop flags and discriminants to a shared cleanup
//! block: lowering emits `try_call f, ret, [tag0: pad(exn0)]` with `pad:
//! jump cleanup(exn, 1)`, and `edgefwd` bypasses the forwarder, leaving
//! `[tag0: cleanup(exn0, v)]` with `v = iconst 1` on the call itself. An
//! exception edge's arguments are uses at the call, so every such constant
//! is live across every call that names it: in a loop of calls each takes
//! a callee-saved register (lol_html's `exec_without_attrs` held 0, 1 and
//! 1 in `x21`/`x24`/`x27` around an indirect call and reloaded `self`
//! from the stack on every trip).
//!
//! Rule: for every exception target of a `try_call`/`try_call_indirect`
//! (not the normal return) that passes an `iconst`, the edge goes to a new
//! cold block taking the other arguments as parameters, which defines the
//! constants afresh and jumps on. One extra `b` on the unwind path.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Block, BlockArg, BlockCall, Function, InstBuilder, InstructionData, Opcode, Value};

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

pub fn run(func: &mut Function) -> usize {
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let Some(&last) = blocks.last() else { return 0 };
    let mut tail = last;
    let mut n = 0;
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else { continue };
        if !matches!(func.dfg.insts[t].opcode(), Opcode::TryCall | Opcode::TryCallIndirect) {
            continue;
        }
        let ndest = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables).len();
        // The normal return is the last target.
        for di in 0..ndest.saturating_sub(1) {
            let bc = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[di];
            let target = bc.block(&func.dfg.value_lists);
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            let consts: Vec<Option<i64>> = args
                .iter()
                .map(|a| match a {
                    BlockArg::Value(v) => iconst(func, *v),
                    _ => None,
                })
                .collect();
            if consts.iter().all(|c| c.is_none()) {
                continue;
            }
            if !pliron_cfg::bisect("exnconst") {
                continue;
            }
            let tparams: Vec<Value> = func.dfg.block_params(target).to_vec();
            let nb = func.dfg.make_block();
            func.layout.insert_block_after(nb, tail);
            func.layout.set_cold(nb);
            tail = nb;
            let mut edge: Vec<BlockArg> = Vec::new();
            let mut fwd: Vec<(usize, Option<Value>)> = Vec::new();
            for (k, a) in args.iter().enumerate() {
                if consts[k].is_some() {
                    fwd.push((k, None));
                } else {
                    let ty = func.dfg.value_type(tparams[k]);
                    let p = func.dfg.append_block_param(nb, ty);
                    edge.push(*a);
                    fwd.push((k, Some(p)));
                }
            }
            let mut pos = FuncCursor::new(func).at_bottom(nb);
            let mut jargs: Vec<BlockArg> = Vec::with_capacity(args.len());
            for (k, p) in fwd {
                let v = match p {
                    Some(p) => p,
                    None => {
                        let ty = pos.func.dfg.value_type(tparams[k]);
                        pos.ins().iconst(ty, consts[k].unwrap())
                    }
                };
                jargs.push(BlockArg::Value(v));
            }
            pos.ins().jump(target, &jargs);
            let nbc = BlockCall::new(nb, edge, &mut func.dfg.value_lists);
            let dfg = &mut func.dfg;
            dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] = nbc;
            n += 1;
        }
    }
    n
}

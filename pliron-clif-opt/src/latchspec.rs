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
//!
//! `PLIRON_LATCHSPEC_LIVE=1` (opt-in): also, no header parameter that
//! receives a speculated value may still be live into `P`'s other
//! successor (an argument of that edge, or a use reachable from it without
//! passing the header, where the parameter is redefined; a search over at
//! most `LIVE_BUDGET` blocks, past which it counts as live). Otherwise the old and the new value are
//! both live at the branch and cannot share a register: the back edge
//! needs a move per such parameter, and since the header has other
//! predecessors the lowering splits the edge again for those moves, so
//! the taken branch the fold saved comes back as well. brotli's
//! `is_mostly_utf8` ran `add x11, x7, #1; add x9, x10, #1; cbnz` and then
//! `mov x7, x11; mov x10, x9; b` on the split back edge (its exit path
//! still reads the old index); without the fold its latch is `add x7, x7,
//! #1; add x10, x10, #1; b`.

use cranelift_codegen::ir::{Block, BlockArg, BlockCall, Function, InstructionData, Opcode, Value, ValueDef};
use rustc_hash::FxHashMap;

use crate::pipeline::{Analysis, CfgDtLa};

/// Instructions a latch may hold (beyond its jump).
const MAX_INSTS: usize = 3;

/// Blocks the liveness search may visit per parameter.
const LIVE_BUDGET: usize = 256;

/// Is a block parameter of `h` (used in blocks `uses`) live into `from`:
/// some use reachable from `from` without passing `h`? `true` past the
/// budget.
fn live_from(
    func: &Function,
    cfg: &cranelift_codegen::flowgraph::ControlFlowGraph,
    from: Block,
    h: Block,
    uses: &[Block],
    seen: &mut rustc_hash::FxHashSet<Block>,
    stack: &mut Vec<Block>,
) -> bool {
    seen.clear();
    stack.clear();
    stack.push(from);
    let mut visited = 0usize;
    while let Some(b) = stack.pop() {
        if b == h || !seen.insert(b) {
            continue;
        }
        visited += 1;
        if visited > LIVE_BUDGET {
            return true;
        }
        if uses.contains(&b) {
            return true;
        }
        let _ = func;
        stack.extend(cfg.succ_iter(b));
    }
    false
}

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
    run_with(func, an, pliron_cfg::env_is("PLIRON_LATCHSPEC_LIVE", "1"))
}

/// [`run`] with the `PLIRON_LATCHSPEC_LIVE` rule chosen by the caller.
pub fn run_with(func: &mut Function, an: &mut Analysis, live_rule: bool) -> usize {
    // Blocks that use each block parameter (once: a fold only moves uses
    // from `L` into `P`, which the rule allows alike).
    let mut param_uses: FxHashMap<Value, Vec<Block>> = FxHashMap::default();
    if live_rule {
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    if matches!(func.dfg.value_def(v), ValueDef::Param(..)) {
                        let e = param_uses.entry(v).or_default();
                        if e.last() != Some(&b) {
                            e.push(b);
                        }
                    }
                }
            }
        }
    }
    let mut seen: rustc_hash::FxHashSet<Block> = Default::default();
    let mut stack: Vec<Block> = Vec::new();
    let mut n = 0;
    loop {
        let CfgDtLa { cfg, la, .. } = an.cfg_dt_la(func);
        let mut job = None;
        'find: for p in func.layout.blocks() {
            let Some(t) = func.layout.last_inst(p) else { continue };
            let InstructionData::Brif { blocks, .. } = func.dfg.insts[t] else { continue };
            'side: for (side, bc) in blocks.iter().enumerate() {
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
                if live_rule {
                    let other = blocks[1 - side];
                    let other_args: Vec<Value> = other
                        .args(&func.dfg.value_lists)
                        .filter_map(|a| match a {
                            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                            _ => None,
                        })
                        .collect();
                    let hparams = func.dfg.block_params(h);
                    for (k, a) in destination.args(&func.dfg.value_lists).enumerate() {
                        let BlockArg::Value(a) = a else { continue };
                        let a = func.dfg.resolve_aliases(a);
                        let speculated = matches!(func.dfg.value_def(a), ValueDef::Result(i, _) if body.contains(&i));
                        let Some(&hp) = hparams.get(k) else { continue };
                        if !speculated || a == hp {
                            continue;
                        }
                        let other_blk = other.block(&func.dfg.value_lists);
                        let live_past = other_args.contains(&hp)
                            || param_uses.get(&hp).is_some_and(|bs| {
                                live_from(func, cfg, other_blk, h, bs, &mut seen, &mut stack)
                            });
                        if live_past {
                            continue 'side;
                        }
                    }
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

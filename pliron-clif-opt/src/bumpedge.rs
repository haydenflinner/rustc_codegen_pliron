//! Step a cursor on the edges into the block that only steps it
//! (`PLIRON_BUMPEDGE=0` disables).
//!
//! A loop rotated past its first memory access reads the element at the
//! bottom and advances the cursor at the top:
//!
//! ```text
//! body(p, x):  q = iadd p, 8          body(q, x):  ...
//!              ...                                 brif c, test, more
//!              brif c, test, more     test:        brif (q != end), next, exit
//! test:        brif (q != end), next, exit
//! next:        y = load q             next:        y = load q
//!              brif ok, body(q, y), panic          q2 = iadd q, 8
//!                                                  brif ok, body(q2, y), panic
//! ```
//!
//! On the left `p` exists only to be stepped. Nothing in `body` reads
//! `q`, so Cranelift's elaborator computes `p + 8` again in each block
//! that does (`test`, `next`, the block after a call), keeps `p` live to
//! all of them, and the back edge ends in a register copy: regex's
//! `Fsm::next_state` state loop with one latch
//! (`tests/parity/prog_loop.rs`, `PLIRON_LOOPROT_MERGE`) ran fifteen
//! instructions for a skipped state against LLVM's twelve. On the right
//! the step sits next to the load of the same pointer in each
//! predecessor, where it is the load's post-index writeback
//! (`ldr x, [p], #8`), and the block carries one cursor.
//!
//! The rewrite: a block param `p` whose only use is one `iadd p, K` in
//! its own block, `K` a constant that fits a post-index immediate, and
//! every edge into the block a `jump`/`brif` passing a plain value. Each
//! edge passes `arg + K`, computed at the end of its source block; the
//! param is replaced by a fresh one (facts recorded about the old param,
//! such as non-null, are not known to hold one step on) and the `iadd`'s
//! result becomes an alias of it. Worth it only when some predecessor
//! reads or writes memory at its argument with no offset; a `brif` whose
//! other arm goes elsewhere pays the add on that path too, so that arm
//! must be cold unless the edge is a back edge (the block dominates its
//! source: the add then replaces one that ran every trip anyway).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{Block, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef};
use rustc_hash::FxHashMap;

use crate::pipeline::Analysis;

/// Most edges a stepped param may have: the add is paid once per edge
/// in code size.
const MAX_EDGES: usize = 4;

struct Plan {
    h: Block,
    p: Value,
    iadd: Inst,
    k: i64,
    /// (source block, terminator, the edge's argument for `p`).
    edges: Vec<(Block, Inst, Value)>,
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let ValueDef::Result(i, _) = func.dfg.value_def(func.dfg.resolve_aliases(v)) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// `a` is the address of a load or store in `b`, with no offset.
fn mem_base_in(func: &Function, b: Block, a: Value) -> bool {
    func.layout.block_insts(b).any(|i| {
        let (addr, off) = match func.dfg.insts[i] {
            InstructionData::Load { arg, offset, .. } => (arg, i32::from(offset)),
            InstructionData::Store { args, offset, .. } => (args[1], i32::from(offset)),
            _ => return false,
        };
        off == 0 && func.dfg.resolve_aliases(addr) == a
    })
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let uses = an.uses(func);
    let mut plans: Vec<Plan> = Vec::new();
    // Candidates first: most blocks have no param that is only stepped,
    // and the predecessor scan below is the costly part.
    let mut cands: FxHashMap<Block, Vec<(Value, Inst, i64)>> = FxHashMap::default();
    for h in func.layout.blocks() {
        if Some(h) == func.layout.entry_block() {
            continue;
        }
        for &p in func.dfg.block_params(h) {
            let ty = func.dfg.value_type(p);
            if !ty.is_int() || ty.bits() > 64 {
                continue;
            }
            let [(ub, ui)] = uses.sites(p) else { continue };
            if *ub != h {
                continue;
            }
            let InstructionData::Binary { opcode: Opcode::Iadd, args } = func.dfg.insts[*ui] else {
                continue;
            };
            let (a0, a1) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
            // `iadd p, p` is not a step.
            let k = match (a0 == p, a1 == p) {
                (true, false) => iconst(func, a1),
                (false, true) => iconst(func, a0),
                _ => None,
            };
            let Some(k) = k else { continue };
            if !(-256..=255).contains(&k) || k == 0 {
                continue;
            }
            cands.entry(h).or_default().push((p, *ui, k));
        }
    }
    if cands.is_empty() {
        return 0;
    }
    // Edges into the candidate blocks, from one walk over the terminators.
    let mut preds: FxHashMap<Block, Vec<(Block, Inst)>> = FxHashMap::default();
    let mut bad: Vec<Block> = Vec::new();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let plain = matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif);
        for bc in func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
            let d = bc.block(&func.dfg.value_lists);
            if !cands.contains_key(&d) {
                continue;
            }
            if !plain {
                bad.push(d);
            } else {
                let e = preds.entry(d).or_default();
                if !e.contains(&(b, t)) {
                    e.push((b, t));
                }
            }
        }
    }
    for d in bad {
        cands.remove(&d);
    }
    let mut hs: Vec<Block> = cands.keys().copied().collect();
    hs.sort_unstable();
    let crate::pipeline::CfgDtLa { dt, .. } = an.cfg_dt_la(func);
    for h in hs {
        let Some(ps) = preds.get(&h) else { continue };
        for &(p, iadd, k) in &cands[&h] {
            let pi = func.dfg.block_params(h).iter().position(|&x| x == p).unwrap();
            let mut edges: Vec<(Block, Inst, Value)> = Vec::new();
            let mut ok = true;
            let mut pays = false;
            'pred: for &(b, t) in ps {
                let dests = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                let mut arg: Option<Value> = None;
                for bc in dests {
                    let d = bc.block(&func.dfg.value_lists);
                    if d == h {
                        let Some(a) = bc.args(&func.dfg.value_lists).nth(pi).and_then(|a| a.as_value()) else {
                            ok = false;
                            break 'pred;
                        };
                        let a = func.dfg.resolve_aliases(a);
                        // Two edges from one block with different
                        // arguments: two adds for one branch.
                        if arg.is_some_and(|x| x != a) {
                            ok = false;
                            break 'pred;
                        }
                        arg = Some(a);
                    } else if !func.layout.is_cold(d) && !dt.block_dominates(h, b) {
                        // The add would also run on the way to `d`.
                        ok = false;
                        break 'pred;
                    }
                }
                let a = arg.unwrap();
                pays |= mem_base_in(func, b, a);
                edges.push((b, t, a));
            }
            if !ok || !pays || edges.is_empty() || edges.len() > MAX_EDGES {
                continue;
            }
            plans.push(Plan { h, p, iadd, k, edges });
        }
    }
    let n = plans.len();
    for pl in plans {
        let ty = func.dfg.value_type(pl.p);
        let q = func.dfg.first_result(pl.iadd);
        let pi = func.dfg.block_params(pl.h).iter().position(|&x| x == pl.p).unwrap();
        for &(b, t, a) in &pl.edges {
            let _ = b;
            // The param passed round unchanged: one step on, it is the
            // old `iadd`'s value. (The stepped value passed round, the
            // common case, needs nothing special: `q` is about to name
            // the new param.)
            let a2 = if a == pl.p {
                q
            } else {
                let mut pos = FuncCursor::new(func).at_inst(t);
                let kc = pos.ins().iconst(ty, pl.k);
                pos.ins().iadd(a, kc)
            };
            let dfg = &mut func.dfg;
            for bc in dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                if bc.block(&dfg.value_lists) != pl.h {
                    continue;
                }
                let mut ix = 0;
                bc.update_args(&mut dfg.value_lists, |arg| {
                    ix += 1;
                    if ix - 1 == pi { a2.into() } else { arg }
                });
            }
        }
        let p2 = func.dfg.replace_block_param(pl.p, ty);
        func.layout.remove_inst(pl.iadd);
        func.dfg.clear_results(pl.iadd);
        func.dfg.change_to_alias(q, p2);
    }
    if n > 0 {
        an.invalidate(func);
    }
    n
}

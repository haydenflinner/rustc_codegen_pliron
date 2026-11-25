//! Dead-end slot demotion (`PLIRON_PADSLOT`, opt-in): a value defined
//! in a block that cannot reach any return and used only in such
//! blocks — the exception object a landing pad holds across cleanup
//! calls is the canonical case — forces a callee-saved register
//! whenever its live range spans a `call`/`try_call`, so the prologue
//! and epilogue pay a `stp`/`ldp` pair on *every* path for a value
//! only a path that never returns ever touches (`drop_glue` thunks
//! keep a third callee-saved register for the exception object across
//! `bl dealloc`).
//!
//! Demoting the value at its definition — `stack_store` right where
//! it is defined, a `stack_load` at every use — drops the preg from
//! the frame's clobber set entirely: the slot traffic runs only on the
//! dead-end path, no register is ever needed, and unlike a callee-save
//! sink this needs no mid-function unwind row. LLVM gets the same
//! shape by keeping these thunks leaf or spilling the pad value
//! itself.
//!
//! A block qualifies when no CFG path from it reaches a `return` or
//! `return_call`, it is reachable from the entry, and it sits outside
//! every loop (a dead-end `loop {}` would pay the slot ops per
//! iteration). The value must additionally cross a call somewhere in
//! the dead region — without one, regalloc gives it a caller-saved
//! register and the slot ops are pure waste.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    Block, Function, Inst, InstBuilder, Opcode, StackSlotData, StackSlotKind, Type, Value,
    ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::pipeline::Analysis;

/// Per-function demotion budget — cold code does not need unbounded
/// stack traffic either.
const MAX_DEMOTIONS: usize = 32;

/// Blocks that can reach a `return`/`return_call` (CFG reverse
/// reachability; exception edges count — the pad a `try_call` can
/// throw into is dead-end only when it also cannot reach a return).
fn ret_reaching(
    func: &Function,
    cfg: &cranelift_codegen::flowgraph::ControlFlowGraph,
) -> FxHashSet<Block> {
    let mut reach = FxHashSet::default();
    let mut stack: Vec<Block> = Vec::new();
    for b in func.layout.blocks() {
        let t = func.layout.last_inst(b).unwrap();
        if func.dfg.insts[t].opcode().is_return() {
            reach.insert(b);
            stack.push(b);
        }
    }
    while let Some(b) = stack.pop() {
        for p in cfg.pred_iter(b) {
            if reach.insert(p.block) {
                stack.push(p.block);
            }
        }
    }
    reach
}

/// Blocks forward-reachable from the entry block.
fn reachable(
    func: &Function,
    cfg: &cranelift_codegen::flowgraph::ControlFlowGraph,
) -> FxHashSet<Block> {
    let entry = func.layout.entry_block().unwrap();
    let mut seen = FxHashSet::default();
    seen.insert(entry);
    let mut stack = vec![entry];
    while let Some(b) = stack.pop() {
        for s in cfg.succ_iter(b) {
            if seen.insert(s) {
                stack.push(s);
            }
        }
    }
    seen
}

pub fn run(func: &mut Function, an: &mut Analysis, pty: Type) -> usize {
    let crate::pipeline::CfgDtLa { cfg, dt: _, la } = an.cfg_dt_la(func);
    let reach = ret_reaching(func, cfg);
    let reach_out = reachable(func, cfg);
    // Dead-end blocks the compiler itself marked cold — pads and
    // panic paths. Unmarked dead-end blocks (post-`exit`,
    // `unreachable` continuations) may still run often enough that
    // the slot traffic costs more than a saved CSR pair.
    let dead: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|b| {
            !reach.contains(b)
                && reach_out.contains(b)
                && la.innermost_loop(*b).is_none()
                && func.layout.is_cold(*b)
        })
        .collect();
    if dead.is_empty() {
        return 0;
    }
    let uses = crate::uses::Uses::new(func);

    // REACH: blocks forward-reachable from a call in a dead block
    // through dead edges (excluding the call's own block). A value
    // used in one is live past that call via the edge; a value used
    // below a call in the call's own block is live past it directly.
    let mut reach_edge: FxHashSet<Block> = FxHashSet::default();
    for &b in &dead {
        if !func
            .layout
            .block_insts(b)
            .any(|i| matches!(func.dfg.insts[i].opcode(), Opcode::Call | Opcode::CallIndirect))
        {
            continue;
        }
        let mut stack: Vec<Block> = cfg
            .succ_iter(b)
            .filter(|s| dead.contains(s))
            .collect();
        while let Some(s) = stack.pop() {
            if !reach_edge.insert(s) {
                continue;
            }
            for t in cfg.succ_iter(s) {
                if dead.contains(&t) {
                    stack.push(t);
                }
            }
        }
    }

    // Candidates: defined in a dead block, every use in a dead block,
    // live range spanning a call in the dead region.
    let mut work: Vec<(Value, Vec<(Block, Inst)>)> = Vec::new();
    for &b in &dead {
        // Block params (the exception object arrives as one) and inst
        // results both qualify.
        let mut defs: Vec<Value> = func.dfg.block_params(b).to_vec();
        for i in func.layout.block_insts(b) {
            defs.extend(func.dfg.inst_results(i).iter().copied());
        }
        for v in defs {
            let v = func.dfg.resolve_aliases(v);
            // The alias may resolve to a def outside the dead region;
            // the store must stay on the dead-end path.
            let def_dead = match func.dfg.value_def(v) {
                ValueDef::Param(db, _) => dead.contains(&db),
                ValueDef::Result(d, _) => func
                    .layout
                    .inst_block(d)
                    .map_or(false, |db| dead.contains(&db)),
                ValueDef::Union(_, _) => false,
            };
            if !def_dead {
                continue;
            }
            let sites: &[(Block, Inst)] = uses.sites(v);
            if sites.is_empty() || !sites.iter().all(|(ub, _)| dead.contains(ub)) {
                continue;
            }
            // Live past a call? A use in a `reach_edge` block, or a
            // use below a `call`/`call_indirect` in its own block.
            let crossed = sites.iter().any(|&(ub, ui)| {
                reach_edge.contains(&ub)
                    || func
                        .layout
                        .block_insts(ub)
                        .take_while(|&i| i != ui)
                        .any(|i| {
                            matches!(
                                func.dfg.insts[i].opcode(),
                                Opcode::Call | Opcode::CallIndirect
                            )
                        })
            });
            if crossed {
                work.push((v, sites.to_vec()));
            }
        }
    }
    if work.is_empty() {
        return 0;
    }

    let mut n = 0;
    let mut slots: FxHashMap<Type, cranelift_codegen::ir::StackSlot> = FxHashMap::default();
    for (v, sites) in work {
        if n >= MAX_DEMOTIONS {
            break;
        }
        let ty = func.dfg.value_type(v);
        let bytes = ty.bytes();
        if bytes == 0 || bytes > 64 {
            continue;
        }
        let ss = *slots.entry(ty).or_insert_with(|| {
            func.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                bytes,
                bytes.trailing_zeros() as u8,
            ))
        });
        // Store right where the value is defined: at the top of a
        // param's block, or after the defining inst. SSA guarantees
        // this dominates every use, so the slot holds the current
        // activation's value on every path to a use.
        match func.dfg.value_def(v) {
            ValueDef::Param(b, _) => {
                let first = func.layout.first_inst(b).unwrap();
                FuncCursor::new(func).at_inst(first).ins().stack_store(pty, v, ss, 0);
            }
            ValueDef::Result(d, _) => {
                FuncCursor::new(func).after_inst(d).ins().stack_store(pty, v, ss, 0);
            }
            ValueDef::Union(_, _) => continue,
        }
        for (_, ui) in sites {
            let w = FuncCursor::new(func)
                .at_inst(ui)
                .ins()
                .stack_load(pty, ty, ss, 0);
            let idx: Vec<usize> = func
                .dfg
                .inst_args(ui)
                .iter()
                .enumerate()
                .filter(|(_, a)| func.dfg.resolve_aliases(**a) == v)
                .map(|(k, _)| k)
                .collect();
            for k in idx {
                func.dfg.inst_args_mut(ui)[k] = w;
            }
        }
        n += 1;
    }
    if n > 0 {
        an.invalidate(func);
    }
    n
}

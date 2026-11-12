//! Edge-conditioned select/branch fold (`PLIRON_EDGESEL`).
//!
//! On `brif c`'s then-edge `c != 0` and on its else-edge `c == 0`, so
//! a `select c, t, e` in the target picks a fixed arm. `str::chars`'s
//! `next().copied().unwrap_or(0)` clamp reaches CLIF as
//!
//! ```text
//! blockP:
//!     c = icmp eq cur, end
//!     brif c, done, body
//! body:
//!     a = select c, 0, cur        // load-addr clamp — c known 0 here
//!     b = select c, cur, cur+1    // cursor clamp
//! ```
//!
//! which isel lowers to a duplicate `cmp` plus two `cmov`s per char
//! on tokenize's ASCII scan (~1.4x vs LLVM; LLVM folds the selects
//! under the edge condition and pays neither). Intersecting the
//! `c == 0`/`c != 0` fact over every incoming edge folds each such
//! select to the known arm and turns a redundant `brif c` inside the
//! block into a `jump`.
//!
//! Only `brif` terminators contribute facts, and a fact survives
//! only when EVERY incoming edge establishes it — one `jump` or
//! `br_table` pred, or a `brif` on a different condition, drops the
//! whole map for that block. One CFG build + one inst scan; linear.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Function, Inst, InstructionData, Opcode, Value};
use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let blocks: Vec<_> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        // c's value on each incoming edge: Some(true) = nonzero,
        // Some(false) = zero, absent = nothing known. A fact must hold
        // on every edge, so the map only ever shrinks.
        let mut facts: FxHashMap<Value, bool> = FxHashMap::default();
        let mut first = true;
        for p in cfg.pred_iter(b) {
            let mut edge: FxHashMap<Value, bool> = FxHashMap::default();
            if let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] {
                let arg = func.dfg.resolve_aliases(arg);
                // `blocks[0]` is taken when `arg != 0`.
                for (i, bc) in blocks.iter().enumerate() {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    match edge.entry(arg) {
                        Entry::Vacant(e) => {
                            e.insert(i == 0);
                        }
                        // `brif c, b, b`: the edge count can't pin c.
                        Entry::Occupied(e) => {
                            if *e.get() != (i == 0) {
                                e.remove();
                            }
                        }
                    }
                }
            }
            if first {
                facts = edge;
                first = false;
            } else {
                facts.retain(|v, k| edge.get(v) == Some(k));
            }
            if facts.is_empty() {
                break;
            }
        }
        if facts.is_empty() {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            match func.dfg.insts[i] {
                InstructionData::Ternary {
                    opcode: Opcode::Select | Opcode::SelectSpectreGuard,
                    args,
                } => {
                    if let Some(&k) = facts.get(&func.dfg.resolve_aliases(args[0])) {
                        let arm = args[usize::from(!k) + 1];
                        let rs: Vec<Value> = func.dfg.inst_results(i).to_vec();
                        func.dfg.detach_inst_results(i);
                        for r in rs {
                            func.dfg.change_to_alias(r, arm);
                        }
                        func.layout.remove_inst(i);
                        n += 1;
                    }
                }
                InstructionData::Brif { arg, blocks, .. } => {
                    if let Some(&k) = facts.get(&func.dfg.resolve_aliases(arg)) {
                        let destination = blocks[usize::from(!k)];
                        func.dfg.insts[i] = InstructionData::Jump {
                            opcode: Opcode::Jump,
                            destination,
                        };
                        n += 1;
                    }
                }
                _ => {}
            }
        }
    }
    n
}

//! Block parameters whose incoming values are one value or undefined.
//!
//! The lowering has no `undef`: a variable read on a path with no write,
//! a promoted local restarted by a lifetime marker (`PLIRON_LTUNDEF`) and
//! LLVM `undef`/`poison` constants all become a plain `iconst 0`. A join
//! that merges a real value with such a zero is then a real merge, while
//! LLVM's `phi [v, undef]` folds to `v`. The common shape is a
//! `Result`/`Option` whose error path leaves the payload unwritten:
//! `usize::try_from(isize).unwrap_unchecked()` (memchr's `distance`) kept
//! `x >= 0 ? x : 0` in the hot loop, and loop-carried copies of the
//! unwritten payload went around the loop.
//!
//! `run` takes the lowering's list of those zeros and solves, per
//! parameter, the optimistic lattice `undef > one value > many` over its
//! incoming edge arguments (a parameter that is itself `many` contributes
//! itself; a cycle of parameters fed only undefined values stays
//! `undef`), with a worklist: each parameter is re-evaluated when one of
//! its argument parameters changes. A parameter that ends at one value
//! `v` whose definition dominates its block becomes an alias of `v`, and
//! its argument slot is stripped from every edge. Choosing `v` for the
//! undefined inputs is one of the values they may take. Parameters that
//! stay `undef` are left alone. Edges other than `jump`/`brif` make their
//! target's parameters `many`.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, Opcode, Value, ValueDef};
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum L {
    Undef,
    One(Value),
    Many,
}

fn meet(a: L, b: L) -> L {
    match (a, b) {
        (L::Undef, x) | (x, L::Undef) => x,
        (L::One(x), L::One(y)) if x == y => L::One(x),
        _ => L::Many,
    }
}

/// Returns the number of parameters removed.
pub fn run(func: &mut Function, undef: &[Value]) -> usize {
    let u: FxHashSet<Value> = undef.iter().map(|&v| func.dfg.resolve_aliases(v)).collect();
    if u.is_empty() {
        return 0;
    }
    let Some(entry) = func.layout.entry_block() else { return 0 };
    // Incoming arguments per parameter (`None`: not a plain value, or an
    // edge from something other than `jump`/`brif`).
    let mut inc: FxHashMap<Value, Vec<Option<Value>>> = FxHashMap::default();
    let mut bad: FxHashSet<Block> = FxHashSet::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else { continue };
        let plain = matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif);
        for bc in func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
            let tgt = bc.block(&func.dfg.value_lists);
            if !plain {
                bad.insert(tgt);
                continue;
            }
            let params = func.dfg.block_params(tgt);
            for (k, a) in bc.args(&func.dfg.value_lists).enumerate() {
                let Some(&p) = params.get(k) else { continue };
                let v = match a {
                    BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                    _ => None,
                };
                inc.entry(p).or_default().push(v);
            }
        }
    }
    if !inc.keys().any(|_| true) {
        return 0;
    }
    let is_param = |func: &Function, v: Value| matches!(func.dfg.value_def(v), ValueDef::Param(..));
    // State: every parameter with incoming edges starts optimistic.
    let mut st: FxHashMap<Value, L> = FxHashMap::default();
    let mut users: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    let mut work: Vec<Value> = Vec::new();
    for b in func.layout.blocks() {
        let ok = b != entry && !bad.contains(&b);
        for &p in func.dfg.block_params(b) {
            let s = if ok && inc.contains_key(&p) { L::Undef } else { L::Many };
            st.insert(p, s);
            if s == L::Undef {
                work.push(p);
                for a in inc[&p].iter().flatten() {
                    if is_param(func, *a) {
                        users.entry(*a).or_default().push(p);
                    }
                }
            }
        }
    }
    // Each state change is a step down the lattice or a switch from
    // `One(v)` to `One(q)` after `q` fell to `many`; the budget only
    // guards against a bug.
    let mut budget = 16 * st.len() + 64;
    while let Some(p) = work.pop() {
        if budget == 0 {
            return 0;
        }
        budget -= 1;
        if st[&p] == L::Many {
            continue;
        }
        let mut s = L::Undef;
        for a in &inc[&p] {
            let c = match *a {
                None => L::Many,
                Some(a) if a == p => L::Undef,
                Some(a) if u.contains(&a) => L::Undef,
                Some(a) => match st.get(&a) {
                    Some(L::Undef) => L::Undef,
                    Some(L::One(v)) => L::One(*v),
                    Some(L::Many) => L::One(a),
                    None => L::One(a),
                },
            };
            s = meet(s, c);
            if s == L::Many {
                break;
            }
        }
        if s != st[&p] {
            st.insert(p, s);
            if let Some(us) = users.get(&p) {
                work.extend(us.iter().copied());
            }
        }
    }
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let def_block = |func: &Function, v: Value| -> Option<Block> {
        match func.dfg.value_def(v) {
            ValueDef::Result(i, _) => func.layout.inst_block(i),
            ValueDef::Param(b, _) => Some(b),
            _ => None,
        }
    };
    // (block, index, param, value), checked for dominance.
    let mut rm: Vec<(Block, usize, Value, Value)> = Vec::new();
    for b in func.layout.blocks() {
        for (k, &p) in func.dfg.block_params(b).iter().enumerate() {
            let Some(L::One(v)) = st.get(&p).copied() else { continue };
            if v == p || func.dfg.value_type(v) != func.dfg.value_type(p) {
                continue;
            }
            let Some(db) = def_block(func, v) else { continue };
            // An instruction of `b` itself does not dominate `b`'s
            // parameters (a loop's back edge passing a value computed in
            // the header): only `b`'s other parameters do.
            if !dt.block_dominates(db, b)
                || (db == b && !matches!(func.dfg.value_def(v), ValueDef::Param(..)))
            {
                continue;
            }
            rm.push((b, k, p, v));
        }
    }
    if rm.is_empty() {
        return 0;
    }
    let mut idx: FxHashMap<Block, Vec<usize>> = FxHashMap::default();
    for r in &rm {
        idx.entry(r.0).or_default().push(r.1);
    }
    for v in idx.values_mut() {
        v.sort_unstable_by(|a, b| b.cmp(a));
    }
    let mut seen: FxHashSet<Inst> = FxHashSet::default();
    for (&b, is) in &idx {
        seen.clear();
        for pred in cfg.pred_iter(b) {
            let t = pred.inst;
            if !seen.insert(t) {
                continue;
            }
            let dfg = &mut func.dfg;
            for bc in dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                if bc.block(&dfg.value_lists) != b {
                    continue;
                }
                for &i in is {
                    bc.remove(i, &mut dfg.value_lists);
                }
            }
        }
    }
    for &(_, _, p, v) in &rm {
        func.dfg.remove_block_param(p);
        func.dfg.change_to_alias(p, v);
    }
    rm.len()
}

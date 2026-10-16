//! Merge identical blocks on Cranelift IR (LLVM's tail merging in
//! BranchFolding, restricted to whole blocks). After inlining, each bounds
//! check or `unwrap` site gets its own copy of the same panic call + trap;
//! blocks without params whose instructions print identically (internal
//! results renamed) are redirected to one copy. A block with successors
//! merges only when every value it defines stays inside it — the printed
//! terminator already pins identical destinations, and a local def used in
//! a sibling would dangle after retargeting. Duplicate *test* blocks are
//! the payload here: `is_alphabetic`-style chains emit the same
//! `x < 170` check per arm, and merging them lets rangefold see the
//! shared miss edge its union rule needs.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, BlockArg, Function, Opcode, Value, ValueDef};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 32;

/// Block each value is used in (None when used in several) — the
/// defs-local check for successor-bearing merges.
fn use_blocks(func: &Function) -> FxHashMap<Value, Option<Block>> {
    let mut m: FxHashMap<Value, Option<Block>> = Default::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let e = m.entry(func.dfg.resolve_aliases(v)).or_insert(Some(b));
                if *e != Some(b) {
                    *e = None;
                }
            }
        }
    }
    m
}

pub(crate) fn key(func: &Function, b: Block) -> Option<String> {
    let insts: Vec<_> = func.layout.block_insts(b).collect();
    let &last = insts.last()?;
    if insts.len() > MAX_INSTS
        || !func.dfg.block_params(b).is_empty()
        || !func.dfg.insts[last].opcode().is_terminator()
    {
        return None;
    }
    let mut s = if func.layout.is_cold(b) {
        String::from("cold\n")
    } else {
        String::new()
    };
    let mut local: FxHashMap<Value, usize> = FxHashMap::default();
    for &i in &insts {
        for &r in func.dfg.inst_results(i) {
            let n = local.len();
            local.insert(r, n);
        }
    }
    for &i in &insts {
        let text = func.dfg.display_inst(i).to_string();
        let mut tok = String::new();
        let flush = |tok: &mut String, s: &mut String| {
            if let Some(n) = tok.strip_prefix('v').and_then(|d| d.parse::<u32>().ok())
                && let Some(k) = local.get(&Value::from_u32(n))
            {
                s.push_str(&format!("%{k}"));
            } else if let Some(n) = tok.strip_prefix('v').and_then(|d| d.parse::<u32>().ok())
                && let Some(c) = constant(func, Value::from_u32(n))
            {
                s.push_str(&c);
            } else {
                s.push_str(tok);
            }
            tok.clear();
        };
        for c in text.chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                tok.push(c);
            } else {
                flush(&mut tok, &mut s);
                s.push(c);
            }
        }
        flush(&mut tok, &mut s);
        s.push('\n');
    }
    Some(s)
}

/// Argument-free pure definitions (constants, symbol addresses) compare by
/// their text, so copies materialized in different blocks still match.
fn constant(func: &Function, v: Value) -> Option<String> {
    if !func.dfg.value_is_valid(v) {
        return None;
    }
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    if !func.dfg.inst_args(i).is_empty()
        || !matches!(
            func.dfg.insts[i].opcode(),
            Opcode::Iconst
                | Opcode::F32const
                | Opcode::F64const
                | Opcode::Vconst
                | Opcode::SymbolValue
                | Opcode::FuncAddr
        )
    {
        return None;
    }
    let t = func.dfg.display_inst(i).to_string();
    Some(format!(
        "{{{}}}",
        t.split_once(" = ").map_or(t.as_str(), |x| x.1)
    ))
}

fn rematerialize_constants(func: &mut Function, block: Block) {
    let insts: Vec<_> = func.layout.block_insts(block).collect();
    let mut copies: FxHashMap<Value, Value> = FxHashMap::default();
    for inst in insts {
        let args = func.dfg.inst_args(inst).to_vec();
        for (n, arg) in args.into_iter().enumerate() {
            let arg = func.dfg.resolve_aliases(arg);
            let replacement = if let Some(&copy) = copies.get(&arg) {
                Some(copy)
            } else {
                let def = func.dfg.value_def(arg).inst().filter(|&def| {
                    func.layout.inst_block(def) != Some(block) && constant(func, arg).is_some()
                });
                if let Some(def) = def {
                    let ty = func.dfg.value_type(arg);
                    let data = func.dfg.insts[def].clone();
                    let copy = func.dfg.make_inst(data);
                    func.dfg.make_inst_results(copy, ty);
                    let value = func.dfg.first_result(copy);
                    let first = func.layout.first_inst(block).unwrap();
                    func.layout.insert_inst(copy, first);
                    copies.insert(arg, value);
                    Some(value)
                } else {
                    None
                }
            };
            if let Some(replacement) = replacement {
                func.dfg.inst_args_mut(inst)[n] = replacement;
            }
        }
    }
}

/// Returns the number of blocks merged away.
pub fn run(func: &mut Function) -> usize {
    let entry = func.layout.entry_block();
    let mut canon: FxHashMap<String, Block> = FxHashMap::default();
    let mut map: FxHashMap<Block, Block> = FxHashMap::default();
    let uses = use_blocks(func);
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for &b in &blocks {
        if Some(b) == entry {
            continue;
        }
        // A block with successors merges only when it is a pure
        // test-shape block and every result it defines stays inside —
        // a value escaping to a sibling would be stranded once preds
        // retarget to the canonical block, and merging anything wider
        // than a test collapses copies later passes specialize per arm
        // (the parse_only digit loop regressed ~25% without this).
        let Some(last) = func.layout.last_inst(b) else {
            continue;
        };
        let has_succs = !func
            .dfg
            .insts[last]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .is_empty();
        if has_succs {
            let insts: Vec<_> = func.layout.block_insts(b).collect();
            let pure_test = insts.len() <= 8
                && insts.iter().all(|&i| {
                    let op = func.dfg.insts[i].opcode();
                    !(op.can_store() || op.can_trap() || op.is_call() || op.other_side_effects())
                });
            if !pure_test
                || !insts.iter().all(|&i| {
                    func.dfg
                        .inst_results(i)
                        .iter()
                        .all(|v| uses.get(v).is_none_or(|&u| u == Some(b)))
                })
            {
                continue;
            }
        }
        if let Some(k) = key(func, b) {
            match canon.get(&k) {
                Some(&c) => {
                    map.insert(b, c);
                }
                None => {
                    canon.insert(k, b);
                }
            }
        }
    }
    if map.is_empty() {
        return 0;
    }
    // Redirecting a dead block's preds into its canonical block opens a
    // path into `canon` that may bypass the blocks `canon`'s used values
    // are defined in — `jump T(v)` where `v` was defined only on `canon`'s
    // original in-paths becomes an undominated edge arg. Keep a merge only
    // when every external def `canon` uses dominates all of the dead
    // block's preds.
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    let mut needs: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    for &c in map.values() {
        needs.entry(c).or_insert_with(|| {
            let mut s = FxHashSet::default();
            let note = |v: Value, s: &mut FxHashSet<Block>| match func
                .dfg
                .value_def(func.dfg.resolve_aliases(v))
            {
                ValueDef::Result(d, _) => {
                    if let Some(db) = func.layout.inst_block(d)
                        && db != c
                    {
                        s.insert(db);
                    }
                }
                ValueDef::Param(d, _) if d != c => {
                    s.insert(d);
                }
                _ => {}
            };
            for i in func.layout.block_insts(c) {
                for &v in func.dfg.inst_args(i) {
                    note(v, &mut s);
                }
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            note(v, &mut s);
                        }
                    }
                }
            }
            s
        });
    }
    map.retain(|&d, &mut c| {
        let Some(nd) = needs.get(&c) else {
            return true;
        };
        cfg.pred_iter(d)
            .all(|p| nd.iter().all(|&dv| dom.block_dominates(dv, p.block)))
    });
    if map.is_empty() {
        return 0;
    }
    let targets: FxHashSet<Block> = map.values().copied().collect();
    for &b in &blocks {
        if targets.contains(&b) {
            rematerialize_constants(func, b);
        }
    }
    let dead: FxHashSet<Block> = map.keys().copied().collect();
    for &b in &blocks {
        if dead.contains(&b) {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let dfg = &mut func.dfg;
        for bc in
            dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if let Some(&c) = map.get(&bc.block(&dfg.value_lists)) {
                bc.set_block(c, &mut dfg.value_lists);
            }
        }
    }
    for &b in &dead {
        while let Some(i) = func.layout.first_inst(b) {
            func.layout.remove_inst(i);
        }
        func.layout.remove_block(b);
    }
    map.len()
}

//! Merge identical exit blocks on Cranelift IR (LLVM's tail merging in
//! BranchFolding, restricted to whole blocks). After inlining, each bounds
//! check or `unwrap` site gets its own copy of the same panic call + trap;
//! blocks without params and without successors whose instructions print
//! identically (internal results renamed) are redirected to one copy.

use cranelift_codegen::ir::{Block, Function, Opcode, Value};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 32;

fn key(func: &Function, b: Block) -> Option<String> {
    let insts: Vec<_> = func.layout.block_insts(b).collect();
    let &last = insts.last()?;
    if insts.len() > MAX_INSTS
        || !func.dfg.block_params(b).is_empty()
        || !func.dfg.insts[last].opcode().is_terminator()
        || func.dfg.insts[last]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .len()
            != 0
    {
        return None;
    }
    let mut local: FxHashMap<Value, usize> = FxHashMap::default();
    for &i in &insts {
        for &r in func.dfg.inst_results(i) {
            let n = local.len();
            local.insert(r, n);
        }
    }
    let mut s = String::new();
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

/// Returns the number of blocks merged away.
pub fn run(func: &mut Function) -> usize {
    let entry = func.layout.entry_block();
    let mut canon: FxHashMap<String, Block> = FxHashMap::default();
    let mut map: FxHashMap<Block, Block> = FxHashMap::default();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for &b in &blocks {
        if Some(b) == entry {
            continue;
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

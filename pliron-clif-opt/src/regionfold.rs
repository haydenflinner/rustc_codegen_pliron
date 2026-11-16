//! Merge isomorphic single-entry regions inside one function.
//!
//! `pad_formatted_parts` inlines `write_formatted_parts` three times.
//! The copies are already in the CLIF, so a later inliner cannot put
//! them back. This pass keeps one copy and turns each original entry
//! into `consts; jump shared(args, tag)`. Exits that land on different
//! blocks dispatch on the tag.
//!
//! A value used in the shared body that is defined outside the region
//! is a parameter of the shared entry. A value used in an exit block
//! that does not dominate every copy is a parameter of that exit,
//! carried through the dispatcher. A copy that does not reach that
//! exit passes a zero of the same integer type. The tag selects the
//! copy that passed the real value, so the zero is not observed.
//!
//! `PLIRON_REGIONFOLD=1` enables it. Native was not timed.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{self, Type};
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 200;
const MAX_BLOCKS: usize = 48;
const MAX_COPIES: usize = 4;
const MIN_BODY: usize = 4;
const MAX_EXTRAS: usize = 64;
const MAX_TRIES: usize = 8;

enum Slot {
    /// Differing const materialization, one inst per copy. Cloned into
    /// the stub; the shared body uses the parameter.
    Const(Vec<Inst>),
    /// Differing external value, one per copy.
    Val(Vec<Value>, Type),
}

struct ExitTarget {
    block: Block,
    /// Append the tag parameter. False when every copy lands on `block`.
    tag: bool,
    /// Indices into the output list this dispatcher carries, in order.
    output_ix: Vec<usize>,
}

/// One integer value to add as a parameter of `dest`. `per_copy[i]` is
/// the value copy `i` passes; `None` is a zero, for a copy that the tag
/// never sends to `dest`.
struct Extra {
    dest: Block,
    ty: Type,
    per_copy: Vec<Option<Value>>,
}

/// A value defined inside the region and used in `dest`. The shared body
/// already computed it, so the exit edge carries the cloned value. It is
/// not a parameter of the stub.
struct Output {
    dest: Block,
    /// The owner's SSA name, the one `dest` uses today.
    used: Value,
    /// The same result in copy 0. The clone map turns it into the shared value.
    copy0: Value,
    ty: Type,
}

struct Region {
    /// `copies[i][j]` is copy `i`'s block for position `j`. `j == 0` is
    /// the entry.
    copies: Vec<Vec<Block>>,
    /// `(position, successor index)` edges that leave the region.
    exits: Vec<(usize, u32)>,
}

pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    for _ in 0..4 {
        if !fold_one(func) {
            break;
        }
        n += 1;
    }
    n
}

fn fold_one(func: &mut Function) -> bool {
    let cfg = ControlFlowGraph::with_function(func);
    let mut shape_of: FxHashMap<Block, String> = FxHashMap::default();
    let mut groups: FxHashMap<String, Vec<Block>> = FxHashMap::default();
    let entry = func.layout.entry_block();
    for b in func.layout.blocks() {
        if Some(b) == entry {
            continue;
        }
        let Some(s) = shape(func, b) else { continue };
        if s.lines().count() < 3 {
            continue;
        }
        shape_of.insert(b, s.clone());
        groups.entry(s).or_default().push(b);
    }
    let mut cands = Vec::new();
    for seeds in groups.values() {
        if seeds.len() < 2 || seeds.len() > MAX_COPIES {
            continue;
        }
        let Some(r) = grow(func, &cfg, &shape_of, seeds) else {
            continue;
        };
        let insts = body_insts(func, &r);
        if profitable(seeds.len(), insts) && preds_ok(func, &cfg, &r) && !escapes(func, &r) {
            cands.push((insts, r));
        }
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, r) in cands.into_iter().take(MAX_TRIES) {
        if apply(func, &cfg, r) {
            return true;
        }
    }
    false
}

fn profitable(copies: usize, body: usize) -> bool {
    copies >= 2 && body >= MIN_BODY && (copies - 1) * body >= copies * 2
}

fn body_insts(func: &Function, r: &Region) -> usize {
    r.copies[0]
        .iter()
        .map(|b| func.layout.block_insts(*b).count())
        .sum()
}

fn preds_ok(func: &Function, cfg: &ControlFlowGraph, r: &Region) -> bool {
    for copy in &r.copies {
        let set: FxHashSet<Block> = copy.iter().copied().collect();
        let entry = copy[0];
        let mut outside = false;
        for p in cfg.pred_iter(entry) {
            if !set.contains(&p.block) {
                outside = true;
            }
        }
        if !outside {
            return false;
        }
        for &b in &copy[1..] {
            let mut any = false;
            for p in cfg.pred_iter(b) {
                any = true;
                if !set.contains(&p.block) {
                    return false;
                }
            }
            if !any {
                return false;
            }
        }
    }
    true
}

fn grow(
    func: &Function,
    cfg: &ControlFlowGraph,
    shape_of: &FxHashMap<Block, String>,
    seeds: &[Block],
) -> Option<Region> {
    let n = seeds.len();
    let mut copies: Vec<Vec<Block>> = (0..n).map(|_| Vec::new()).collect();
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    for (i, &s) in seeds.iter().enumerate() {
        if !seen.insert(s) {
            return None;
        }
        copies[i].push(s);
    }
    let mut exits = Vec::new();
    let mut qi = 0;
    while qi < copies[0].len() {
        if copies[0].len() > MAX_BLOCKS {
            return None;
        }
        let pos = qi;
        qi += 1;
        let succs0 = succs(func, copies[0][pos])?;
        for i in 1..n {
            let si = succs(func, copies[i][pos])?;
            if si.len() != succs0.len() {
                return None;
            }
        }
        for (si, &s0) in succs0.iter().enumerate() {
            if let Some(at) = copies[0].iter().position(|&b| b == s0) {
                for i in 1..n {
                    let got = succs(func, copies[i][pos])?[si];
                    if copies[i][at] != got {
                        return None;
                    }
                }
                continue;
            }
            let mut sibs = Vec::with_capacity(n);
            let mut ok = shape_of.get(&s0).is_some();
            for i in 0..n {
                let s = succs(func, copies[i][pos])?[si];
                if shape_of.get(&s) != shape_of.get(&s0) || seen.contains(&s) {
                    ok = false;
                }
                sibs.push(s);
            }
            if ok {
                for (i, s) in sibs.into_iter().enumerate() {
                    seen.insert(s);
                    copies[i].push(s);
                }
            } else {
                exits.push((pos, si as u32));
            }
        }
    }
    if !region_insts_ok(func, &copies) {
        return None;
    }
    let _ = cfg;
    Some(Region { copies, exits })
}

fn region_insts_ok(func: &Function, copies: &[Vec<Block>]) -> bool {
    for b in &copies[0] {
        for i in func.layout.block_insts(*b) {
            if func.dfg.insts[i].exception_table().is_some()
                || func.dfg.insts[i].opcode() == Opcode::BrTable
            {
                return false;
            }
        }
    }
    true
}

fn succs(func: &Function, b: Block) -> Option<Vec<Block>> {
    let term = func.layout.last_inst(b)?;
    if func.dfg.insts[term].opcode() == Opcode::BrTable
        || func.dfg.insts[term].exception_table().is_some()
    {
        return None;
    }
    Some(
        func.dfg.insts[term]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .map(|bc| bc.block(&func.dfg.value_lists))
            .collect(),
    )
}

fn shape(func: &Function, b: Block) -> Option<String> {
    let insts: Vec<Inst> = func.layout.block_insts(b).collect();
    if insts.is_empty() || insts.len() > MAX_INSTS {
        return None;
    }
    let last = *insts.last()?;
    if !func.dfg.insts[last].opcode().is_terminator()
        || func.dfg.insts[last].opcode() == Opcode::BrTable
        || func.dfg.insts[last].exception_table().is_some()
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
        if is_const(func, i) {
            let r = func.dfg.inst_results(i).first()?;
            s.push_str(&format!("%{n} = CONST {ty:?}\n", n = local[r], ty = func.dfg.value_type(*r)));
            continue;
        }
        let text = func.dfg.display_inst(i).to_string();
        let text = text.split(';').next().unwrap_or("");
        let mut tok = String::new();
        let flush = |tok: &mut String, s: &mut String| {
            if let Some(rest) = tok.strip_prefix("block")
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_digit())
            {
                s.push_str("{B}");
            } else if let Some(n) = tok.strip_prefix('v').and_then(|d| d.parse::<u32>().ok()) {
                let v = func.dfg.resolve_aliases(Value::from_u32(n));
                if let Some(k) = local.get(&v) {
                    s.push_str(&format!("%{k}"));
                } else {
                    s.push_str("{V}");
                }
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

fn is_const(func: &Function, i: Inst) -> bool {
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            ..
        } | InstructionData::UnaryIeee32 { .. }
            | InstructionData::UnaryIeee64 { .. }
            | InstructionData::UnaryGlobalValue {
                opcode: Opcode::SymbolValue | Opcode::FuncAddr | Opcode::TlsValue,
                ..
            }
    )
}

fn const_eq(func: &Function, a: Inst, b: Inst) -> bool {
    match (&func.dfg.insts[a], &func.dfg.insts[b]) {
        (InstructionData::UnaryImm { imm: a, .. }, InstructionData::UnaryImm { imm: b, .. }) => {
            a.bits() == b.bits()
        }
        (InstructionData::UnaryIeee32 { imm: a, .. }, InstructionData::UnaryIeee32 { imm: b, .. }) => {
            a.bits() == b.bits()
        }
        (InstructionData::UnaryIeee64 { imm: a, .. }, InstructionData::UnaryIeee64 { imm: b, .. }) => {
            a.bits() == b.bits()
        }
        (
            InstructionData::UnaryGlobalValue { global_value: a, .. },
            InstructionData::UnaryGlobalValue { global_value: b, .. },
        ) => a == b,
        _ => false,
    }
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.resolved_value_def(v) {
        ValueDef::Result(d, _) => func.layout.inst_block(d),
        ValueDef::Param(pb, _) => Some(pb),
        ValueDef::Union(..) => None,
    }
}

fn dominates_all(func: &Function, dt: &DominatorTree, db: Block, entries: &[Block]) -> bool {
    entries.iter().all(|e| dt.dominates(db, *e, &func.layout))
}

/// Uses in `block` whose definition does not dominate every copy and is
/// not local to `block`. `None` when a value has no block (a union).
fn weak_uses(
    func: &Function,
    dt: &DominatorTree,
    entries: &[Block],
    block: Block,
) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    for i in func.layout.block_insts(block) {
        for v in func.dfg.inst_values(i) {
            let v = func.dfg.resolve_aliases(v);
            let db = def_block(func, v)?;
            if db == block || dominates_all(func, dt, db, entries) {
                continue;
            }
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    Some(out)
}

/// A use past the exit stays valid when its definition dominates every
/// copy, or sits in the exit's own subtree (the new edge still flows
/// through that definition). Anything else would need a parameter on
/// every block of the cone; skip the region instead.
fn cone_breaks(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    region: &FxHashSet<Block>,
    entries: &[Block],
    root: Block,
) -> bool {
    let mut stack: Vec<Block> = cfg.succ_iter(root).filter(|s| !region.contains(s)).collect();
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    while let Some(b) = stack.pop() {
        if !seen.insert(b) || region.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                let Some(db) = def_block(func, v) else {
                    return true;
                };
                let downstream = dt.dominates(root, db, &func.layout);
                if db != b && !dominates_all(func, dt, db, entries) && !downstream {
                    return true;
                }
            }
        }
        for s in cfg.succ_iter(b) {
            if !region.contains(&s) {
                stack.push(s);
            }
        }
    }
    false
}

fn exit_extras(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    r: &Region,
) -> Option<(Vec<Extra>, Vec<Output>)> {
    let region: FxHashSet<Block> = r.copies.iter().flatten().copied().collect();
    let entries: Vec<Block> = r.copies.iter().map(|c| c[0]).collect();
    let mut owners: FxHashMap<Block, Option<usize>> = FxHashMap::default();
    for &(pos, si) in &r.exits {
        for (ci, copy) in r.copies.iter().enumerate() {
            let d = succs(func, copy[pos])?.get(si as usize).copied()?;
            if region.contains(&d) {
                return None;
            }
            match owners.get(&d).copied() {
                None => {
                    owners.insert(d, Some(ci));
                }
                Some(Some(prev)) if prev != ci => {
                    owners.insert(d, None);
                }
                _ => {}
            }
        }
    }
    let mut extras = Vec::new();
    let mut outputs = Vec::new();
    for (&dest, owner) in &owners {
        if cone_breaks(func, cfg, dt, &region, &entries, dest) {
            return None;
        }
        let weaks = weak_uses(func, dt, &entries, dest)?;
        if weaks.is_empty() {
            continue;
        }
        let owner = (*owner)?;
        let outside = cfg.pred_iter(dest).any(|p| !region.contains(&p.block));
        for v in weaks {
            let ty = func.dfg.value_type(v);
            if !ty.is_int() || extras.len() + outputs.len() >= MAX_EXTRAS {
                return None;
            }
            let db = def_block(func, v)?;
            if region.contains(&db) {
                if outside || !r.copies[owner].contains(&db) {
                    return None;
                }
                let copy0 = parallel_value(func, r, owner, v)?;
                outputs.push(Output { dest, used: v, copy0, ty });
            } else {
                let mut per_copy = vec![None; r.copies.len()];
                per_copy[owner] = Some(v);
                extras.push(Extra { dest, ty, per_copy });
            }
        }
    }
    Some((extras, outputs))
}

/// `v` is defined in `copies[owner]`. The value in the same block and
/// instruction of copy 0 is what the shared clone produces.
fn parallel_value(func: &Function, r: &Region, owner: usize, v: Value) -> Option<Value> {
    let v = func.dfg.resolve_aliases(v);
    let db = def_block(func, v)?;
    let pos = r.copies[owner].iter().position(|&b| b == db)?;
    let b0 = r.copies[0][pos];
    match func.dfg.resolved_value_def(v) {
        ValueDef::Param(_, pi) => func.dfg.block_params(b0).get(pi).copied(),
        ValueDef::Result(inst, ri) => {
            let ii = func.layout.block_insts(db).position(|i| i == inst)?;
            let i0 = func.layout.block_insts(b0).nth(ii)?;
            func.dfg.inst_results(i0).get(ri).copied()
        }
        ValueDef::Union(..) => None,
    }
}

fn rewrite_uses(func: &mut Function, block: Block, map: &FxHashMap<Value, Value>) {
    for i in func.layout.block_insts(block).collect::<Vec<_>>() {
        let alias: Vec<(Value, Value)> = func
            .dfg
            .inst_values(i)
            .map(|v| (v, func.dfg.resolve_aliases(v)))
            .collect();
        let dfg = &mut func.dfg;
        let mut data = dfg.insts[i];
        data.map_values(
            &mut dfg.value_lists,
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
            |v| {
                let v = alias.iter().find(|(a, _)| *a == v).map(|(_, r)| *r).unwrap_or(v);
                *map.get(&v).unwrap_or(&v)
            },
        );
        func.dfg.insts[i] = data;
    }
}

fn append_exit_args(func: &mut Function, inst: Inst, dest: Block, vals: &[Value]) {
    let dfg = &mut func.dfg;
    let mut data = dfg.insts[inst];
    for d in data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
        if d.block(&dfg.value_lists) == dest {
            for &v in vals {
                d.append_argument(BlockArg::Value(v), &mut dfg.value_lists);
            }
        }
    }
    func.dfg.insts[inst] = data;
}

fn install_outputs(func: &mut Function, outputs: &[Output]) {
    let mut by_dest: FxHashMap<Block, Vec<(Value, Value)>> = FxHashMap::default();
    for o in outputs {
        let p = func.dfg.append_block_param(o.dest, o.ty);
        by_dest.entry(o.dest).or_default().push((o.used, p));
    }
    for (dest, pairs) in &by_dest {
        let map: FxHashMap<Value, Value> = pairs.iter().copied().collect();
        rewrite_uses(func, *dest, &map);
    }
}

/// Give each exit the extra parameters `exit_extras` asked for, and
/// teach the predecessors that stay behind to pass the real value.
fn install_extras(func: &mut Function, cfg: &ControlFlowGraph, r: &Region, extras: &[Extra]) {
    let region: FxHashSet<Block> = r.copies.iter().flatten().copied().collect();
    let mut by_dest: FxHashMap<Block, Vec<(Value, Value)>> = FxHashMap::default();
    for e in extras {
        let v = e.per_copy.iter().copied().flatten().next().unwrap();
        let p = func.dfg.append_block_param(e.dest, e.ty);
        by_dest.entry(e.dest).or_default().push((v, p));
    }
    for (dest, pairs) in &by_dest {
        let map: FxHashMap<Value, Value> = pairs.iter().copied().collect();
        rewrite_uses(func, *dest, &map);
        let vals: Vec<Value> = pairs.iter().map(|(v, _)| *v).collect();
        let preds: Vec<_> = cfg.pred_iter(*dest).collect();
        for p in preds {
            if region.contains(&p.block) {
                continue;
            }
            append_exit_args(func, p.inst, *dest, &vals);
        }
    }
}

fn dominance_ok(func: &Function) -> bool {
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let Some(db) = def_block(func, v) else {
                    return false;
                };
                if !dt.dominates(db, b, &func.layout) {
                    return false;
                }
            }
            if !func.dfg.insts[i].opcode().is_terminator() {
                continue;
            }
            for d in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
            {
                let target = d.block(&func.dfg.value_lists);
                if d.args(&func.dfg.value_lists).count() != func.dfg.block_params(target).len() {
                    return false;
                }
            }
        }
    }
    true
}

fn exit_dests(func: &Function, r: &Region) -> Option<FxHashSet<Block>> {
    let mut dests = FxHashSet::default();
    for &(pos, si) in &r.exits {
        for copy in &r.copies {
            let d = succs(func, copy[pos])?.get(si as usize).copied()?;
            dests.insert(d);
        }
    }
    Some(dests)
}

fn escapes(func: &Function, r: &Region) -> bool {
    let set: FxHashSet<Block> = r.copies.iter().flatten().copied().collect();
    // Uses in the exit block itself are threaded as exit parameters.
    // A use further out still means some other copy's stub would reach
    // a value it does not define.
    let dests = exit_dests(func, r).unwrap_or_default();
    let doomed = |v: Value| -> bool {
        let v = func.dfg.resolve_aliases(v);
        match func.dfg.resolved_value_def(v) {
            ValueDef::Result(i, _) => func.layout.inst_block(i).is_some_and(|b| set.contains(&b)),
            // Entry params too: a block outside this copy used one by
            // dominance, and the other copies' stubs also reach that block.
            ValueDef::Param(b, _) => set.contains(&b),
            ValueDef::Union(..) => false,
        }
    };
    for b in func.layout.blocks() {
        if set.contains(&b) || dests.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                if doomed(v) {
                    return true;
                }
            }
        }
    }
    false
}

fn apply(func: &mut Function, cfg: &ControlFlowGraph, r: Region) -> bool {
    let dt = DominatorTree::with_function(func, cfg);
    let Some((extras, outputs)) = exit_extras(func, cfg, &dt, &r) else {
        return false;
    };
    let saved = func.clone();
    if !apply_mut(func, cfg, &r, &extras, &outputs) || !dominance_ok(func) {
        *func = saved;
        return false;
    }
    true
}

fn apply_mut(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    r: &Region,
    extras: &[Extra],
    outputs: &[Output],
) -> bool {
    let set0: FxHashSet<Block> = r.copies[0].iter().copied().collect();
    install_extras(func, cfg, r, extras);
    install_outputs(func, outputs);
    let slots = match slots_of(func, r, &set0) {
        Some(s) => s,
        None => return false,
    };
    let need_tag = exits_need_tag(func, r);
    let exits = match build_exits(func, r, need_tag, extras, outputs) {
        Some(e) => e,
        None => return false,
    };
    let Some(shared) = clone_region(func, r, &set0, &slots, need_tag, &exits, extras, outputs)
    else {
        return false;
    };
    for (ci, copy) in r.copies.iter().enumerate() {
        stub(func, copy[0], shared[0], &slots, ci, need_tag, extras);
    }
    for copy in &r.copies {
        for &b in &copy[1..] {
            func.layout.remove_block_and_insts(b);
        }
    }
    true
}

fn exits_need_tag(func: &Function, r: &Region) -> bool {
    r.exits.iter().any(|&(pos, si)| {
        let dests: Vec<Block> = r
            .copies
            .iter()
            .filter_map(|c| succs(func, c[pos]).and_then(|s| s.get(si as usize).copied()))
            .collect();
        dests.len() == r.copies.len() && dests.iter().any(|d| *d != dests[0])
    })
}

fn slots_of(func: &Function, r: &Region, set0: &FxHashSet<Block>) -> Option<Vec<Slot>> {
    let sets: Vec<FxHashSet<Block>> = r
        .copies
        .iter()
        .map(|c| c.iter().copied().collect())
        .collect();
    let _ = set0;
    let mut slots = Vec::new();
    for (pos, _) in r.copies[0].iter().enumerate() {
        let insts: Vec<Vec<Inst>> = r
            .copies
            .iter()
            .map(|c| func.layout.block_insts(c[pos]).collect())
            .collect();
        let ninst = insts[0].len();
        if insts.iter().any(|v| v.len() != ninst) {
            return None;
        }
        for ii in 0..ninst {
            let parallel: Vec<Inst> = insts.iter().map(|v| v[ii]).collect();
            let i0 = parallel[0];
            if is_const(func, i0) {
                if parallel.iter().any(|&i| !const_eq(func, i, i0)) {
                    slots.push(Slot::Const(parallel));
                }
                continue;
            }
            for (ai, v0) in external_uses(func, i0, &sets[0]) {
                let ty = func.dfg.value_type(v0);
                if sets.iter().any(|s| in_region(func, v0, s)) {
                    return None;
                }
                let mut vals = vec![v0];
                for (ci, &i) in parallel.iter().enumerate().skip(1) {
                    let v = external_uses(func, i, &sets[ci])
                        .into_iter()
                        .find(|(a, _)| *a == ai)
                        .map(|(_, v)| v)?;
                    if func.dfg.value_type(v) != ty || sets.iter().any(|s| in_region(func, v, s)) {
                        return None;
                    }
                    vals.push(v);
                }
                // Every external is a parameter. A value that dominates
                // every entry would also dominate the shared block; one
                // that dominates only the copy it is used in does not.
                // The exit-parameter path covers the second case when the
                // use is in the exit block rather than the body.
                if let Some(Slot::Val(vs, _)) = slots.iter().find(|s| match s {
                    Slot::Val(vs, _) => vs[0] == v0,
                    _ => false,
                }) {
                    if vs != &vals {
                        return None;
                    }
                } else {
                    slots.push(Slot::Val(vals, ty));
                }
            }
        }
    }
    Some(slots)
}

fn external_uses(func: &Function, inst: Inst, region: &FxHashSet<Block>) -> Vec<(usize, Value)> {
    let mut out = Vec::new();
    for (ai, &v) in func.dfg.inst_args(inst).iter().enumerate() {
        let v = func.dfg.resolve_aliases(v);
        if !in_region(func, v, region) {
            out.push((ai, v));
        }
    }
    for bc in func.dfg.insts[inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .iter()
    {
        for (ai, a) in bc.args(&func.dfg.value_lists).enumerate() {
            if let BlockArg::Value(v) = a {
                let v = func.dfg.resolve_aliases(v);
                if !in_region(func, v, region) {
                    out.push((1_000 + ai, v));
                }
            }
        }
    }
    out
}

fn in_region(func: &Function, v: Value, region: &FxHashSet<Block>) -> bool {
    match func.dfg.resolved_value_def(v) {
        ValueDef::Param(b, _) => region.contains(&b),
        ValueDef::Result(i, _) => func.layout.inst_block(i).is_some_and(|b| region.contains(&b)),
        ValueDef::Union(..) => false,
    }
}

fn build_exits(
    func: &mut Function,
    r: &Region,
    need_tag: bool,
    extras: &[Extra],
    outputs: &[Output],
) -> Option<FxHashMap<(usize, u32), ExitTarget>> {
    let mut map = FxHashMap::default();
    for &(pos, si) in &r.exits {
        let mut dests = Vec::new();
        let mut arg_tys: Option<Vec<Type>> = None;
        for copy in &r.copies {
            let term = func.layout.last_inst(copy[pos])?;
            let bc = func.dfg.insts[term]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .get(si as usize)
                .copied()?;
            let mut tys = Vec::new();
            for a in bc.args(&func.dfg.value_lists) {
                let BlockArg::Value(v) = a else { return None };
                tys.push(func.dfg.value_type(v));
            }
            let dest = bc.block(&func.dfg.value_lists);
            let owned: Vec<Type> = extras
                .iter()
                .filter(|e| e.dest == dest)
                .map(|e| e.ty)
                .chain(outputs.iter().filter(|o| o.dest == dest).map(|o| o.ty))
                .collect();
            let params: Vec<Value> = func.dfg.block_params(dest).to_vec();
            if params.len() != tys.len() + owned.len() {
                return None;
            }
            for (p, ty) in params.iter().zip(tys.iter().chain(owned.iter())) {
                if func.dfg.value_type(*p) != *ty {
                    return None;
                }
            }
            match &arg_tys {
                None => arg_tys = Some(tys),
                Some(prev) if prev == &tys => {}
                _ => return None,
            }
            dests.push(dest);
        }
        let arg_tys = arg_tys.unwrap_or_default();
        let same = dests.iter().all(|d| *d == dests[0]);
        if same {
            map.insert(
                (pos, si),
                ExitTarget { block: dests[0], tag: false, output_ix: Vec::new() },
            );
        } else if need_tag {
            let output_ix: Vec<usize> = outputs
                .iter()
                .enumerate()
                .filter(|(_, o)| dests.contains(&o.dest))
                .map(|(i, _)| i)
                .collect();
            let block = emit_dispatch(func, &dests, &arg_tys, extras, outputs, &output_ix);
            map.insert((pos, si), ExitTarget { block, tag: true, output_ix });
        } else {
            return None;
        }
    }
    Some(map)
}

fn emit_dispatch(
    func: &mut Function,
    dests: &[Block],
    arg_tys: &[Type],
    extras: &[Extra],
    outputs: &[Output],
    output_ix: &[usize],
) -> Block {
    let n = dests.len();
    let mut blocks = Vec::with_capacity(n - 1);
    for _ in 0..n - 1 {
        let b = func.dfg.make_block();
        func.layout.append_block(b);
        for &ty in arg_tys {
            func.dfg.append_block_param(b, ty);
        }
        for e in extras {
            func.dfg.append_block_param(b, e.ty);
        }
        for &i in output_ix {
            func.dfg.append_block_param(b, outputs[i].ty);
        }
        func.dfg.append_block_param(b, types::I32);
        blocks.push(b);
    }
    let args_for = |ps: &[Value], dest: Block| -> Vec<BlockArg> {
        let mut args: Vec<BlockArg> =
            ps[..arg_tys.len()].iter().copied().map(BlockArg::from).collect();
        let extra_at = arg_tys.len();
        for (ei, e) in extras.iter().enumerate() {
            if e.dest == dest {
                args.push(BlockArg::from(ps[extra_at + ei]));
            }
        }
        let out_at = extra_at + extras.len();
        for (si, &oi) in output_ix.iter().enumerate() {
            if outputs[oi].dest == dest {
                args.push(BlockArg::from(ps[out_at + si]));
            }
        }
        args
    };
    for i in 0..n - 1 {
        let b = blocks[i];
        let ps: Vec<Value> = func.dfg.block_params(b).to_vec();
        let tag = *ps.last().unwrap();
        let mut pos = FuncCursor::new(func).at_bottom(b);
        let k = pos.ins().iconst(types::I32, i as i64);
        let c = pos.ins().icmp(IntCC::Equal, tag, k);
        let args_i = args_for(&ps, dests[i]);
        if i + 1 == n - 1 {
            let args_j = args_for(&ps, dests[i + 1]);
            pos.ins().brif(c, dests[i], &args_i, dests[i + 1], &args_j);
        } else {
            let next: Vec<BlockArg> = ps.iter().copied().map(BlockArg::from).collect();
            pos.ins().brif(c, dests[i], &args_i, blocks[i + 1], &next);
        }
    }
    blocks[0]
}

fn clone_region(
    func: &mut Function,
    r: &Region,
    set0: &FxHashSet<Block>,
    slots: &[Slot],
    need_tag: bool,
    exits: &FxHashMap<(usize, u32), ExitTarget>,
    extras: &[Extra],
    outputs: &[Output],
) -> Option<Vec<Block>> {
    let mut shared = Vec::new();
    for &b in &r.copies[0] {
        let nb = func.dfg.make_block();
        func.layout.append_block(nb);
        for p in func.dfg.block_params(b).to_vec() {
            let ty = func.dfg.value_type(p);
            func.dfg.append_block_param(nb, ty);
        }
        shared.push(nb);
    }
    let entry = shared[0];
    let mut extra_params = Vec::new();
    for s in slots {
        let ty = match s {
            Slot::Const(insts) => func.dfg.value_type(func.dfg.inst_results(insts[0])[0]),
            Slot::Val(_, ty) => *ty,
        };
        extra_params.push(func.dfg.append_block_param(entry, ty));
    }
    let mut exit_params = Vec::new();
    for e in extras {
        exit_params.push(func.dfg.append_block_param(entry, e.ty));
    }
    let tag = if need_tag {
        Some(func.dfg.append_block_param(entry, types::I32))
    } else {
        None
    };
    let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
    for (b, sb) in r.copies[0].iter().zip(&shared) {
        let orig = func.dfg.block_params(*b).to_vec();
        let neu = func.dfg.block_params(*sb).to_vec();
        for (p, sp) in orig.iter().zip(neu.iter()) {
            vmap.insert(*p, *sp);
        }
    }
    let mut si = 0;
    for s in slots {
        if let Slot::Const(insts) = s {
            let r0 = func.dfg.inst_results(insts[0])[0];
            vmap.insert(r0, extra_params[si]);
        }
        if let Slot::Val(vals, _) = s {
            vmap.insert(vals[0], extra_params[si]);
        }
        si += 1;
    }
    let skip_const: FxHashSet<Inst> = slots
        .iter()
        .filter_map(|s| match s {
            Slot::Const(insts) => Some(insts[0]),
            _ => None,
        })
        .collect();
    let block_map: FxHashMap<Block, Block> = r.copies[0].iter().copied().zip(shared.iter().copied()).collect();
    for (pos, &b) in r.copies[0].iter().enumerate() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            if skip_const.contains(&i) {
                continue;
            }
            let alias: Vec<(Value, Value)> = func
                .dfg
                .inst_values(i)
                .map(|v| (v, func.dfg.resolve_aliases(v)))
                .collect();
            let ni = func.dfg.clone_inst(i);
            func.set_srcloc(ni, func.srcloc(i));
            let dfg = &mut func.dfg;
            let mut data = dfg.insts[ni];
            for d in data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                *d = d.deep_clone(&mut dfg.value_lists);
            }
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |v| {
                    let v = alias.iter().find(|(a, _)| *a == v).map(|(_, r)| *r).unwrap_or(v);
                    *vmap.get(&v).unwrap_or(&v)
                },
            );
            if func.dfg.insts[i].opcode().is_terminator()
                && !retarget(
                    &mut data,
                    &mut func.dfg,
                    pos,
                    &block_map,
                    exits,
                    &exit_params,
                    outputs,
                    &vmap,
                    tag,
                )
            {
                return None;
            }
            func.dfg.insts[ni] = data;
            func.layout.append_inst(ni, shared[pos]);
            for (&o, &nv) in func.dfg.inst_results(i).iter().zip(func.dfg.inst_results(ni).iter()) {
                vmap.insert(o, nv);
            }
        }
    }
    let _ = set0;
    Some(shared)
}

fn retarget(
    data: &mut InstructionData,
    dfg: &mut cranelift_codegen::ir::DataFlowGraph,
    pos: usize,
    block_map: &FxHashMap<Block, Block>,
    exits: &FxHashMap<(usize, u32), ExitTarget>,
    extra_params: &[Value],
    outputs: &[Output],
    vmap: &FxHashMap<Value, Value>,
    tag: Option<Value>,
) -> bool {
    for (si, dest) in data
        .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        .iter_mut()
        .enumerate()
    {
        let b = dest.block(&dfg.value_lists);
        if let Some(&sb) = block_map.get(&b) {
            dest.set_block(sb, &mut dfg.value_lists);
            continue;
        }
        let Some(ex) = exits.get(&(pos, si as u32)) else {
            continue;
        };
        dest.set_block(ex.block, &mut dfg.value_lists);
        if ex.tag {
            for &p in extra_params {
                dest.append_argument(BlockArg::Value(p), &mut dfg.value_lists);
            }
            for &ix in &ex.output_ix {
                let Some(&sv) = vmap.get(&outputs[ix].copy0) else {
                    return false;
                };
                dest.append_argument(BlockArg::Value(sv), &mut dfg.value_lists);
            }
            if let Some(tag) = tag {
                dest.append_argument(BlockArg::Value(tag), &mut dfg.value_lists);
            }
        }
    }
    true
}

fn stub(
    func: &mut Function,
    entry: Block,
    shared_entry: Block,
    slots: &[Slot],
    copy: usize,
    need_tag: bool,
    extras: &[Extra],
) {
    let params: Vec<Value> = func.dfg.block_params(entry).to_vec();
    for i in func.layout.block_insts(entry).collect::<Vec<_>>() {
        func.layout.remove_inst(i);
    }
    let mut pos = FuncCursor::new(func).at_bottom(entry);
    let mut args: Vec<BlockArg> = params.iter().copied().map(BlockArg::from).collect();
    for s in slots {
        match s {
            Slot::Const(insts) => {
                let ni = pos.func.dfg.clone_inst(insts[copy]);
                pos.insert_inst(ni);
                args.push(BlockArg::from(pos.func.dfg.inst_results(ni)[0]));
            }
            Slot::Val(vals, _) => args.push(BlockArg::from(vals[copy])),
        }
    }
    for e in extras {
        match e.per_copy[copy] {
            Some(v) => args.push(BlockArg::from(v)),
            None => {
                let z = pos.ins().iconst(e.ty, 0);
                args.push(BlockArg::from(z));
            }
        }
    }
    if need_tag {
        let t = pos.ins().iconst(types::I32, copy as i64);
        args.push(BlockArg::from(t));
    }
    pos.ins().jump(shared_entry, &args);
}

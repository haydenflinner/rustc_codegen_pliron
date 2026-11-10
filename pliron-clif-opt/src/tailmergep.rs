//! Parameterized tail merge (`PLIRON_TMPARAM`, opt-in).
//!
//! Cold error/return tails that are identical modulo `iconst`
//! immediates — the UTF-8 validator's ~15 `Utf8Error`-construction
//! exits, differing only in tag/`error_len` immediates — collapse to
//! `iconsts; jump M(args)` stubs feeding one shared copy
//! `M(p1..pk): body`. Each member keeps its block identity so no pred
//! edges move, and every member dominates the shared block, so the
//! body's external operands (all identical `Value`s by key-match)
//! still dominate every use. LLVM emits exactly this shape: the
//! differing immediate materializes at each site and the construction
//! tail is shared.
//!
//! Two operand families can differ across a group: a local `iconst`'s
//! immediate (masked in the shape key), and an operand that names an
//! `iconst` defined outside the block (normalized to `{C}` in the key).
//! Positions where all members agree stay concrete in the shared body;
//! positions that differ become params, materialized per stub.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::{
    types::Type, Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 32;
/// Branch-arg marker for an occurrence in `inst_args` (not a bc index).
const ARG: u32 = u32::MAX;

fn is_iconst(func: &Function, i: Inst) -> bool {
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            ..
        }
    )
}

fn iconst_val(func: &Function, v: Value) -> bool {
    func.dfg.value_is_valid(v)
        && func
            .dfg
            .resolved_value_def(v)
            .inst()
            .is_some_and(|i| is_iconst(func, i))
}

/// Block each value is used in (None when used in several) — mirrors
/// `tailmerge`'s defs-local check.
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

struct Member {
    block: Block,
    insts: Vec<Inst>,
    /// `iconst` defs in inst order: (inst position, imm, result ty).
    locals: Vec<(usize, i64, Type)>,
    /// Ext-`iconst` operand occurrences, in scan order:
    /// (inst position, bc index or `ARG`, arg index, resolved value).
    exts: Vec<(u32, u32, u32, Value)>,
}

/// Shape key: local defs positional, local `iconst` immediates masked,
/// external `iconst` operands normalized to `{C}`.
fn key_of(func: &Function, b: Block) -> Option<(String, Member)> {
    if Some(b) == func.layout.entry_block() || !func.dfg.block_params(b).is_empty() {
        return None;
    }
    let insts: Vec<Inst> = func.layout.block_insts(b).collect();
    let &last = insts.last()?;
    if insts.len() > MAX_INSTS || !func.dfg.insts[last].opcode().is_terminator() {
        return None;
    }
    let mut local: FxHashMap<Value, usize> = FxHashMap::default();
    for &i in &insts {
        for &r in func.dfg.inst_results(i) {
            let n = local.len();
            local.insert(r, n);
        }
    }
    let mut m = Member {
        block: b,
        insts: insts.clone(),
        locals: vec![],
        exts: vec![],
    };
    let mut s = if func.layout.is_cold(b) {
        String::from("cold\n")
    } else {
        String::new()
    };
    for (ii, &i) in insts.iter().enumerate() {
        for (ai, &v) in func.dfg.inst_args(i).iter().enumerate() {
            let v = func.dfg.resolve_aliases(v);
            if !local.contains_key(&v) && iconst_val(func, v) {
                m.exts.push((ii as u32, ARG, ai as u32, v));
            }
        }
        for (bci, bc) in func
            .dfg
            .insts[i]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            for (ai, a) in bc.args(&func.dfg.value_lists).enumerate() {
                match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        if !local.contains_key(&v) && iconst_val(func, v) {
                            m.exts.push((ii as u32, bci as u32, ai as u32, v));
                        }
                    }
                    _ => return None,
                }
            }
        }
        if is_iconst(func, i) {
            let Some(&r) = func.dfg.inst_results(i).first() else {
                return None;
            };
            let imm = match func.dfg.insts[i] {
                InstructionData::UnaryImm { imm, .. } => imm.bits(),
                _ => return None,
            };
            m.locals
                .push((ii, imm, func.dfg.value_type(r)));
            s.push_str(&format!("%{} = CONST {:?}\n", local[&r], func.dfg.value_type(r)));
            continue;
        }
        // `display_inst` appends `; vN = K` resolved-const comments —
        // they reintroduce the masked immediates, so cut them first.
        let text = func.dfg.display_inst(i).to_string();
        let text = text.split(';').next().unwrap_or("").to_string();
        let mut tok = String::new();
        let flush = |tok: &mut String, s: &mut String| {
            if let Some(n) = tok.strip_prefix('v').and_then(|d| d.parse::<u32>().ok()) {
                let v = func.dfg.resolve_aliases(Value::from_u32(n));
                if let Some(k) = local.get(&v) {
                    s.push_str(&format!("%{k}"));
                } else if iconst_val(func, v) {
                    s.push_str("{C}");
                } else {
                    s.push_str(tok);
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
    Some((s, m))
}

enum Slot {
    /// `locals` index whose imm differs across members.
    Local(usize, Type),
    /// `exts` index whose value differs across members.
    Ext(usize, Type),
}

/// Returns the number of stubs installed.
pub fn run(func: &mut Function) -> usize {
    let mut groups: FxHashMap<String, Vec<Member>> = FxHashMap::default();
    for b in func.layout.blocks() {
        if let Some((k, m)) = key_of(func, b) {
            groups.entry(k).or_default().push(m);
        }
    }
    let uses = use_blocks(func);
    let mut merges: Vec<(Vec<Member>, Vec<Slot>)> = vec![];
    'grp: for members in groups.into_values() {
        if members.len() < 2 {
            continue;
        }
        // Every result's uses must stay inside its member — the body is
        // deleted and the value would dangle.
        let group: FxHashSet<Block> = members.iter().map(|m| m.block).collect();
        for m in &members {
            let inside: FxHashSet<Value> = m
                .insts
                .iter()
                .flat_map(|&i| func.dfg.inst_results(i))
                .copied()
                .collect();
            if !inside
                .iter()
                .all(|v| uses.get(v).is_none_or(|&u| u == Some(m.block)))
            {
                continue 'grp;
            }
            // No operand may be defined inside a group member — the
            // stub deletes its body.
            for &i in &m.insts {
                for v in func.dfg.inst_args(i) {
                    let v = func.dfg.resolve_aliases(*v);
                    if inside.contains(&v) {
                        continue;
                    }
                    if let Some(d) = func.dfg.resolved_value_def(v).inst() {
                        if group.contains(&func.layout.inst_block(d).unwrap_or(m.block)) {
                            continue 'grp;
                        }
                    }
                }
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            let v = func.dfg.resolve_aliases(v);
                            if inside.contains(&v) {
                                continue;
                            }
                            if let Some(d) = func.dfg.resolved_value_def(v).inst() {
                                if group
                                    .contains(&func.layout.inst_block(d).unwrap_or(m.block))
                                {
                                    continue 'grp;
                                }
                            }
                        }
                    }
                }
            }
        }
        let m0 = &members[0];
        let mut slots = vec![];
        for j in 0..m0.locals.len() {
            if members.iter().any(|m| m.locals[j].1 != m0.locals[j].1) {
                slots.push(Slot::Local(j, m0.locals[j].2));
            }
        }
        for j in 0..m0.exts.len() {
            if members.iter().any(|m| m.exts[j].3 != m0.exts[j].3) {
                slots.push(Slot::Ext(j, func.dfg.value_type(m0.exts[j].3)));
            }
        }
        // Identical-modulo-numbering tails are `tailmerge`'s job.
        if slots.is_empty() {
            continue;
        }
        let n_local = slots.iter().filter(|s| matches!(s, Slot::Local(..))).count();
        // before: n * len; after: n * (n_local + jump) + (len - n_local)
        let len = m0.insts.len();
        let n = members.len();
        if n * len <= n * (n_local + 1) + (len - n_local) {
            continue;
        }
        merges.push((members, slots));
    }
    if merges.is_empty() {
        return 0;
    }
    let mut stubs = 0;
    for (members, slots) in merges {
        if !pliron_cfg::bisect("tailmergep") {
            continue;
        }
        let m0 = &members[0];
        let mb = func.dfg.make_block();
        func.layout.insert_block_after(mb, m0.block);
        if func.layout.is_cold(m0.block) {
            func.layout.set_cold(mb);
        }
        let params: Vec<Value> = slots
            .iter()
            .map(|s| match s {
                Slot::Local(_, ty) | Slot::Ext(_, ty) => func.dfg.append_block_param(mb, *ty),
            })
            .collect();
        // Clone member 0's body into `mb`: differing local iconsts are
        // skipped and their results seed the value map with the param;
        // differing ext-const operands are rewritten by position after.
        let skip: FxHashSet<usize> = slots
            .iter()
            .filter_map(|s| match s {
                Slot::Local(j, _) => Some(m0.locals[*j].0),
                _ => None,
            })
            .collect();
        let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
        for (si, s) in slots.iter().enumerate() {
            if let Slot::Local(j, _) = s {
                let (ii, _, _) = m0.locals[*j];
                let r = func.dfg.inst_results(m0.insts[ii])[0];
                vmap.insert(r, params[si]);
            }
        }
        let alias: FxHashMap<Value, Value> = m0
            .insts
            .iter()
            .flat_map(|&i| func.dfg.inst_values(i))
            .map(|v| (v, func.dfg.resolve_aliases(v)))
            .collect();
        let mut cloned: Vec<Option<Inst>> = vec![None; m0.insts.len()];
        for (ii, &i) in m0.insts.iter().enumerate() {
            if skip.contains(&ii) {
                continue;
            }
            let ni = func.dfg.clone_inst(i);
            func.set_srcloc(ni, func.srcloc(i));
            let dfg = &mut func.dfg;
            let mut data = dfg.insts[ni];
            for d in
                data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
            {
                *d = d.deep_clone(&mut dfg.value_lists);
            }
            let dfg = &mut func.dfg;
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |v| {
                    let v = *alias.get(&v).unwrap_or(&v);
                    *vmap.get(&v).unwrap_or(&v)
                },
            );
            func.dfg.insts[ni] = data;
            func.layout.append_inst(ni, mb);
            for (&o, &nv) in func
                .dfg
                .inst_results(i)
                .iter()
                .zip(func.dfg.inst_results(ni))
            {
                vmap.insert(o, nv);
            }
            cloned[ii] = Some(ni);
        }
        for (si, s) in slots.iter().enumerate() {
            if let Slot::Ext(j, _) = s {
                let (ii, bci, ai, _) = m0.exts[*j];
                let ni = cloned[ii as usize].unwrap();
                if bci == ARG {
                    func.dfg.inst_args_mut(ni)[ai as usize] = params[si];
                } else {
                    let dfg = &mut func.dfg;
                    let mut data = dfg.insts[ni];
                    let mut k = 0u32;
                    data.branch_destination_mut(
                        &mut dfg.jump_tables,
                        &mut dfg.exception_tables,
                    )[bci as usize]
                        .update_args(&mut dfg.value_lists, |a| {
                            let r = if k == ai {
                                BlockArg::Value(params[si])
                            } else {
                                a
                            };
                            k += 1;
                            r
                        });
                    dfg.insts[ni] = data;
                }
            }
        }
        // Each member shrinks to `iconsts; jump mb(args)` — the block
        // keeps its identity so preds need no retargeting.
        for m in &members {
            for &i in &m.insts {
                func.layout.remove_inst(i);
            }
            let mut pos = FuncCursor::new(func).at_bottom(m.block);
            let mut args = vec![];
            for s in &slots {
                match s {
                    Slot::Local(j, ty) => {
                        let (_, imm, _) = m.locals[*j];
                        args.push(pos.ins().iconst(*ty, imm));
                    }
                    Slot::Ext(j, _) => args.push(m.exts[*j].3),
                }
            }
            let args: Vec<BlockArg> = args.iter().map(|&v| BlockArg::from(v)).collect();
            pos.ins().jump(mb, &args);
            stubs += 1;
        }
    }
    stubs
}

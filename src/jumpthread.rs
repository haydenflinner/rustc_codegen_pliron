//! Jump threading on the final Cranelift IR (`PLIRON_JUMPTHREAD`).
//!
//! After SROA, Rust's `Option`/`bool`-returning helpers become merges whose
//! incoming values are often constants (`None` = null, `true`/`false`), and the
//! merge block immediately tests them: `jump b(0)` … `b(v): brif v, …`.
//! Cranelift has no jump threading, so each such edge pays a materialised
//! constant, a jump and a test. Here a predecessor edge whose block arguments
//! decide the merge block's branch is retargeted straight to the taken
//! successor. Only merge blocks made of a few pure integer ops are threaded,
//! and only when no use of the merge block's values is reachable from the new
//! target without passing through the merge block, so dominance still holds.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value,
    ValueDef,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_BODY: usize = 12;

fn mask(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        v
    } else {
        v & ((1u64 << bits) - 1)
    }
}

fn sext(v: u64, bits: u32) -> i64 {
    if bits >= 64 {
        v as i64
    } else {
        let s = 64 - bits;
        ((v << s) as i64) >> s
    }
}

fn bits(func: &Function, v: Value) -> Option<u32> {
    let ty = func.dfg.value_type(v);
    (ty.is_int() && ty.bits() <= 64).then(|| ty.bits())
}

/// Constant value of `v`: from `env`, or an `iconst` anywhere in the function.
fn known(func: &Function, env: &FxHashMap<Value, u64>, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    if let Some(&c) = env.get(&v) {
        return Some(c);
    }
    let ValueDef::Result(inst, 0) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(mask(imm.bits() as u64, bits(func, v)?)),
        _ => None,
    }
}

fn icmp(cc: IntCC, a: u64, b: u64, w: u32) -> bool {
    let (sa, sb) = (sext(a, w), sext(b, w));
    match cc {
        IntCC::Equal => a == b,
        IntCC::NotEqual => a != b,
        IntCC::SignedLessThan => sa < sb,
        IntCC::SignedGreaterThanOrEqual => sa >= sb,
        IntCC::SignedGreaterThan => sa > sb,
        IntCC::SignedLessThanOrEqual => sa <= sb,
        IntCC::UnsignedLessThan => a < b,
        IntCC::UnsignedGreaterThanOrEqual => a >= b,
        IntCC::UnsignedGreaterThan => a > b,
        IntCC::UnsignedLessThanOrEqual => a <= b,
    }
}

/// Evaluate one pure body instruction of a merge block.
fn eval(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    nz: &dyn Fn(Value) -> bool,
    inst: Inst,
) -> Option<u64> {
    let res = func.dfg.first_result(inst);
    let w = bits(func, res)?;
    let r = match func.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits() as u64,
        InstructionData::IntCompare { cond, args, .. } => {
            let aw = bits(func, args[0])?;
            match (known(func, env, args[0]), known(func, env, args[1])) {
                (Some(a), Some(b)) => icmp(cond, a, b, aw) as u64,
                (Some(0), None) | (None, Some(0)) => {
                    let x = if known(func, env, args[0]).is_some() {
                        args[1]
                    } else {
                        args[0]
                    };
                    match (cond, nz(x)) {
                        (IntCC::Equal, true) => 0,
                        (IntCC::NotEqual, true) => 1,
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        InstructionData::Unary { opcode, arg } => {
            let aw = bits(func, arg)?;
            let a = known(func, env, arg)?;
            match opcode {
                Opcode::Uextend | Opcode::Ireduce => a,
                Opcode::Sextend => sext(a, aw) as u64,
                Opcode::Bnot => !a,
                _ => return None,
            }
        }
        InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } => {
            let c = known(func, env, args[0])?;
            known(func, env, args[if c != 0 { 1 } else { 2 }])?
        }
        InstructionData::Binary { opcode, args } => {
            let (a, b) = (known(func, env, args[0])?, known(func, env, args[1])?);
            match opcode {
                Opcode::Band => a & b,
                Opcode::Bor => a | b,
                Opcode::Bxor => a ^ b,
                Opcode::Iadd => a.wrapping_add(b),
                Opcode::Isub => a.wrapping_sub(b),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(mask(r, w))
}

fn pure_op(func: &Function, inst: Inst) -> bool {
    use Opcode::*;
    matches!(
        func.dfg.insts[inst].opcode(),
        Iconst
            | Icmp
            | Uextend
            | Sextend
            | Ireduce
            | Bnot
            | Band
            | Bor
            | Bxor
            | Iadd
            | Isub
            | Select
    ) && func.dfg.inst_results(inst).len() == 1
}

/// For each block, the other blocks that use one of its values (params or results).
fn users(func: &Function) -> FxHashMap<Block, FxHashSet<Block>> {
    let mut out: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    let def_block = |v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    for b in func.layout.blocks() {
        for inst in func.layout.block_insts(b) {
            let mut note = |v: Value| {
                if let Some(d) = def_block(v)
                    && d != b
                {
                    out.entry(d).or_default().insert(b);
                }
            };
            for &v in func.dfg.inst_args(inst) {
                note(v);
            }
            for bc in func.dfg.insts[inst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        note(v);
                    }
                }
            }
        }
    }
    out
}

const MAX_WALK: usize = 4096;

/// Values known non-null: `!nonnull` loads, inbounds offsets of non-null
/// pointers, and block params whose every incoming argument is non-null
/// (greatest fixpoint).
pub struct NonNull<'a> {
    loads: &'a FxHashSet<Value>,
    derived: &'a FxHashMap<Value, Value>,
    params: FxHashSet<Value>,
}

impl<'a> NonNull<'a> {
    fn new(
        func: &Function,
        loads: &'a FxHashSet<Value>,
        derived: &'a FxHashMap<Value, Value>,
    ) -> Self {
        let mut nn = NonNull {
            loads,
            derived,
            params: FxHashSet::default(),
        };
        if loads.is_empty() {
            return nn;
        }
        let entry = func.layout.entry_block();
        for b in func.layout.blocks() {
            if Some(b) != entry {
                nn.params.extend(
                    func.dfg
                        .block_params(b)
                        .iter()
                        .copied()
                        .filter(|&p| func.dfg.value_type(p) == types::I64),
                );
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for b in func.layout.blocks() {
                let Some(t) = func.layout.last_inst(b) else {
                    continue;
                };
                for bc in func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    let d = bc.block(&func.dfg.value_lists);
                    let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                    for (i, &p) in func.dfg.block_params(d).iter().enumerate() {
                        if nn.params.contains(&p)
                            && !matches!(args.get(i), Some(&BlockArg::Value(v)) if nn.is(func, v))
                        {
                            nn.params.remove(&p);
                            changed = true;
                        }
                    }
                }
            }
        }
        nn
    }

    fn is(&self, func: &Function, v: Value) -> bool {
        let mut v = func.dfg.resolve_aliases(v);
        for _ in 0..8 {
            if self.loads.contains(&v) || self.params.contains(&v) {
                return true;
            }
            match self.derived.get(&v) {
                Some(&b) => v = func.dfg.resolve_aliases(b),
                None => return false,
            }
        }
        false
    }
}

/// `icmp eq/ne x, 0` with `x` known non-null → constant.
fn fold_null_tests(func: &mut Function, nn: &NonNull) -> usize {
    let none = FxHashMap::default();
    let mut hits = Vec::new();
    for b in func.layout.blocks() {
        for inst in func.layout.block_insts(b) {
            let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[inst] else {
                continue;
            };
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual)
                || func.dfg.value_type(args[0]).is_vector()
            {
                continue;
            }
            let x = match (known(func, &none, args[0]), known(func, &none, args[1])) {
                (Some(0), None) => args[1],
                (None, Some(0)) => args[0],
                _ => continue,
            };
            if nn.is(func, x) {
                hits.push((inst, (cond == IntCC::NotEqual) as i64));
            }
        }
    }
    for &(inst, k) in &hits {
        let ty = func.dfg.value_type(func.dfg.first_result(inst));
        func.replace(inst).iconst(ty, k);
    }
    hits.len()
}

enum Arg {
    V(Value),
    K(u64, Type),
}

/// A retargeted edge `pinst`'s destination `di` → `s`. With `repair`, the values
/// of the bypassed merge block that are used from `s` on become new params of
/// `s` (fed by the merge block's own branch `term`/`ti` and by the new edge).
struct Edit {
    pinst: Inst,
    di: usize,
    s: Block,
    args: Vec<Arg>,
    repair: Option<(Inst, usize, Vec<Value>, Vec<Block>)>,
}

/// Blocks reachable from `s` without entering `b`, or None past MAX_WALK.
fn region(cfg: &ControlFlowGraph, b: Block, s: Block) -> Option<Vec<Block>> {
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut order = Vec::new();
    let mut work = vec![s];
    while let Some(x) = work.pop() {
        if x == b || !seen.insert(x) {
            continue;
        }
        order.push(x);
        if order.len() > MAX_WALK {
            return None;
        }
        work.extend(cfg.succ_iter(x));
    }
    Some(order)
}

/// Thread one round; returns the number of edges retargeted.
fn round(func: &mut Function, nn: &NonNull) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let users = users(func);
    let entry = func.layout.entry_block();
    let def_block = |v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    let mut edits: Vec<Edit> = Vec::new();
    let mut touched: FxHashSet<Inst> = FxHashSet::default();
    // Blocks taking part in any edit, and blocks no later edit may touch.
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut locked: FxHashSet<Block> = FxHashSet::default();
    for b in func.layout.blocks() {
        if Some(b) == entry || locked.contains(&b) {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        let Some((&term, body)) = insts.split_last() else {
            continue;
        };
        if body.len() > MAX_BODY || !body.iter().all(|&i| pure_op(func, i)) {
            continue;
        }
        let InstructionData::Brif {
            arg: cond,
            blocks: targets,
            ..
        } = func.dfg.insts[term]
        else {
            continue;
        };
        let params = func.dfg.block_params(b).to_vec();
        let body_vals: FxHashSet<Value> = body.iter().map(|&i| func.dfg.first_result(i)).collect();
        let b_users = users.get(&b);
        for pred in cfg.pred_iter(b) {
            let (pinst, pblock) = (pred.inst, pred.block);
            if pblock == b || touched.contains(&pinst) || locked.contains(&pblock) {
                continue;
            }
            if !matches!(func.dfg.insts[pinst].opcode(), Opcode::Jump | Opcode::Brif) {
                continue;
            }
            let dests = func.dfg.insts[pinst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            for (di, bc) in dests.iter().enumerate() {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if args.len() != params.len() {
                    continue;
                }
                let mut env: FxHashMap<Value, u64> = FxHashMap::default();
                let mut argv: FxHashMap<Value, Value> = FxHashMap::default();
                let mut pn: FxHashSet<Value> = FxHashSet::default();
                let mut ok = true;
                for (&p, a) in params.iter().zip(&args) {
                    let BlockArg::Value(v) = *a else {
                        ok = false;
                        break;
                    };
                    let v = func.dfg.resolve_aliases(v);
                    argv.insert(p, v);
                    if let Some(c) = known(func, &env, v) {
                        env.insert(p, c);
                    } else if nn.is(func, v) {
                        pn.insert(p);
                    }
                }
                if !ok {
                    continue;
                }
                let nz = |v: Value| {
                    let v = func.dfg.resolve_aliases(v);
                    nn.is(func, v) || pn.contains(&v)
                };
                for &i in body {
                    if let Some(c) = eval(func, &env, &nz, i) {
                        env.insert(func.dfg.first_result(i), c);
                    }
                }
                let taken = match known(func, &env, cond) {
                    Some(c) => c != 0,
                    None if nz(cond) => true,
                    None => continue,
                };
                let ti = if taken { 0 } else { 1 };
                let t = targets[ti];
                let s = t.block(&func.dfg.value_lists);
                if s == b || locked.contains(&s) {
                    continue;
                }
                let Some(reg) = region(&cfg, b, s) else {
                    continue;
                };
                // B's values used from `s` on (resolved), in first-use order.
                let mut needed: Vec<Value> = Vec::new();
                if let Some(bu) = b_users
                    && reg.iter().any(|x| bu.contains(x))
                {
                    for &x in &reg {
                        if !bu.contains(&x) {
                            continue;
                        }
                        for inst in func.layout.block_insts(x) {
                            for v in func.dfg.inst_values(inst) {
                                let r = func.dfg.resolve_aliases(v);
                                if def_block(r) == Some(b) && !needed.contains(&r) {
                                    needed.push(r);
                                }
                            }
                        }
                    }
                }
                let repair = !needed.is_empty();
                if repair
                    && (cfg.pred_iter(s).count() != 1
                        || targets[1 - ti].block(&func.dfg.value_lists) == s
                        || seen.contains(&b)
                        || seen.contains(&s)
                        || seen.contains(&pblock)
                        || reg.iter().any(|x| seen.contains(x))
                        || !reg
                            .iter()
                            .filter(|x| b_users.is_some_and(|bu| bu.contains(x)))
                            .all(|&x| domtree.block_dominates(s, x)))
                {
                    continue;
                }
                let map = |v: Value, out: &mut Vec<Arg>| -> bool {
                    if let Some(&pv) = argv.get(&v) {
                        out.push(Arg::V(pv));
                    } else if body_vals.contains(&v) {
                        match env.get(&v) {
                            Some(&k) => out.push(Arg::K(k, func.dfg.value_type(v))),
                            None => return false,
                        }
                    } else {
                        out.push(Arg::V(v));
                    }
                    true
                };
                let mut new = Vec::new();
                for a in t.args(&func.dfg.value_lists) {
                    let BlockArg::Value(v) = a else {
                        ok = false;
                        break;
                    };
                    ok &= map(func.dfg.resolve_aliases(v), &mut new);
                }
                for &w in &needed {
                    ok &= map(w, &mut new);
                }
                if !ok {
                    continue;
                }
                touched.insert(pinst);
                seen.extend([b, s, pblock]);
                let repair = repair.then(|| {
                    touched.insert(term);
                    locked.extend([b, s, pblock]);
                    locked.extend(reg.iter().copied());
                    (term, ti, needed, reg)
                });
                edits.push(Edit {
                    pinst,
                    di,
                    s,
                    args: new,
                    repair,
                });
                break;
            }
        }
    }
    let n = edits.len();
    for e in edits {
        if let Some((term, ti, needed, reg)) = e.repair {
            let mut to: FxHashMap<Value, Value> = FxHashMap::default();
            for &w in &needed {
                let ty = func.dfg.value_type(w);
                to.insert(w, func.dfg.append_block_param(e.s, ty));
            }
            for x in reg {
                let insts: Vec<Inst> = func.layout.block_insts(x).collect();
                for inst in insts {
                    let repl: FxHashMap<Value, Value> = func
                        .dfg
                        .inst_values(inst)
                        .filter_map(|v| to.get(&func.dfg.resolve_aliases(v)).map(|&n| (v, n)))
                        .collect();
                    if !repl.is_empty() {
                        func.dfg
                            .map_inst_values(inst, |v| repl.get(&v).copied().unwrap_or(v));
                    }
                }
            }
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[term]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[ti];
            for &w in &needed {
                bc.append_argument(w, &mut dfg.value_lists);
            }
        }
        let mut args = Vec::with_capacity(e.args.len());
        for a in e.args {
            args.push(BlockArg::Value(match a {
                Arg::V(v) => v,
                Arg::K(k, ty) => {
                    let mut cur = FuncCursor::new(func).at_inst(e.pinst);
                    cur.ins().iconst(ty, k as i64)
                }
            }));
        }
        let bc = BlockCall::new(e.s, args, &mut func.dfg.value_lists);
        let dfg = &mut func.dfg;
        dfg.insts[e.pinst]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.di] = bc;
    }
    n
}

/// Splice `jump`-only successors that have no other predecessor into their
/// predecessor, so a merge block and its test end up in one block.
fn merge_chains(func: &mut Function) {
    let cfg = ControlFlowGraph::with_function(func);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut gone: FxHashSet<Block> = FxHashSet::default();
    for x in blocks {
        if gone.contains(&x) {
            continue;
        }
        while let Some(term) = func.layout.last_inst(x) {
            let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
                break;
            };
            let y = destination.block(&func.dfg.value_lists);
            if y == x || Some(y) == entry || cfg.pred_iter(y).count() != 1 {
                break;
            }
            let args: Vec<Value> = destination
                .args(&func.dfg.value_lists)
                .filter_map(|a| match a {
                    BlockArg::Value(v) => Some(v),
                    _ => None,
                })
                .collect();
            if args.len() != func.dfg.num_block_params(y) {
                break;
            }
            let params = func.dfg.detach_block_params(y);
            let params: Vec<Value> = params.as_slice(&func.dfg.value_lists).to_vec();
            for (p, a) in params.into_iter().zip(args) {
                func.dfg.change_to_alias(p, a);
            }
            func.layout.remove_inst(term);
            let insts: Vec<Inst> = func.layout.block_insts(y).collect();
            for i in insts {
                func.layout.remove_inst(i);
                func.layout.append_inst(i, x);
            }
            func.layout.remove_block(y);
            gone.insert(y);
        }
    }
}

/// Thread until nothing changes (bounded); returns the edges retargeted.
pub fn run(
    func: &mut Function,
    loads: &FxHashSet<Value>,
    derived: &FxHashMap<Value, Value>,
) -> usize {
    merge_chains(func);
    let mut total = 0;
    for _ in 0..8 {
        let nn = NonNull::new(func, loads, derived);
        let n = round(func, &nn);
        total += n;
        if n == 0 {
            break;
        }
    }
    let nn = NonNull::new(func, loads, derived);
    total + fold_null_tests(func, &nn)
}

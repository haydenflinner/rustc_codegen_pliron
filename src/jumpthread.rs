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
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
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
    rg: &dyn Fn(Value) -> Option<(u64, u64)>,
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
            let ranged = range_cmp(
                cond,
                known(func, env, args[0]),
                known(func, env, args[1]),
                rg(args[0]),
                rg(args[1]),
                aw,
            );
            match (known(func, env, args[0]), known(func, env, args[1])) {
                _ if ranged.is_some() => u64::from(ranged == Some(true)),
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

type KBits = FxHashMap<Value, (u64, u64)>;

/// Known `(zero, one)` bit masks of `v` from constants, `env`, `kenv` and
/// bitwise ops / constant shifts / extends on them.
fn kbits(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    kenv: &KBits,
    v: Value,
    depth: u32,
) -> (u64, u64) {
    let v = func.dfg.resolve_aliases(v);
    let Some(w) = bits(func, v) else {
        return (0, 0);
    };
    let full = mask(u64::MAX, w);
    if let Some(c) = known(func, env, v) {
        return (!c & full, c);
    }
    if let Some(&k) = kenv.get(&v) {
        return k;
    }
    let Some(i) = func.dfg.value_def(v).inst() else {
        return (0, 0);
    };
    if depth >= 6 {
        return (0, 0);
    }
    let sub = |x: Value| kbits(func, env, kenv, x, depth + 1);
    let op = |opcode: Opcode, a: (u64, u64), b: (u64, u64)| -> (u64, u64) {
        let s = (b.0 | b.1 == full)
            .then_some(b.1)
            .filter(|&s| s < u64::from(w));
        match (opcode, s) {
            (Opcode::Band, _) => (a.0 | b.0, a.1 & b.1),
            (Opcode::Bor, _) => (a.0 & b.0, a.1 | b.1),
            (Opcode::Bxor, _) => ((a.0 & b.0) | (a.1 & b.1), (a.0 & b.1) | (a.1 & b.0)),
            (Opcode::Ishl, Some(s)) => ((a.0 << s) | ((1u64 << s) - 1), a.1 << s),
            (Opcode::Ushr, Some(s)) => ((a.0 >> s) | !(full >> s), a.1 >> s),
            _ => (0, 0),
        }
    };
    let r = match func.dfg.insts[i] {
        InstructionData::Binary { opcode, args } => op(opcode, sub(args[0]), sub(args[1])),
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => {
            let a = sub(arg);
            let aw = bits(func, arg).unwrap_or(w);
            (a.0 | (full & !mask(u64::MAX, aw)), a.1)
        }
        InstructionData::Unary {
            opcode: Opcode::Ireduce,
            arg,
        } => sub(arg),
        _ => (0, 0),
    };
    (r.0 & full, r.1 & full)
}

/// Constant result of body instruction `i` from known bits: fully known
/// values, and `x ==/!= k` where a known bit of `x` differs from `k`.
fn kfold(func: &Function, env: &FxHashMap<Value, u64>, kenv: &KBits, i: Inst) -> Option<u64> {
    if kenv.is_empty() {
        return None;
    }
    if let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] {
        if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
            return None;
        }
        let (x, k) = match (known(func, env, args[0]), known(func, env, args[1])) {
            (None, Some(k)) => (args[0], k),
            (Some(k), None) => (args[1], k),
            _ => return None,
        };
        let (z, o) = kbits(func, env, kenv, x, 0);
        return ((o & !k) | (z & k) != 0).then_some(u64::from(cond == IntCC::NotEqual));
    }
    let r = func.dfg.first_result(i);
    let full = mask(u64::MAX, bits(func, r)?);
    let (z, o) = kbits(func, env, kenv, r, 0);
    (z | o == full).then_some(o)
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
            | Ishl
            | Ushr
            | StackAddr
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
            if self.loads.contains(&v) || self.params.contains(&v) || addr_of_symbol(func, v) {
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

/// Addresses of symbols, thread-locals and stack slots are never null.
fn addr_of_symbol(func: &Function, v: Value) -> bool {
    func.dfg.value_def(v).inst().is_some_and(|i| {
        matches!(
            func.dfg.insts[i].opcode(),
            Opcode::TlsValue | Opcode::SymbolValue | Opcode::FuncAddr | Opcode::StackAddr
        ) && crate::pass_enabled("PLIRON_NN_ADDR")
    })
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
fn round(func: &mut Function, nn: &NonNull, domcond: bool) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let users = users(func);
    let facts = if domcond {
        edge_facts(func, &cfg)
    } else {
        FxHashMap::default()
    };
    let entry = func.layout.entry_block();
    let kb_on = crate::pass_enabled("PLIRON_KBITS");
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
                // Known bits of non-constant incoming args (e.g. a packed `Option` tag).
                let mut kenv: KBits = FxHashMap::default();
                if kb_on {
                    let none = FxHashMap::default();
                    for (&p, &v) in &argv {
                        if !env.contains_key(&p) {
                            let k = kbits(func, &env, &none, v, 0);
                            if k != (0, 0) {
                                kenv.insert(p, k);
                            }
                        }
                    }
                }
                let nz = |v: Value| {
                    let v = func.dfg.resolve_aliases(v);
                    nn.is(func, v) || pn.contains(&v)
                };
                // Ranges of the incoming args implied by branches dominating the pred.
                let mut prange: FxHashMap<Value, (u64, u64)> = FxHashMap::default();
                let fs = facts_at(&domtree, &facts, pblock);
                if !fs.is_empty() {
                    for (&p, &v) in &argv {
                        if !env.contains_key(&p)
                            && let Some(r) = urange(func, &fs, v, 0)
                        {
                            prange.insert(p, r);
                        }
                    }
                }
                let rg = |v: Value| prange.get(&func.dfg.resolve_aliases(v)).copied();
                for &i in body {
                    if let Some(c) =
                        eval(func, &env, &nz, &rg, i).or_else(|| kfold(func, &env, &kenv, i))
                    {
                        env.insert(func.dfg.first_result(i), c);
                    }
                }
                let taken = match known(func, &env, cond) {
                    Some(c) => c != 0,
                    None if nz(cond) => true,
                    None if !kenv.is_empty() && kbits(func, &env, &kenv, cond, 0).1 != 0 => true,
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

/// Retarget edges into blocks that only `jump` onward straight to the final
/// target, so a constant arg reaches the block that tests it. Each forwarded
/// arg must be a param of the bypassed block or defined where it dominates
/// the predecessor's branch. Returns the edges retargeted.
fn bypass_forwarders(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    // Block params used outside their own block: bypassing that block would
    // leave those uses undominated.
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for &x in &blocks {
        for i in func.layout.block_insts(x) {
            let mut note = |v: Value| {
                let v = func.dfg.resolve_aliases(v);
                *uses.entry(v).or_default() += 1;
                if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                    && d != x
                {
                    escaping.insert(v);
                }
            };
            func.dfg.inst_args(i).iter().for_each(|&v| note(v));
            for bc in func.dfg.insts[i]
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
    let mut n = 0;
    for b in blocks {
        if Some(b) == entry
            || func
                .dfg
                .block_params(b)
                .iter()
                .any(|p| escaping.contains(p))
        {
            continue;
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        // Any other insts must be pure and used only by each other.
        let body: Vec<Inst> = func.layout.block_insts(b).filter(|&i| i != term).collect();
        let mut local: FxHashMap<Value, u32> = FxHashMap::default();
        for &i in &body {
            for &a in func.dfg.inst_args(i) {
                *local.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
        }
        if !body.iter().all(|&i| {
            pure_op(func, i) && {
                let r = func.dfg.first_result(i);
                uses.get(&r).copied().unwrap_or(0) == local.get(&r).copied().unwrap_or(0)
            }
        }) {
            continue;
        }
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let t = destination.block(&func.dfg.value_lists);
        if t == b {
            continue;
        }
        let mut targs = Vec::new();
        for a in destination.args(&func.dfg.value_lists) {
            match a {
                BlockArg::Value(v) => targs.push(func.dfg.resolve_aliases(v)),
                _ => targs.clear(),
            }
        }
        if targs.len() != func.dfg.num_block_params(t) {
            continue;
        }
        let params = func.dfg.block_params(b).to_vec();
        let preds: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        for (pb, pinst) in preds {
            if pb == b
                || !matches!(
                    func.dfg.insts[pinst].opcode(),
                    Opcode::Jump | Opcode::Brif | Opcode::BrTable
                )
            {
                continue;
            }
            let dests: Vec<BlockCall> = func.dfg.insts[pinst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            for (di, bc) in dests.into_iter().enumerate() {
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let pargs: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if pargs.len() != params.len() {
                    continue;
                }
                let mut new = Vec::with_capacity(targs.len());
                for &v in &targs {
                    if let Some(k) = params.iter().position(|&p| p == v) {
                        new.push(pargs[k]);
                        continue;
                    }
                    let ok = match func.dfg.value_def(v) {
                        ValueDef::Result(i, _) => {
                            i != pinst && domtree.dominates(i, pinst, &func.layout)
                        }
                        ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
                        _ => false,
                    };
                    if !ok {
                        break;
                    }
                    new.push(BlockArg::Value(v));
                }
                if new.len() != targs.len() {
                    continue;
                }
                let nbc = BlockCall::new(t, new, &mut func.dfg.value_lists);
                let dfg = &mut func.dfg;
                dfg.insts[pinst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
                    nbc;
                n += 1;
            }
        }
    }
    n
}

/// The icmp a branch condition tests, through `band 1` and extends of its 0/1 result.
fn cond_icmp(func: &Function, v: Value) -> Option<Inst> {
    let env = FxHashMap::default();
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..6 {
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                ..
            } => return Some(i),
            InstructionData::Unary {
                opcode: Opcode::Uextend | Opcode::Ireduce,
                arg,
            } => v = func.dfg.resolve_aliases(arg),
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => {
                let (a, b) = (
                    func.dfg.resolve_aliases(args[0]),
                    func.dfg.resolve_aliases(args[1]),
                );
                if known(func, &env, b) == Some(1) {
                    v = a;
                } else if known(func, &env, a) == Some(1) {
                    v = b;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

type Cmp = (IntCC, Value, Value);

/// A scalar icmp as `(cc, x, y)`; `(x - y) ==/!= 0` becomes `x ==/!= y`.
fn norm_icmp(func: &Function, i: Inst) -> Option<Cmp> {
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let (mut a, mut b) = (
        func.dfg.resolve_aliases(args[0]),
        func.dfg.resolve_aliases(args[1]),
    );
    if func.dfg.value_type(a).is_vector() {
        return None;
    }
    if matches!(cond, IntCC::Equal | IntCC::NotEqual) {
        let env = FxHashMap::default();
        if known(func, &env, a) == Some(0) {
            std::mem::swap(&mut a, &mut b);
        }
        if known(func, &env, b) == Some(0)
            && let Some(d) = func.dfg.value_def(a).inst()
            && let InstructionData::Binary {
                opcode: Opcode::Isub,
                args: s,
            } = func.dfg.insts[d]
        {
            return Some((
                cond,
                func.dfg.resolve_aliases(s[0]),
                func.dfg.resolve_aliases(s[1]),
            ));
        }
    }
    Some((cond, a, b))
}

/// What a true fact `f` says about query `q`, if anything.
fn implied(f: Cmp, q: Cmp) -> Option<bool> {
    use IntCC::*;
    let (fc, fx, fy) = f;
    let qc = if (q.1, q.2) == (fx, fy) {
        q.0
    } else if (q.1, q.2) == (fy, fx) {
        q.0.swap_args()
    } else {
        return None;
    };
    if qc == fc {
        return Some(true);
    }
    if qc == fc.complement() {
        return Some(false);
    }
    match fc {
        Equal => Some(matches!(
            qc,
            UnsignedGreaterThanOrEqual
                | UnsignedLessThanOrEqual
                | SignedGreaterThanOrEqual
                | SignedLessThanOrEqual
        )),
        UnsignedLessThan | UnsignedGreaterThan | SignedLessThan | SignedGreaterThan => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Decide `a cc b` when one side is the constant `k` and the other lies in
/// the unsigned range `[lo, hi]` (`w`-bit compare).
fn range_cmp(
    cc: IntCC,
    a: Option<u64>,
    b: Option<u64>,
    ra: Option<(u64, u64)>,
    rb: Option<(u64, u64)>,
    w: u32,
) -> Option<bool> {
    use IntCC::*;
    let (cc, (lo, hi), k) = match (a, b) {
        (None, Some(k)) => (cc, ra?, k),
        (Some(k), None) => (cc.swap_args(), rb?, k),
        _ => return None,
    };
    let half = 1u64 << (w - 1);
    let cc = match cc {
        _ if hi >= half || k >= half => cc,
        SignedLessThan => UnsignedLessThan,
        SignedLessThanOrEqual => UnsignedLessThanOrEqual,
        SignedGreaterThan => UnsignedGreaterThan,
        SignedGreaterThanOrEqual => UnsignedGreaterThanOrEqual,
        c => c,
    };
    let dec = |t: bool, f: bool| {
        if t {
            Some(true)
        } else if f {
            Some(false)
        } else {
            None
        }
    };
    match cc {
        UnsignedLessThan => dec(hi < k, lo >= k),
        UnsignedLessThanOrEqual => dec(hi <= k, lo > k),
        UnsignedGreaterThan => dec(lo > k, hi <= k),
        UnsignedGreaterThanOrEqual => dec(lo >= k, hi < k),
        Equal => dec(lo == k && hi == k, k < lo || k > hi),
        NotEqual => dec(k < lo || k > hi, lo == k && hi == k),
        _ => None,
    }
}

/// Unsigned range of `v` from its width, `uextend`/`band` with a constant,
/// and the facts `fs` known where it is used.
fn urange(func: &Function, fs: &[Cmp], v: Value, depth: u32) -> Option<(u64, u64)> {
    let v = func.dfg.resolve_aliases(v);
    let w = bits(func, v)?;
    if func.dfg.value_type(v).is_vector() {
        return None;
    }
    let full = mask(u64::MAX, w);
    let (mut lo, mut hi) = (0u64, full);
    let env = FxHashMap::default();
    for &(cc, a, b) in fs {
        let (cc, k) = if a == v {
            (cc, known(func, &env, b))
        } else if b == v {
            (cc.swap_args(), known(func, &env, a))
        } else {
            continue;
        };
        let Some(k) = k else { continue };
        match cc {
            IntCC::UnsignedLessThan if k > 0 => hi = hi.min(k - 1),
            IntCC::UnsignedLessThanOrEqual => hi = hi.min(k),
            IntCC::UnsignedGreaterThan if k < full => lo = lo.max(k + 1),
            IntCC::UnsignedGreaterThanOrEqual => lo = lo.max(k),
            IntCC::Equal => {
                lo = lo.max(k);
                hi = hi.min(k);
            }
            _ => {}
        }
    }
    if depth < 4
        && let Some(i) = func.dfg.value_def(v).inst()
    {
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } => {
                if let Some((l, h)) = urange(func, fs, arg, depth + 1) {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Binary {
                opcode: opcode @ (Opcode::Iadd | Opcode::Isub),
                args,
            } => {
                if let Some(k) = known(func, &env, args[1])
                    && let Some((l, h)) = urange(func, fs, args[0], depth + 1)
                {
                    let (l2, h2) = if opcode == Opcode::Isub {
                        (l.wrapping_sub(k), h.wrapping_sub(k))
                    } else {
                        (l.wrapping_add(k), h.wrapping_add(k))
                    };
                    // Only when the whole range moves without wrapping.
                    let nowrap = if opcode == Opcode::Isub {
                        l >= k
                    } else {
                        h <= full - k
                    };
                    if nowrap {
                        lo = lo.max(l2);
                        hi = hi.min(h2);
                    }
                }
            }
            InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg,
            } => {
                if let Some((l, h)) = urange(func, fs, arg, depth + 1)
                    && h <= full
                {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => {
                if let Some(m) = known(func, &env, args[1]).or(known(func, &env, args[0])) {
                    hi = hi.min(m);
                }
            }
            _ => {}
        }
    }
    (lo <= hi && (lo, hi) != (0, full)).then_some((lo, hi))
}

/// The condition known on entry to each block whose only predecessor ends
/// in `brif c` (`c` or its complement).
fn edge_facts(func: &Function, cfg: &ControlFlowGraph) -> FxHashMap<Block, Cmp> {
    let mut fact: FxHashMap<Block, Cmp> = FxHashMap::default();
    for b in func.layout.blocks() {
        let mut preds = cfg.pred_iter(b);
        let (Some(p), None) = (preds.next(), preds.next()) else {
            continue;
        };
        if p.block == b {
            continue;
        }
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] else {
            continue;
        };
        let t = blocks[0].block(&func.dfg.value_lists);
        if t == blocks[1].block(&func.dfg.value_lists) {
            continue;
        }
        let Some((c, x, y)) = cond_icmp(func, arg).and_then(|i| norm_icmp(func, i)) else {
            continue;
        };
        fact.insert(b, (if b == t { c } else { c.complement() }, x, y));
    }
    fact
}

/// Index constants known on entry to blocks whose only predecessor is a
/// `br_table` listing them once (not as default): `idx == k`, and also
/// `x == k` when `idx = ireduce x` and `x` is a `uextend` no wider than `idx`.
fn table_facts(func: &Function, cfg: &ControlFlowGraph) -> FxHashMap<Block, Vec<(Value, u64)>> {
    let mut m: FxHashMap<Block, Vec<(Value, u64)>> = FxHashMap::default();
    let pool = &func.dfg.value_lists;
    for b in func.layout.blocks() {
        let mut preds = cfg.pred_iter(b);
        let (Some(p), None) = (preds.next(), preds.next()) else {
            continue;
        };
        if p.block == b {
            continue;
        }
        let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[p.inst] else {
            continue;
        };
        let jt = &func.dfg.jump_tables[table];
        if jt.default_block().block(pool) == b {
            continue;
        }
        let mut hits = jt
            .as_slice()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.block(pool) == b);
        let (Some((k, _)), None) = (hits.next(), hits.next()) else {
            continue;
        };
        let arg = func.dfg.resolve_aliases(arg);
        let mut v = vec![(arg, k as u64)];
        if let Some(i) = func.dfg.value_def(arg).inst()
            && let InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg: x,
            } = func.dfg.insts[i]
        {
            let x = func.dfg.resolve_aliases(x);
            if let Some(j) = func.dfg.value_def(x).inst()
                && let InstructionData::Unary {
                    opcode: Opcode::Uextend,
                    arg: n,
                } = func.dfg.insts[j]
                && bits(func, n)
                    .zip(bits(func, arg))
                    .is_some_and(|(n, a)| n <= a)
            {
                v.push((x, k as u64));
            }
        }
        m.insert(b, v);
    }
    m
}

/// Facts holding throughout `b`: those of `b` and its dominators.
fn facts_at(domtree: &DominatorTree, fact: &FxHashMap<Block, Cmp>, b: Block) -> Vec<Cmp> {
    let mut fs = Vec::new();
    if fact.is_empty() {
        return fs;
    }
    let mut cur = Some(b);
    for _ in 0..64 {
        let Some(c) = cur else { break };
        if let Some(&f) = fact.get(&c) {
            fs.push(f);
        }
        cur = domtree.idom(c);
    }
    fs
}

/// Fold icmps decided by a dominating branch: a block whose only predecessor
/// ends in `brif c` knows `c` (or its complement), as do the blocks it
/// dominates; equal operands or a constant against a known range decide it.
/// Returns the icmps folded.
pub fn fold_dominated_conds(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let fact = edge_facts(func, &cfg);
    let tfact = table_facts(func, &cfg);
    if fact.is_empty() && tfact.is_empty() {
        return 0;
    }
    let mut folds = Vec::new();
    for b in func.layout.blocks() {
        let fs = facts_at(&domtree, &fact, b);
        let mut env: FxHashMap<Value, u64> = FxHashMap::default();
        let mut cur = Some(b);
        for _ in 0..64 {
            let Some(c) = cur else { break };
            for &(v, k) in tfact.get(&c).into_iter().flatten() {
                env.insert(v, k);
            }
            cur = domtree.idom(c);
        }
        if fs.is_empty() && env.is_empty() {
            continue;
        }
        // `x == y` facts let a query on `y` be decided through `x`.
        let eqs: Vec<(Value, Value)> = fs
            .iter()
            .filter(|f| f.0 == IntCC::Equal)
            .map(|f| (f.1, f.2))
            .collect();
        let alts = |v: Value| {
            let mut a = vec![v];
            for &(x, y) in &eqs {
                if x == v {
                    a.push(y);
                } else if y == v {
                    a.push(x);
                }
            }
            a
        };
        let decide = |q: Cmp| -> Option<bool> {
            if let Some(r) = fs.iter().find_map(|&f| implied(f, q)) {
                return Some(r);
            }
            let w = bits(func, q.1)?;
            let (ka, kb) = (known(func, &env, q.1), known(func, &env, q.2));
            if let (Some(a), Some(b)) = (ka, kb) {
                return Some(icmp(q.0, a, b, w));
            }
            range_cmp(
                q.0,
                ka,
                kb,
                urange(func, &fs, q.1, 0),
                urange(func, &fs, q.2, 0),
                w,
            )
        };
        for i in func.layout.block_insts(b) {
            let Some(q) = norm_icmp(func, i) else {
                continue;
            };
            let r = alts(q.1)
                .into_iter()
                .flat_map(|a| alts(q.2).into_iter().map(move |b| (q.0, a, b)))
                .find_map(decide);
            if let Some(k) = r {
                folds.push((i, k));
            }
        }
    }
    for &(i, k) in &folds {
        let ty = func.dfg.value_type(func.dfg.first_result(i));
        func.replace(i).iconst(ty, i64::from(k));
    }
    folds.len()
}

/// Block params whose every incoming `jump`/`brif` arg is one value `v` (or
/// the param itself) become aliases of `v`. Threading's SSA repair creates
/// these, and Cranelift only removes them after our condition folding ran, so
/// the same value seen through two params would not match a dominating test.
pub fn remove_trivial_params(func: &mut Function) -> usize {
    #[derive(Clone, Copy, PartialEq)]
    enum In {
        Unset,
        One(Value),
        Many,
    }
    let entry = func.layout.entry_block();
    let mut inc: FxHashMap<Block, Vec<In>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let fixed = matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif);
        for bc in
            func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            let s = bc.block(&func.dfg.value_lists);
            let params = func.dfg.block_params(s);
            let st = inc
                .entry(s)
                .or_insert_with(|| vec![In::Unset; params.len()]);
            if !fixed {
                st.iter_mut().for_each(|x| *x = In::Many);
                continue;
            }
            for (i, a) in bc.args(&func.dfg.value_lists).enumerate().take(st.len()) {
                let new = match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        if v == params[i] {
                            continue;
                        }
                        In::One(v)
                    }
                    _ => In::Many,
                };
                st[i] = match (st[i], new) {
                    (In::Unset, x) => x,
                    (In::One(a), In::One(b)) if a == b => In::One(a),
                    _ => In::Many,
                };
            }
        }
    }
    let def_block = |func: &Function, v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    let mut rm: Vec<(Block, usize, Value, Value)> = Vec::new();
    for (&b, st) in &inc {
        if Some(b) == entry {
            continue;
        }
        let ps = func.dfg.block_params(b);
        for (i, s) in st.iter().enumerate() {
            if let In::One(v) = *s
                && def_block(func, v) != Some(b)
            {
                rm.push((b, i, ps[i], v));
            }
        }
    }
    // No alias chains between params removed together (avoids alias loops).
    let gone: FxHashSet<Value> = rm.iter().map(|r| r.2).collect();
    rm.retain(|r| !gone.contains(&r.3));
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
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if !matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif) {
            continue;
        }
        let dfg = &mut func.dfg;
        for bc in
            dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if let Some(is) = idx.get(&bc.block(&dfg.value_lists)) {
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

/// Thread until nothing changes (bounded); returns the edges retargeted.
pub fn run(
    func: &mut Function,
    loads: &FxHashSet<Value>,
    derived: &FxHashMap<Value, Value>,
) -> usize {
    merge_chains(func);
    let domcond = crate::pass_enabled("PLIRON_DOMCOND");
    let trivp = crate::pass_enabled("PLIRON_TRIVPARAM");
    let mut total = 0;
    for _ in 0..8 {
        if trivp {
            remove_trivial_params(func);
        }
        let nn = NonNull::new(func, loads, derived);
        let dc = if domcond {
            fold_dominated_conds(func)
        } else {
            0
        };
        let n = dc + bypass_forwarders(func) + round(func, &nn, domcond);
        total += n;
        if n == 0 {
            break;
        }
    }
    let nn = NonNull::new(func, loads, derived);
    total + fold_null_tests(func, &nn)
}

/// Turns `brif`/`br_table` on a compile-time constant into a `jump`. Later passes (load
/// forwarding) expose constants after threading ran, and Cranelift never folds branches,
/// so the dead successor's block arguments would otherwise stay live.
pub fn fold_const_branches(func: &mut Function) -> usize {
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for _ in 0..2 {
        for &b in &blocks {
            for i in func.layout.block_insts(b) {
                if func.dfg.inst_results(i).len() == 1
                    && let Some(c) = eval(func, &env, &|_| false, &|_| None, i)
                {
                    env.insert(func.dfg.first_result(i), c);
                }
            }
        }
    }
    let mut n = 0;
    for &b in &blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let dest = match func.dfg.insts[t] {
            InstructionData::Brif { arg, blocks, .. } => {
                let arg = func.dfg.resolve_aliases(arg);
                known(func, &env, arg).map(|c| blocks[usize::from(c == 0)])
            }
            InstructionData::BranchTable { arg, table, .. } => {
                let arg = func.dfg.resolve_aliases(arg);
                known(func, &env, arg).map(|c| {
                    let jt = &func.dfg.jump_tables[table];
                    usize::try_from(c)
                        .ok()
                        .and_then(|c| jt.as_slice().get(c).copied())
                        .unwrap_or(jt.default_block())
                })
            }
            _ => None,
        };
        if let Some(d) = dest {
            func.dfg.insts[t] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination: d,
            };
            n += 1;
        }
    }
    n
}

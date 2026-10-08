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
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
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
fn eval(func: &Function, env: &FxHashMap<Value, u64>, inst: Inst) -> Option<u64> {
    let res = func.dfg.first_result(inst);
    let w = bits(func, res)?;
    let r = match func.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits() as u64,
        InstructionData::IntCompare { cond, args, .. } => {
            let aw = bits(func, args[0])?;
            let (a, b) = (known(func, env, args[0])?, known(func, env, args[1])?);
            icmp(cond, a, b, aw) as u64
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

/// Whether a block in `uses` is reachable from `s` without passing through `b`
/// (following both the CFG and edges added earlier this round). If so, threading
/// an edge to `s` would let a path bypass `b`'s definitions. Gives up (true) on
/// very large walks.
fn bypasses(
    cfg: &ControlFlowGraph,
    extra: &FxHashMap<Block, Vec<Block>>,
    b: Block,
    s: Block,
    uses: &FxHashSet<Block>,
) -> bool {
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut work = vec![s];
    while let Some(x) = work.pop() {
        if x == b || !seen.insert(x) {
            continue;
        }
        if uses.contains(&x) || seen.len() > MAX_WALK {
            return true;
        }
        work.extend(cfg.succ_iter(x));
        if let Some(e) = extra.get(&x) {
            work.extend(e.iter().copied());
        }
    }
    false
}

/// Thread one round; returns the number of edges retargeted.
fn round(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let users = users(func);
    let none = FxHashSet::default();
    let mut extra: FxHashMap<Block, Vec<Block>> = FxHashMap::default();
    let entry = func.layout.entry_block();
    let mut edits: Vec<(Inst, usize, Block, Vec<Result<Value, (u64, Value)>>)> = Vec::new();
    let mut touched: FxHashSet<Inst> = FxHashSet::default();
    for b in func.layout.blocks() {
        if Some(b) == entry {
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
        for pred in cfg.pred_iter(b) {
            let (pinst, pblock) = (pred.inst, pred.block);
            if pblock == b || touched.contains(&pinst) {
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
                let mut env: FxHashMap<Value, u64> = FxHashMap::default();
                let mut argv: FxHashMap<Value, Value> = FxHashMap::default();
                let mut ok = args.len() == params.len();
                for (&p, a) in params.iter().zip(&args) {
                    let BlockArg::Value(v) = *a else {
                        ok = false;
                        break;
                    };
                    let v = func.dfg.resolve_aliases(v);
                    argv.insert(p, v);
                    if let Some(c) = known(func, &env, v) {
                        env.insert(p, c);
                    }
                }
                if !ok {
                    continue;
                }
                for &i in body {
                    if let Some(c) = eval(func, &env, i) {
                        env.insert(func.dfg.first_result(i), c);
                    }
                }
                let Some(c) = known(func, &env, cond) else {
                    continue;
                };
                let t = targets[if c != 0 { 0 } else { 1 }];
                let s = t.block(&func.dfg.value_lists);
                if s == b || bypasses(&cfg, &extra, b, s, users.get(&b).unwrap_or(&none)) {
                    continue;
                }
                let mut new = Vec::new();
                for a in t.args(&func.dfg.value_lists) {
                    let BlockArg::Value(v) = a else {
                        ok = false;
                        break;
                    };
                    let v = func.dfg.resolve_aliases(v);
                    if let Some(&pv) = argv.get(&v) {
                        new.push(Ok(pv));
                    } else if body_vals.contains(&v) {
                        match env.get(&v) {
                            Some(&k) => new.push(Err((k, v))),
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    } else {
                        new.push(Ok(v));
                    }
                }
                if ok {
                    touched.insert(pinst);
                    extra.entry(pblock).or_default().push(s);
                    edits.push((pinst, di, s, new));
                    break;
                }
            }
        }
    }
    let n = edits.len();
    for (pinst, di, s, new) in edits {
        let mut args = Vec::with_capacity(new.len());
        for a in new {
            args.push(BlockArg::Value(match a {
                Ok(v) => v,
                Err((k, like)) => {
                    let ty = func.dfg.value_type(like);
                    let mut cur = FuncCursor::new(func).at_inst(pinst);
                    cur.ins().iconst(ty, k as i64)
                }
            }));
        }
        let bc = BlockCall::new(s, args, &mut func.dfg.value_lists);
        let dfg = &mut func.dfg;
        dfg.insts[pinst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
            [di] = bc;
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
pub fn run(func: &mut Function) -> usize {
    merge_chains(func);
    let mut total = 0;
    for _ in 0..4 {
        let n = round(func);
        total += n;
        if n == 0 {
            break;
        }
    }
    total
}

//! Switch to arithmetic (the linear case of LLVM `simplifycfg`'s
//! switch-to-lookup-table). A `br_table` whose live entries all reach one
//! block, passing the same values or constants linear in the index, becomes
//! `idx * s + b` and one bounds test, or none when the default is
//! `unreachable`. Entries that only reach `unreachable` are don't-cares. A
//! common `return` (tail duplication copies it into every case) counts as one
//! target too.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, TrapCode,
    Type, Value, ValueDef,
};
use rustc_data_structures::fx::FxHashMap;

use crate::unreach::UNREACHABLE;

#[derive(Clone, Copy, PartialEq)]
enum Arg {
    Same(Value),
    Const(u64),
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Block(Block),
    Return,
}

#[derive(Clone, Copy)]
enum Lin {
    Same(Value),
    Map { s: u64, b: u64 },
}

fn unreachable_block(func: &Function, b: Block) -> bool {
    let mut it = func.layout.block_insts(b);
    match (it.next(), it.next()) {
        (Some(i), None) => matches!(
            func.dfg.insts[i],
            InstructionData::Trap { opcode: Opcode::Trap, code }
                if code == TrapCode::unwrap_user(UNREACHABLE)
        ),
        _ => false,
    }
}

fn iconst(func: &Function, v: Value) -> Option<u64> {
    let ValueDef::Result(i, 0) = func.dfg.value_def(v) else {
        return None;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits() as u64),
        _ => None,
    }
}

fn values(func: &Function, bc: BlockCall) -> Option<Vec<Value>> {
    bc.args(&func.dfg.value_lists)
        .map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect()
}

/// Where entry `bc` of `src`'s table ends up: through its target when that
/// holds only `iconst`s and a `jump` and only `src` reaches it. `Some(None)`
/// for an entry that only reaches `unreachable`.
fn resolve(
    func: &Function,
    cfg: &ControlFlowGraph,
    src: Block,
    bc: BlockCall,
) -> Option<Option<(Target, Vec<(Arg, Type)>)>> {
    let t = bc.block(&func.dfg.value_lists);
    let direct = values(func, bc)?;
    if direct.is_empty() && unreachable_block(func, t) {
        return Some(None);
    }
    let arg = |v: Value, local: &FxHashMap<Value, u64>| {
        let a = local
            .get(&v)
            .copied()
            .or_else(|| iconst(func, v))
            .map_or(Arg::Same(v), Arg::Const);
        (a, func.dfg.value_type(v))
    };
    if direct.is_empty()
        && func.dfg.num_block_params(t) == 0
        && cfg.pred_iter(t).all(|p| p.block == src)
    {
        let mut local = FxHashMap::default();
        for i in func.layout.block_insts(t) {
            match func.dfg.insts[i] {
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm,
                } => {
                    local.insert(func.dfg.first_result(i), imm.bits() as u64);
                }
                InstructionData::Jump { destination, .. } => {
                    let vs = values(func, destination)?;
                    let d = destination.block(&func.dfg.value_lists);
                    let args = vs.into_iter().map(|v| arg(v, &local)).collect();
                    return Some(Some((Target::Block(d), args)));
                }
                InstructionData::MultiAry {
                    opcode: Opcode::Return,
                    ..
                } => {
                    let args = func
                        .dfg
                        .inst_args(i)
                        .iter()
                        .map(|&v| arg(func.dfg.resolve_aliases(v), &local))
                        .collect();
                    return Some(Some((Target::Return, args)));
                }
                _ => break,
            }
        }
    }
    let none = FxHashMap::default();
    Some(Some((
        Target::Block(t),
        direct.into_iter().map(|v| arg(v, &none)).collect(),
    )))
}

fn plan(
    func: &Function,
    cfg: &ControlFlowGraph,
    src: Block,
    entries: &[BlockCall],
) -> Option<(Target, Vec<(Lin, Type)>)> {
    let mut care: Vec<(u64, Target, Vec<(Arg, Type)>)> = Vec::new();
    for (i, &bc) in entries.iter().enumerate() {
        if let Some((b, a)) = resolve(func, cfg, src, bc)? {
            care.push((i as u64, b, a));
        }
    }
    let (i0, dest, a0) = care.first()?.clone();
    if care
        .iter()
        .any(|(_, b, a)| *b != dest || a.len() != a0.len())
    {
        return None;
    }
    let mut out = Vec::new();
    for (k, &(first, ty)) in a0.iter().enumerate() {
        if care.iter().any(|c| c.2[k].1 != ty) {
            return None;
        }
        match first {
            Arg::Same(v) => {
                if care.iter().any(|c| c.2[k].0 != Arg::Same(v)) {
                    return None;
                }
                out.push((Lin::Same(v), ty));
            }
            Arg::Const(_) => {
                if !ty.is_int() || ty.bits() > 64 {
                    return None;
                }
                let w = ty.bits();
                let m = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
                let sx = |c: u64| ((c << (64 - w)) as i64 >> (64 - w)) as i128;
                let mut consts = Vec::new();
                for c in &care {
                    let Arg::Const(ci) = c.2[k].0 else {
                        return None;
                    };
                    consts.push((c.0, ci & m));
                }
                let c0 = consts[0].1;
                let s = match consts.get(1) {
                    Some(&(i1, c1)) => {
                        let (d, di) = (sx(c1) - sx(c0), (i1 - i0) as i128);
                        if d % di != 0 {
                            return None;
                        }
                        (d / di) as u64 & m
                    }
                    None => 0,
                };
                let b = c0.wrapping_sub(i0.wrapping_mul(s)) & m;
                if consts
                    .iter()
                    .any(|&(i, c)| b.wrapping_add(i.wrapping_mul(s)) & m != c)
                {
                    return None;
                }
                out.push((Lin::Map { s, b }, ty));
            }
        }
    }
    Some((dest, out))
}

fn rewrite(
    func: &mut Function,
    term: Inst,
    idx: Value,
    len: usize,
    dest: Target,
    lins: &[(Lin, Type)],
    dflt: BlockCall,
) {
    let dflt_block = dflt.block(&func.dfg.value_lists);
    let dflt_args: Vec<BlockArg> = dflt.args(&func.dfg.value_lists).collect();
    let dead_default = dflt_args.is_empty() && unreachable_block(func, dflt_block);
    let ity = func.dfg.value_type(idx);
    let mut pos = FuncCursor::new(func).at_inst(term);
    let mut args = Vec::new();
    for &(l, ty) in lins {
        let v = match l {
            Lin::Same(v) => v,
            Lin::Map { s, b } => {
                if s == 0 {
                    pos.ins().iconst(ty, b as i64)
                } else {
                    let x = if ity.bits() < ty.bits() {
                        pos.ins().uextend(ty, idx)
                    } else if ity.bits() > ty.bits() {
                        pos.ins().ireduce(ty, idx)
                    } else {
                        idx
                    };
                    let x = if s == 1 {
                        x
                    } else {
                        pos.ins().imul_imm_u(x, s as i64)
                    };
                    if b == 0 {
                        x
                    } else {
                        pos.ins().iadd_imm_u(x, b as i64)
                    }
                }
            }
        };
        args.push(BlockArg::Value(v));
    }
    let dest = match dest {
        Target::Block(d) => d,
        Target::Return if dead_default => {
            let vals: Vec<Value> = args.iter().map(|a| a.as_value().unwrap()).collect();
            pos.func.replace(term).return_(&vals);
            return;
        }
        Target::Return => {
            let r = pos.func.dfg.make_block();
            pos.func.layout.append_block(r);
            let ps: Vec<Value> = lins
                .iter()
                .map(|&(_, ty)| pos.func.dfg.append_block_param(r, ty))
                .collect();
            let mut rp = FuncCursor::new(pos.func).at_bottom(r);
            rp.ins().return_(&ps);
            pos = FuncCursor::new(rp.func).at_inst(term);
            r
        }
    };
    if dead_default {
        pos.func.replace(term).jump(dest, &args);
    } else {
        let c = pos
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThan, idx, len as i64);
        pos.func
            .replace(term)
            .brif(c, dest, &args, dflt_block, &dflt_args);
    }
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let mut todo = Vec::new();
    for src in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(src) else {
            continue;
        };
        let InstructionData::BranchTable { arg, table, .. } = func.dfg.insts[term] else {
            continue;
        };
        let jt = &func.dfg.jump_tables[table];
        let entries = jt.as_slice().to_vec();
        if entries.len() < 2 {
            continue;
        }
        if let Some((dest, lins)) = plan(func, &cfg, src, &entries) {
            todo.push((term, arg, entries.len(), dest, lins, jt.default_block()));
        }
    }
    let n = todo.len();
    for (term, idx, len, dest, lins, dflt) in todo {
        rewrite(func, term, idx, len, dest, &lins, dflt);
    }
    n
}

//! Sink a merged param's pure use-chain into the incoming edges.
//!
//! `bool` flags merged through a block param (`is_alphabetic`'s
//! `block478(v415: i8)` fed by `1`/`0`/call-return edges) keep the flag
//! alive across the whole check region; regalloc then spills it and the
//! merge block pays load+narrow+op per element. The chain
//! `uextend → band 1 → iadd count` is pure and single-use, so each edge
//! can evaluate it instead: the constant arms fold to `iadd count, 1`
//! (one inst, no flag at all), the call arm `iadd count, ret0` — the
//! flag never exists as a spilled entity.
//!
//! The chain tail stays usable inside the block by becoming a param of
//! the same type: `b` loses the flag param and gains the result param.
//! Non-chain operands must be params of `b` (substituted per-edge) or
//! defs dominating `b`. Edges whose needed args are `try_call`
//! pseudo-values (`ret0` can't be read before the call) get a
//! trampoline block between pred and `b` that forwards the args and
//! evaluates the chain on the edge.
//!
//! `PLIRON_PARAMSINK=0` disables it.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstructionData, Opcode, Type, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// Every block `v` is used in (`inst_values` covers edge args too).
fn use_blocks(func: &Function) -> FxHashMap<Value, FxHashSet<Block>> {
    let mut m: FxHashMap<Value, FxHashSet<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                m.entry(func.dfg.resolve_aliases(v)).or_default().insert(b);
            }
        }
    }
    m
}

/// Rebuild a pure `data` with new operand values.
pub(crate) fn rebuild(data: &InstructionData, a: &[Value]) -> InstructionData {
    match *data {
        InstructionData::Binary { opcode, .. } => InstructionData::Binary {
            opcode,
            args: [a[0], a[1]],
        },
        InstructionData::Unary { opcode, .. } => InstructionData::Unary { opcode, arg: a[0] },
        InstructionData::IntCompare { opcode, cond, .. } => InstructionData::IntCompare {
            opcode,
            cond,
            args: [a[0], a[1]],
        },
        InstructionData::Ternary { opcode, .. } => InstructionData::Ternary {
            opcode,
            args: [a[0], a[1], a[2]],
        },
        other => other,
    }
}

/// The param's use-chain inside `b`: pure single-result insts linked
/// while each intermediate has no uses outside `b`. Returns the chain
/// and the tail value (which may be used widely in `b` — its uses take
/// the replacement param).
fn chain_of(
    func: &Function,
    uses: &FxHashMap<Value, FxHashSet<Block>>,
    b: Block,
    p: Value,
) -> Option<(Vec<Inst>, Value)> {
    let all_in_b = |v: Value| uses.get(&v).is_some_and(|s| s.len() == 1 && s.contains(&b));
    if !all_in_b(p) {
        return None;
    }
    let mut chain = Vec::new();
    let mut cur = p;
    loop {
        let mut it = func.layout.block_insts(b).filter(|&i| {
            func.dfg
                .inst_values(i)
                .any(|v| func.dfg.resolve_aliases(v) == cur)
        });
        let Some(ui) = it.next() else { break };
        if it.next().is_some() || !crate::jumpthread::pure_op(func, ui) {
            break;
        }
        let r = func.dfg.inst_results(ui)[0];
        chain.push(ui);
        cur = r;
        if chain.len() > 8 || !all_in_b(cur) {
            break;
        }
    }
    if chain.is_empty() || !all_in_b(cur) {
        return None;
    }
    Some((chain, cur))
}

#[derive(Clone, Copy)]
enum E {
    V(Value),
    K(u64, Type),
}

fn put(
    func: &mut Function,
    dst: Block,
    before: Option<Inst>,
    data: InstructionData,
    ty: Type,
) -> Value {
    let ni = func.dfg.make_inst(data);
    func.dfg.make_inst_results(ni, ty);
    match before {
        Some(t) => func.layout.insert_inst(ni, t),
        None => func.layout.append_inst(ni, dst),
    }
    func.dfg.first_result(ni)
}

/// Sink single-use chains into their preds; returns (params sunk,
/// dropped param → its replacement param). The new param carries the
/// sunk value, so it equals the dropped param — the pipeline carries
/// the old param's facts across.
pub fn run(func: &mut Function) -> (usize, Vec<(Value, Value)>) {
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    let uses = use_blocks(func);
    let mut edges: FxHashMap<Block, Vec<(Block, Inst, usize)>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let pb = func.layout.inst_block(i).unwrap();
            for (d, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                edges
                    .entry(bc.block(&func.dfg.value_lists))
                    .or_default()
                    .push((pb, i, d));
            }
        }
    }
    let nz: &dyn Fn(Value) -> bool = &|_| false;
    let rg: &dyn Fn(Value) -> Option<(u64, u64)> = &|_| None;
    let mut total = 0;
    let mut moved: Vec<(Value, Value)> = Vec::new();
    'blocks: for (&b, es) in &edges {
        let ps = func.dfg.block_params(b).to_vec();
        if ps.is_empty()
            || es.is_empty()
            || !es.iter().all(|&(_, i, d)| {
                func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d]
                    .args(&func.dfg.value_lists)
                    .len()
                    == ps.len()
            })
        {
            continue;
        }
        for (ix, &p) in ps.iter().enumerate() {
            let Some((chain, tail)) = chain_of(func, &uses, b, p) else {
                continue;
            };
            if chain.len() * (es.len() - 1) > 24 {
                continue;
            }
            // No edge arg may be `p` itself: its slot drops, so a
            // self-edge passing `p` (even at an unrelated slot) would
            // leave a dangling use in the emitted code or kept args.
            if es.iter().any(|&(_, i, d)| {
                func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d]
                    .args(&func.dfg.value_lists)
                    .any(|a| {
                        a.as_value()
                            .is_some_and(|x| func.dfg.resolve_aliases(x) == p)
                    })
            }) {
                continue;
            }
            // Param slots (besides ix) the chain reads; every needed
            // slot must carry a plain Value for in-pred sinking.
            let side: Vec<usize> = chain
                .iter()
                .flat_map(|&ci| func.dfg.inst_args(ci).to_vec())
                .filter_map(|v| {
                    let v = func.dfg.resolve_aliases(v);
                    (v != p).then(|| ps.iter().position(|&q| q == v)).flatten()
                })
                .collect();
            // Non-param operands must strictly dominate `b` — then they
            // lie on every path to `b` and stay usable on every edge
            // (a def inside `b` can't be reproduced in the preds).
            let dom_ok = chain.iter().all(|&ci| {
                func.dfg.inst_args(ci).to_vec().iter().all(|&v| {
                    let v = func.dfg.resolve_aliases(v);
                    if v == p
                        || ps.contains(&v)
                        || chain.iter().any(|&j| func.dfg.inst_results(j).contains(&v))
                    {
                        return true;
                    }
                    let db = match func.dfg.value_def(v) {
                        ValueDef::Result(di, _) => func.layout.inst_block(di),
                        ValueDef::Param(db, _) => Some(db),
                        ValueDef::Union(_, _) => None,
                    };
                    db.is_some_and(|db| db != b && dom.block_dominates(db, b))
                })
            });
            if !dom_ok {
                continue;
            }
            let mut ok = true;
            for &(pb, i, d) in es {
                let bc_args: Vec<BlockArg> = func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d]
                    .args(&func.dfg.value_lists)
                    .collect();
                let sinkable = std::iter::once(ix)
                    .chain(side.iter().copied())
                    .all(|j| matches!(bc_args.get(j), Some(BlockArg::Value(_))));
                // Where the chain goes and what each param slot reads.
                let dst;
                let before;
                let mut tps: Vec<Value> = Vec::new();
                if sinkable {
                    dst = pb;
                    before = func.layout.last_inst(pb);
                } else {
                    let tb = func.dfg.make_block();
                    func.layout.insert_block_after(tb, pb);
                    for &pj in &ps {
                        let ty = func.dfg.value_type(pj);
                        tps.push(func.dfg.append_block_param(tb, ty));
                    }
                    let dfg = &mut func.dfg;
                    dfg.insts[i]
                        .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d]
                        .set_block(tb, &mut dfg.value_lists);
                    dst = tb;
                    before = None;
                }
                let get = |j: usize| -> Option<Value> {
                    if sinkable {
                        bc_args[j].as_value()
                    } else {
                        Some(tps[j])
                    }
                };
                // Per-edge const env: literal args bind their params.
                let mut env: FxHashMap<Value, u64> = FxHashMap::default();
                for (j, &pj) in ps.iter().enumerate() {
                    let Some(x) = get(j) else { continue };
                    let x = func.dfg.resolve_aliases(x);
                    if let Some(k) = crate::jumpthread::known(func, &env, x) {
                        env.insert(pj, k);
                    }
                }
                let mut link_out: Vec<E> = Vec::with_capacity(chain.len());
                for (li, &ci) in chain.iter().enumerate() {
                    let data = func.dfg.insts[ci];
                    let ty = func.dfg.value_type(func.dfg.first_result(ci));
                    if let Some(k) = crate::jumpthread::eval(func, &env, nz, rg, ci) {
                        env.insert(func.dfg.first_result(ci), k);
                        link_out.push(E::K(k, ty));
                        continue;
                    }
                    let mut args: Vec<E> = Vec::new();
                    for &v in func.dfg.inst_args(ci).to_vec().iter() {
                        let v = func.dfg.resolve_aliases(v);
                        let vty = func.dfg.value_type(v);
                        if let Some(&k) = env.get(&v) {
                            args.push(E::K(k, vty));
                            continue;
                        }
                        if v == p || ps.contains(&v) {
                            let j = if v == p {
                                ix
                            } else {
                                ps.iter().position(|&q| q == v).unwrap()
                            };
                            let Some(x) = get(j) else {
                                ok = false;
                                break;
                            };
                            args.push(E::V(func.dfg.resolve_aliases(x)));
                        } else if let Some(li2) = chain[..li]
                            .iter()
                            .position(|&j| func.dfg.inst_results(j).contains(&v))
                        {
                            args.push(link_out[li2]);
                        } else {
                            args.push(E::V(v));
                        }
                    }
                    if !ok {
                        break;
                    }
                    let vs: Vec<Value> = args
                        .iter()
                        .map(|a| match a {
                            E::V(v) => *v,
                            &E::K(k, aty) => put(
                                func,
                                dst,
                                before,
                                InstructionData::UnaryImm {
                                    opcode: Opcode::Iconst,
                                    imm: (k as i64).into(),
                                },
                                aty,
                            ),
                        })
                        .collect();
                    let nv = put(func, dst, before, rebuild(&data, &vs), ty);
                    link_out.push(E::V(nv));
                }
                if !ok {
                    break;
                }
                let tv = match *link_out.last().unwrap() {
                    E::V(v) => v,
                    E::K(k, ty) => put(
                        func,
                        dst,
                        before,
                        InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            imm: (k as i64).into(),
                        },
                        ty,
                    ),
                };
                // Finalize: in-pred edges trade slot ix for the tail
                // value; trampolines close with a fresh jump.
                if sinkable {
                    let dfg = &mut func.dfg;
                    let bc = &mut dfg.insts[i]
                        .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
                    bc.remove(ix, &mut dfg.value_lists);
                    bc.append_argument(tv, &mut dfg.value_lists);
                } else {
                    let mut ja: Vec<BlockArg> = tps
                        .iter()
                        .enumerate()
                        .filter(|&(j, _)| j != ix)
                        .map(|(_, &q)| BlockArg::Value(q))
                        .collect();
                    ja.push(BlockArg::Value(tv));
                    let dest = BlockCall::new(b, ja, &mut func.dfg.value_lists);
                    let ni = func.dfg.make_inst(InstructionData::Jump {
                        opcode: Opcode::Jump,
                        destination: dest,
                    });
                    func.layout.append_inst(ni, dst);
                }
            }
            if !ok {
                continue 'blocks;
            }
            // `tail` becomes a param; uses inside `b` rebind, the chain
            // and `p` drop.
            func.dfg.resolve_all_aliases();
            let tty = func.dfg.value_type(tail);
            let np = func.dfg.append_block_param(b, tty);
            for i in func.layout.block_insts(b).collect::<Vec<_>>() {
                if chain.contains(&i) {
                    continue;
                }
                let dfg = &mut func.dfg;
                let mut data = dfg.insts[i];
                data.map_values(
                    &mut dfg.value_lists,
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                    |x| if x == tail { np } else { x },
                );
                dfg.insts[i] = data;
            }
            for &ci in &chain {
                func.dfg.clear_results(ci);
                func.layout.remove_inst(ci);
            }
            func.dfg.remove_block_param(p);
            moved.push((p, np));
            total += 1;
            continue 'blocks;
        }
    }
    (total, moved)
}

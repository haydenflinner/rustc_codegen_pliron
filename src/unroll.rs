//! Full unrolling of small loops whose control flow is decided by constants
//! (`PLIRON_UNROLL`). From a loop's single entry edge, the header's incoming
//! constants are propagated through the loop and each branch on the path is
//! evaluated. If every branch is decided and the path leaves the loop within
//! `MAX_ITERS` header visits, the whole trip becomes one straight-line block
//! (results that evaluate to constants become `iconst`s) and the entry edge
//! jumps to it; the original loop is left unreachable. Fixed-size copies such
//! as md5's `[u32; 16]` input loop then store to constant offsets, which
//! `loadfwd` forwards and slot DSE deletes.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, StackSlot,
    Value,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::jumpthread::{NonNull, eval, known};

const MAX_ITERS: usize = 32;
const MAX_INSTS: usize = 400;
const MAX_LOOPS: usize = 8;

pub fn run(
    func: &mut Function,
    loads: &FxHashSet<Value>,
    derived: &FxHashMap<Value, Value>,
) -> usize {
    let mut tried: FxHashSet<Block> = FxHashSet::default();
    let mut n = 0;
    'outer: while n < MAX_LOOPS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let nn = NonNull::new(func, loads, derived);
        let blocks: Vec<Block> = func.layout.blocks().collect();
        for h in blocks {
            if !tried.insert(h) {
                continue;
            }
            let Some((pred, pinst, body, entry, used_out)) = candidate(func, &cfg, &dt, h) else {
                continue;
            };
            if std::env::var_os("PLIRON_UNROLL_DEBUG").is_some_and(|v| v == "2") {
                eprintln!("unroll: candidate {h} ({} blocks)", body.len());
            }
            match trip(func, &nn, &body, h, entry.clone(), None) {
                Some((_, _, map)) if used_out.iter().all(|v| map.contains_key(v)) => {}
                _ => continue,
            }
            let nb = func.dfg.make_block();
            func.layout.insert_block_after(nb, pred);
            let (exit, out, map) = trip(func, &nn, &body, h, entry, Some(nb)).unwrap();
            // Uses past the loop see the last trip's definitions (the loop is
            // only entered via `pred`, so `nb` dominates them now).
            if !used_out.is_empty() {
                let outside: Vec<Block> = func
                    .layout
                    .blocks()
                    .filter(|b| !body.contains(b) && *b != nb)
                    .collect();
                for b in outside {
                    let insts: Vec<Inst> = func.layout.block_insts(b).collect();
                    for i in insts {
                        let old: Vec<Value> = func.dfg.inst_values(i).collect();
                        let new: Vec<Value> = old
                            .iter()
                            .map(|&v| {
                                let r = func.dfg.resolve_aliases(v);
                                if used_out.contains(&r) { map[&r] } else { v }
                            })
                            .collect();
                        if new != old {
                            func.dfg.overwrite_inst_values(i, new.into_iter());
                        }
                    }
                }
            }
            let out: Vec<BlockArg> = out.into_iter().map(BlockArg::Value).collect();
            FuncCursor::new(func).at_bottom(nb).ins().jump(exit, &out);
            let dfg = &mut func.dfg;
            for bc in dfg.insts[pinst]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
            {
                if bc.block(&dfg.value_lists) == h {
                    *bc = BlockCall::new(nb, std::iter::empty(), &mut dfg.value_lists);
                }
            }
            n += 1;
            continue 'outer;
        }
        break;
    }
    n
}

/// Natural loop at `h` with exactly one entry edge (a `jump`/`brif` from
/// outside): (pred, its branch, loop blocks, entry arguments, loop values used
/// outside the loop).
fn candidate(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    h: Block,
) -> Option<(Block, Inst, FxHashSet<Block>, Vec<Value>, FxHashSet<Value>)> {
    if !dt.is_reachable(h) {
        return None;
    }
    let latches: Vec<Block> = cfg
        .pred_iter(h)
        .map(|p| p.block)
        .filter(|&p| dt.is_reachable(p) && dt.block_dominates(h, p))
        .collect();
    if latches.is_empty() {
        return None;
    }
    let mut body: FxHashSet<Block> = FxHashSet::default();
    body.insert(h);
    let mut work = latches;
    while let Some(b) = work.pop() {
        if body.insert(b) {
            work.extend(
                cfg.pred_iter(b)
                    .map(|p| p.block)
                    .filter(|&p| dt.is_reachable(p)),
            );
        }
    }
    let outside: Vec<_> = cfg
        .pred_iter(h)
        .filter(|p| !body.contains(&p.block))
        .collect();
    let [ref e] = outside[..] else {
        return None;
    };
    if !matches!(
        func.dfg.insts[e.inst],
        InstructionData::Jump { .. } | InstructionData::Brif { .. }
    ) {
        return None;
    }
    let pool = &func.dfg.value_lists;
    let calls: Vec<BlockCall> = func.dfg.insts[e.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .iter()
        .filter(|bc| bc.block(pool) == h)
        .copied()
        .collect();
    let [bc] = calls[..] else {
        return None;
    };
    let mut entry = Vec::new();
    for a in bc.args(pool) {
        let BlockArg::Value(v) = a else {
            return None;
        };
        entry.push(func.dfg.resolve_aliases(v));
    }
    let mut defs: FxHashSet<Value> = FxHashSet::default();
    for &b in &body {
        defs.extend(func.dfg.block_params(b).iter().copied());
        for i in func.layout.block_insts(b) {
            defs.extend(func.dfg.inst_results(i).iter().copied());
        }
    }
    let mut used_out = FxHashSet::default();
    for b in func.layout.blocks() {
        if body.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            used_out.extend(
                func.dfg
                    .inst_values(i)
                    .map(|v| func.dfg.resolve_aliases(v))
                    .filter(|v| defs.contains(v)),
            );
        }
    }
    Some((e.block, e.inst, body, entry, used_out))
}

/// Taken successor of terminator `t` and its arguments, if `env` decides it.
fn decide(func: &Function, env: &FxHashMap<Value, u64>, t: Inst) -> Option<(Block, Vec<Value>)> {
    let bc = match func.dfg.insts[t] {
        InstructionData::Jump { destination, .. } => destination,
        InstructionData::Brif { arg, blocks, .. } => {
            blocks[if known(func, env, arg)? != 0 { 0 } else { 1 }]
        }
        InstructionData::BranchTable { arg, table, .. } => {
            let c = known(func, env, arg)?;
            let jt = &func.dfg.jump_tables[table];
            usize::try_from(c)
                .ok()
                .and_then(|c| jt.as_slice().get(c).copied())
                .unwrap_or(jt.default_block())
        }
        _ => return None,
    };
    let pool = &func.dfg.value_lists;
    let mut args = Vec::new();
    for a in bc.args(pool) {
        let BlockArg::Value(v) = a else {
            return None;
        };
        args.push(func.dfg.resolve_aliases(v));
    }
    Some((bc.block(pool), args))
}

/// Walk one trip through the loop from its entry. With `nb`, also emit it
/// into `nb`. Returns the exit block and its (emitted) arguments.
fn trip(
    func: &mut Function,
    nn: &NonNull,
    body: &FxHashSet<Block>,
    h: Block,
    entry: Vec<Value>,
    nb: Option<Block>,
) -> Option<(Block, Vec<Value>, FxHashMap<Value, Value>)> {
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    // Constants defined before the loop that `known` alone can't see.
    for &b in body {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                if !env.contains_key(&v)
                    && let Some(c) = cval(func, body, v, 6)
                {
                    env.insert(v, c);
                }
            }
        }
    }
    for &a in &entry {
        if let Some(c) = cval(func, body, a, 6) {
            env.insert(a, c);
        }
    }
    // Block param -> the value it was bound to outside any param (for non-null facts).
    let mut src: FxHashMap<Value, Value> = FxHashMap::default();
    // Original value -> emitted value.
    let mut map: FxHashMap<Value, Value> = FxHashMap::default();
    // Stack-slot addresses of loop values in the current trip, the bytes last
    // stored to each slot byte, and the bytes making up emitted load results.
    let mut slots: FxHashMap<Value, (StackSlot, i64)> = FxHashMap::default();
    let mut mem: FxHashMap<(StackSlot, i64), Byte> = FxHashMap::default();
    let mut vbytes: FxHashMap<Value, Vec<Option<Byte>>> = FxHashMap::default();
    let (mut b, mut args) = (h, entry);
    let (mut visits, mut insts) = (0, 0);
    loop {
        if !body.contains(&b) {
            let out = args.iter().map(|v| *map.get(v).unwrap_or(v)).collect();
            return Some((b, out, map));
        }
        if b == h {
            visits += 1;
            if visits > MAX_ITERS {
                return why("iters");
            }
        }
        let params = func.dfg.block_params(b).to_vec();
        if params.len() != args.len() {
            return why("arity");
        }
        let bind: Vec<_> = args
            .iter()
            .map(|&a| {
                (
                    known(func, &env, a),
                    *map.get(&a).unwrap_or(&a),
                    *src.get(&a).unwrap_or(&a),
                    addr_of(func, &env, &slots, body, a),
                )
            })
            .collect();
        for (&p, (c, m, s, sa)) in params.iter().zip(bind) {
            match sa {
                Some(sa) => slots.insert(p, sa),
                None => slots.remove(&p),
            };
            match c {
                Some(c) => env.insert(p, c),
                None => env.remove(&p),
            };
            map.insert(p, m);
            src.insert(p, s);
        }
        let all: Vec<Inst> = func.layout.block_insts(b).collect();
        let (&t, rest) = all.split_last()?;
        for &i in rest {
            let op = func.dfg.insts[i].opcode();
            if op.is_branch() || op.is_terminator() || op.is_return() {
                return why("term in body");
            }
            let rs = func.dfg.inst_results(i).to_vec();
            if rs.len() == 1 {
                match addr_step(func, &env, &slots, body, i) {
                    Some(a) => slots.insert(rs[0], a),
                    None => slots.remove(&rs[0]),
                };
            }
            let mut lbytes = None;
            match func.dfg.insts[i] {
                InstructionData::Store {
                    opcode,
                    args,
                    offset,
                    ..
                } => {
                    let n = match opcode {
                        Opcode::Store => func.dfg.value_type(args[0]).bytes(),
                        Opcode::Istore8 => 1,
                        Opcode::Istore16 => 2,
                        Opcode::Istore32 => 4,
                        _ => 0,
                    } as usize;
                    match addr_of(func, &env, &slots, body, args[1]) {
                        Some((ss, o)) if n > 0 => {
                            let o = o.wrapping_add(i64::from(i32::from(offset)));
                            let val = func.dfg.resolve_aliases(args[0]);
                            let bytes: Vec<Option<Byte>> = match known(func, &env, val) {
                                Some(c) if n <= 8 => (0..n)
                                    .map(|k| Some(Byte::C((c >> (8 * k)) as u8)))
                                    .collect(),
                                _ => {
                                    let e = *map.get(&val).unwrap_or(&val);
                                    match vbytes.get(&e) {
                                        Some(b) if b.len() >= n => b[..n].to_vec(),
                                        _ => (0..n).map(|k| Some(Byte::V(e, k as u8))).collect(),
                                    }
                                }
                            };
                            for (k, b) in bytes.into_iter().enumerate() {
                                let key = (ss, o.wrapping_add(k as i64));
                                match b {
                                    Some(b) => mem.insert(key, b),
                                    None => mem.remove(&key),
                                };
                            }
                        }
                        _ => mem.clear(),
                    }
                }
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    ..
                } if rs.len() == 1 => {
                    if let Some((ss, o)) = addr_of(func, &env, &slots, body, arg) {
                        let o = o.wrapping_add(i64::from(i32::from(offset)));
                        let ty = func.dfg.value_type(rs[0]);
                        let n = ty.bytes() as usize;
                        let bytes: Vec<Option<Byte>> = (0..n)
                            .map(|k| mem.get(&(ss, o.wrapping_add(k as i64))).copied())
                            .collect();
                        let consts: Option<Vec<u8>> = bytes
                            .iter()
                            .map(|b| match b {
                                Some(Byte::C(c)) => Some(*c),
                                _ => None,
                            })
                            .collect();
                        if let Some(cs) = consts.filter(|_| ty.is_int() && n <= 8) {
                            let c = cs
                                .iter()
                                .enumerate()
                                .fold(0u64, |a, (k, &b)| a | (u64::from(b) << (8 * k)));
                            env.insert(rs[0], c);
                            let v = match nb {
                                Some(nb) => FuncCursor::new(func)
                                    .at_bottom(nb)
                                    .ins()
                                    .iconst(ty, c as i64),
                                None => rs[0],
                            };
                            map.insert(rs[0], v);
                            continue;
                        }
                        if let Some(Some(Byte::V(e, 0))) = bytes.first()
                            && func.dfg.value_type(*e) == ty
                            && bytes
                                .iter()
                                .enumerate()
                                .all(|(k, b)| *b == Some(Byte::V(*e, k as u8)))
                        {
                            env.remove(&rs[0]);
                            map.insert(rs[0], *e);
                            continue;
                        }
                        if bytes.iter().any(Option::is_some) {
                            lbytes = Some(bytes);
                        }
                    }
                }
                _ => {
                    let op = func.dfg.insts[i].opcode();
                    if op.is_call() || op.can_store() || op.other_side_effects() {
                        mem.clear();
                    }
                }
            }
            let c = if rs.len() == 1 {
                let f: &Function = func;
                eval(
                    f,
                    &env,
                    &|x| nn.is(f, *src.get(&f.dfg.resolve_aliases(x)).unwrap_or(&x)),
                    &|_| None,
                    i,
                )
            } else {
                None
            };
            if let Some(c) = c {
                env.insert(rs[0], c);
                let v = match nb {
                    // `env` only tracks 64 bits; folding a wider result would
                    // fabricate the high half (and `iconst` tops out at i64).
                    Some(nb) => {
                        let ty = func.dfg.value_type(rs[0]);
                        if ty.bits() > 64 {
                            map.insert(rs[0], rs[0]);
                            continue;
                        }
                        FuncCursor::new(func)
                            .at_bottom(nb)
                            .ins()
                            .iconst(ty, c as i64)
                    }
                    None => rs[0],
                };
                map.insert(rs[0], v);
                continue;
            }
            for r in &rs {
                env.remove(r);
            }
            insts += 1;
            if insts > MAX_INSTS {
                return why("insts");
            }
            if let Some(nb) = nb {
                let ni = func.dfg.clone_inst(i);
                let vals: Vec<Value> = func
                    .dfg
                    .inst_values(ni)
                    .map(|v| {
                        let v = func.dfg.resolve_aliases(v);
                        *map.get(&v).unwrap_or(&v)
                    })
                    .collect();
                func.dfg.overwrite_inst_values(ni, vals.into_iter());
                func.layout.append_inst(ni, nb);
                let nrs = func.dfg.inst_results(ni).to_vec();
                if let Some(b) = lbytes {
                    vbytes.insert(nrs[0], b);
                }
                for (r, nr) in rs.into_iter().zip(nrs) {
                    map.insert(r, nr);
                }
            } else {
                if let Some(b) = lbytes {
                    vbytes.insert(rs[0], b);
                }
                for r in rs {
                    map.insert(r, r);
                }
            }
        }
        let Some(d) = decide(func, &env, t) else {
            return why(&format!("undecided {b} {}", func.dfg.display_inst(t)));
        };
        (b, args) = d;
    }
}

fn why<T>(r: &str) -> Option<T> {
    if std::env::var_os("PLIRON_UNROLL_DEBUG").is_some_and(|v| v == "2") {
        eprintln!("unroll: no: {r}");
    }
    None
}

/// Constant value of `v`, defined outside `body`, folding a few integer ops
/// (and `(x + c) - x`) and block params whose every incoming argument folds to
/// the same constant.
fn cval(func: &Function, body: &FxHashSet<Block>, v: Value, depth: u32) -> Option<u64> {
    use cranelift_codegen::ir::{Opcode, ValueDef};
    let v = func.dfg.resolve_aliases(v);
    let ty = func.dfg.value_type(v);
    if !ty.is_int() || ty.bits() > 64 || depth == 0 {
        return None;
    }
    let w = ty.bits();
    let m = |x: u64| if w >= 64 { x } else { x & ((1u64 << w) - 1) };
    match func.dfg.value_def(v) {
        ValueDef::Result(i, 0) => {
            if func.layout.inst_block(i).is_some_and(|b| body.contains(&b)) {
                return None;
            }
            match func.dfg.insts[i] {
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm,
                } => Some(m(imm.bits() as u64)),
                InstructionData::Binary { opcode, args } => {
                    let (x, y) = (
                        func.dfg.resolve_aliases(args[0]),
                        func.dfg.resolve_aliases(args[1]),
                    );
                    if opcode == Opcode::Isub
                        && let Some(xi) = func.dfg.value_def(x).inst()
                        && let InstructionData::Binary {
                            opcode: Opcode::Iadd,
                            args: [p, q],
                        } = func.dfg.insts[xi]
                    {
                        let (p, q) = (func.dfg.resolve_aliases(p), func.dfg.resolve_aliases(q));
                        if p == y {
                            return cval(func, body, q, depth - 1);
                        }
                        if q == y {
                            return cval(func, body, p, depth - 1);
                        }
                    }
                    let a = cval(func, body, x, depth - 1)?;
                    let b = cval(func, body, y, depth - 1)?;
                    Some(m(match opcode {
                        Opcode::Iadd => a.wrapping_add(b),
                        Opcode::Isub => a.wrapping_sub(b),
                        Opcode::Imul => a.wrapping_mul(b),
                        Opcode::Udiv if b != 0 => a / b,
                        Opcode::Urem if b != 0 => a % b,
                        Opcode::Band => a & b,
                        Opcode::Bor => a | b,
                        Opcode::Bxor => a ^ b,
                        _ => return None,
                    }))
                }
                _ => None,
            }
        }
        ValueDef::Param(b, k) => {
            if body.contains(&b) {
                return None;
            }
            let cfg_preds: Vec<Inst> = func
                .layout
                .blocks()
                .filter_map(|p| func.layout.last_inst(p))
                .collect();
            let mut out = None;
            let mut any = false;
            for t in cfg_preds {
                for bc in func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    let BlockArg::Value(a) = bc.args(&func.dfg.value_lists).nth(k)? else {
                        return None;
                    };
                    let c = cval(func, body, a, depth - 1)?;
                    if out.is_some_and(|o| o != c) {
                        return None;
                    }
                    out = Some(c);
                    any = true;
                }
            }
            if any { out } else { None }
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Byte {
    C(u8),
    /// Byte `k` (little-endian) of an emitted value.
    V(Value, u8),
}

fn in_body(func: &Function, body: &FxHashSet<Block>, v: Value) -> bool {
    use cranelift_codegen::ir::ValueDef;
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i).is_some_and(|b| body.contains(&b)),
        ValueDef::Param(b, _) => body.contains(&b),
        _ => false,
    }
}

/// Stack slot and offset that `v` points at in the current trip.
fn addr_of(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    slots: &FxHashMap<Value, (StackSlot, i64)>,
    body: &FxHashSet<Block>,
    v: Value,
) -> Option<(StackSlot, i64)> {
    let v = func.dfg.resolve_aliases(v);
    if let Some(&a) = slots.get(&v) {
        return Some(a);
    }
    if in_body(func, body, v) {
        return None;
    }
    addr_step(func, env, slots, body, func.dfg.value_def(v).inst()?)
}

fn addr_step(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    slots: &FxHashMap<Value, (StackSlot, i64)>,
    body: &FxHashSet<Block>,
    i: Inst,
) -> Option<(StackSlot, i64)> {
    match func.dfg.insts[i] {
        InstructionData::StackAddr {
            stack_slot, offset, ..
        } => Some((stack_slot, i64::from(i32::from(offset)))),
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            let (p, c) = match (known(func, env, a), known(func, env, b)) {
                (_, Some(c)) => (a, c),
                (Some(c), _) => (b, c),
                _ => return None,
            };
            let (s, o) = addr_of(func, env, slots, body, p)?;
            Some((s, o.wrapping_add(c as i64)))
        }
        _ => None,
    }
}

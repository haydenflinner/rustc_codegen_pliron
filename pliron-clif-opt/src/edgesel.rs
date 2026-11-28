//! Edge-conditioned select/branch fold (`PLIRON_EDGESEL`, default-on;
//! `=0` disables).
//!
//! On `brif c`'s then-edge `c != 0` and on its else-edge `c == 0`, so
//! a `select c, t, e` in the target picks a fixed arm. `str::chars`'s
//! `next().copied().unwrap_or(0)` clamp reaches CLIF as
//!
//! ```text
//! blockP:
//!     c = icmp eq cur, end
//!     brif c, done, body
//! body:
//!     a = select c, 0, cur        // load-addr clamp — c known 0 here
//!     b = select c, cur, cur+1    // cursor clamp
//! ```
//!
//! which isel lowers to a duplicate `cmp` plus two `cmov`s per char
//! on tokenize's ASCII scan (~1.4x vs LLVM; LLVM folds the selects
//! under the edge condition and pays neither). Intersecting the
//! `c == 0`/`c != 0` fact over every incoming edge folds each such
//! select to the known arm and turns a redundant `brif c` inside the
//! block into a `jump`.
//!
//! Only `brif` terminators contribute facts, and a fact survives
//! only when EVERY incoming edge establishes it — one `jump` or
//! `br_table` pred, or a `brif` on a different condition, drops the
//! whole map for that block. One CFG build + one inst scan; linear.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Opcode, Value, ValueDef};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::hash_map::Entry;

/// One incoming CFG edge's block-call arguments, values only.
struct InEdge {
    inst: Inst,
    edge: usize,
    args: Vec<Value>,
    values_only: bool,
}

fn int_bits(func: &Function, v: Value) -> Option<u32> {
    let ty = func.dfg.value_type(v);
    (ty.is_int() && !ty.is_vector() && ty.bits() <= 64).then(|| ty.bits())
}

fn mask_bits(k: u64, bits: u32) -> u64 {
    if bits >= 64 {
        k
    } else {
        k & ((1u64 << bits) - 1)
    }
}

/// `v` as a masked integer constant.
fn iconst(func: &Function, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    Some(mask_bits(imm.bits() as u64, int_bits(func, v)?))
}

/// Can `v` possibly be the constant `k`? Unknown arithmetic answers true;
/// only constants and constant-armed select trees can prove impossibility.
fn possible_eq(
    func: &Function,
    memo: &mut FxHashMap<Value, bool>,
    v: Value,
    k: u64,
    depth: usize,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if let Some(&r) = memo.get(&v) {
        return r;
    }
    if depth >= 10 {
        return true;
    }
    let r = if let Some(c) = iconst(func, v) {
        c == k
    } else if let Some(i) = func.dfg.value_def(v).inst()
        && let InstructionData::Ternary {
            opcode: Opcode::Select | Opcode::SelectSpectreGuard,
            args,
        } = func.dfg.insts[i]
    {
        possible_eq(func, memo, args[1], k, depth + 1)
            || possible_eq(func, memo, args[2], k, depth + 1)
    } else {
        true
    };
    memo.insert(v, r);
    r
}

fn fact(facts: &mut FxHashMap<Value, bool>, c: Value, truth: bool) -> bool {
    match facts.entry(c) {
        Entry::Vacant(e) => {
            e.insert(truth);
            true
        }
        Entry::Occupied(e) => *e.get() == truth,
    }
}

/// Given `v == k` on an edge, infer the polarity of the select conditions
/// that shape `v`. This is the inverse of the constant-armed select tree
/// rustc's `slice::range` validation lowering produces: success selects the
/// `2` discriminant only when both the bounds check and the end-overflow
/// check are false.
fn infer_eq(
    func: &Function,
    facts: &mut FxHashMap<Value, bool>,
    memo: &mut FxHashMap<Value, bool>,
    v: Value,
    k: u64,
    depth: usize,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if iconst(func, v) == Some(k) {
        return true;
    }
    if depth >= 10 {
        return false;
    }
    let Some(i) = func.dfg.value_def(v).inst() else {
        return false;
    };
    let InstructionData::Ternary {
        opcode: Opcode::Select | Opcode::SelectSpectreGuard,
        args,
    } = func.dfg.insts[i]
    else {
        return false;
    };
    let tp = possible_eq(func, memo, args[1], k, depth + 1);
    let fp = possible_eq(func, memo, args[2], k, depth + 1);
    let (truth, arm) = match (tp, fp) {
        (true, false) => (true, args[1]),
        (false, true) => (false, args[2]),
        _ => return false,
    };
    let c = func.dfg.resolve_aliases(args[0]);
    let mut trial = facts.clone();
    if fact(&mut trial, c, truth) && infer_eq(func, &mut trial, memo, arm, k, depth + 1) {
        *facts = trial;
        return true;
    }
    false
}

/// Evaluate the constant-armed selects in an incoming binding under the
/// inferred edge polarities. Unknown conditions stop at the original value:
/// this never materializes a partially rewritten select.
fn eval_edge(func: &Function, facts: &FxHashMap<Value, bool>, v: Value, depth: usize) -> Value {
    let v = func.dfg.resolve_aliases(v);
    if depth >= 10 {
        return v;
    }
    let Some(i) = func.dfg.value_def(v).inst() else {
        return v;
    };
    let InstructionData::Ternary {
        opcode: Opcode::Select | Opcode::SelectSpectreGuard,
        args,
    } = func.dfg.insts[i]
    else {
        return v;
    };
    let c = func.dfg.resolve_aliases(args[0]);
    match facts.get(&c) {
        Some(true) => eval_edge(func, facts, args[1], depth + 1),
        Some(false) => eval_edge(func, facts, args[2], depth + 1),
        None => v,
    }
}

fn envv(func: &Function, env: &FxHashMap<Value, Value>, v: Value, depth: usize) -> Value {
    let v = func.dfg.resolve_aliases(v);
    if depth >= 10 {
        return v;
    }
    match env.get(&v) {
        Some(&to) if to != v => envv(func, env, to, depth + 1),
        _ => v,
    }
}

fn iconst_i64(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// `(x + c1 + ... + cn) - x` as the zero-extended narrow `iconst`
/// encoding of `Σci` in wrapping arithmetic.
fn sub_delta(func: &Function, a: Value, b: Value, bits: u32) -> Option<i64> {
    let b = func.dfg.resolve_aliases(b);
    let mut v = func.dfg.resolve_aliases(a);
    let mut k = 0i64;
    for _ in 0..10 {
        if v == b {
            return Some(mask_bits(k as u64, bits) as i64);
        }
        let i = func.dfg.value_def(v).inst()?;
        let InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } = func.dfg.insts[i]
        else {
            return None;
        };
        let x = func.dfg.resolve_aliases(args[0]);
        let y = func.dfg.resolve_aliases(args[1]);
        if let Some(c) = iconst_i64(func, y) {
            k = k.wrapping_add(c);
            v = x;
        } else if let Some(c) = iconst_i64(func, x) {
            k = k.wrapping_add(c);
            v = y;
        } else {
            return None;
        }
    }
    None
}

/// Edge-equality cancellation (`PLIRON_EDGESEL_REL=1`, opt-in).
///
/// `slice::range` validation is merged into a small dispatch as
///
/// ```text
/// p:  tag  = select c0, 0, select c1, 1, 2
///     lo   = select c0, ..., x
///     hi   = select c0, ..., x + 4
///     jump d(tag, lo, hi)
/// d(tagp, lop, hip):
///     q = icmp eq tagp, 2
///     brif q, success, error
/// success:
///     n = isub hip, lop
/// ```
///
/// The success edge proves `tagp == 2`, which forces both select
/// conditions false, so `lop/hip` are `x` and `x + 4` there. The fold is
/// deliberately narrow: it only rewrites an `isub` in the unique-successor
/// block when the edge-evaluated operands cancel to a constant. It does
/// not substitute the payload values elsewhere, so there is no live-range
/// or SSA export hazard; and requiring unique incoming edges to both the
/// dispatch and the successor means every dynamic execution of the
/// successor is immediately preceded by the equality edge.
pub fn edge_rel(func: &mut Function) -> usize {
    // Most functions have no constant discriminant dispatch over one of
    // the dispatch block's own params. Collect just the blocks whose
    // incoming edges can matter, so the edge scan below does not allocate
    // an argument vector for every branch in the function.
    let mut wanted: FxHashSet<Block> = FxHashSet::default();
    for d in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(d) else {
            continue;
        };
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[term] else {
            continue;
        };
        let c = func.dfg.resolve_aliases(arg);
        let Some(ci) = func.dfg.value_def(c).inst() else {
            continue;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: cargs,
        } = func.dfg.insts[ci]
        else {
            continue;
        };
        if !matches!(
            cond,
            cranelift_codegen::ir::condcodes::IntCC::Equal
                | cranelift_codegen::ir::condcodes::IntCC::NotEqual
        ) {
            continue;
        }
        for (a, b) in [(cargs[0], cargs[1]), (cargs[1], cargs[0])] {
            if iconst(func, b).is_some() && iconst(func, a).is_none() {
                let x = func.dfg.resolve_aliases(a);
                if let ValueDef::Param(pb, _) = func.dfg.value_def(x)
                    && pb == d
                {
                    wanted.insert(d);
                    wanted.insert(blocks[0].block(&func.dfg.value_lists));
                    wanted.insert(blocks[1].block(&func.dfg.value_lists));
                    break;
                }
            }
        }
    }
    if wanted.is_empty() {
        return 0;
    }

    // All branch edges by target. Scanning once keeps the candidate walk
    // linear; per-edge expression evaluation is depth-capped.
    let mut incoming: FxHashMap<Block, Vec<InEdge>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for (edge, bc) in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            let dst = bc.block(&func.dfg.value_lists);
            if !wanted.contains(&dst) {
                continue;
            }
            let mut args = Vec::new();
            let mut values_only = true;
            for a in bc.args(&func.dfg.value_lists) {
                match a.as_value() {
                    Some(v) => args.push(func.dfg.resolve_aliases(v)),
                    None => {
                        values_only = false;
                        break;
                    }
                }
            }
            // Count every CFG edge, including one whose non-value block
            // args we cannot evaluate: it still makes the target
            // non-unique for the soundness gate below.
            incoming.entry(dst).or_default().push(InEdge {
                inst: t,
                edge,
                args,
                values_only,
            });
        }
    }

    let mut edits: Vec<(Inst, i64)> = Vec::new();
    for d in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(d) else {
            continue;
        };
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[term] else {
            continue;
        };
        let c = func.dfg.resolve_aliases(arg);
        let Some(ci) = func.dfg.value_def(c).inst() else {
            continue;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: cargs,
        } = func.dfg.insts[ci]
        else {
            continue;
        };
        let (x, k) = match (iconst(func, cargs[0]), iconst(func, cargs[1])) {
            (None, Some(k)) => (func.dfg.resolve_aliases(cargs[0]), k),
            (Some(k), None) => (func.dfg.resolve_aliases(cargs[1]), k),
            _ => continue,
        };
        if int_bits(func, x).is_none() {
            continue;
        }
        let ValueDef::Param(pb, xix) = func.dfg.value_def(x) else {
            continue;
        };
        if pb != d {
            continue;
        }
        // Exactly one way into the dispatch block: the incoming bindings
        // are the bindings on every execution of the edge below.
        let Some(din) = incoming.get(&d) else {
            continue;
        };
        if din.len() != 1
            || !din[0].values_only
            || din[0].args.len() != func.dfg.block_params(d).len()
        {
            continue;
        }
        let dbind = &din[0].args;
        for edge in 0..2 {
            let equality_edge = match cond {
                cranelift_codegen::ir::condcodes::IntCC::Equal => edge == 0,
                cranelift_codegen::ir::condcodes::IntCC::NotEqual => edge == 1,
                _ => false,
            };
            if !equality_edge {
                continue;
            }
            let succ = blocks[edge].block(&func.dfg.value_lists);
            if succ == d || blocks[1 - edge].block(&func.dfg.value_lists) == succ {
                continue;
            }
            // Exactly one way into the successor, and it is this edge.
            let Some(sin) = incoming.get(&succ) else {
                continue;
            };
            if sin.len() != 1 || sin[0].inst != term || sin[0].edge != edge {
                continue;
            }
            if !sin[0].values_only || sin[0].args.len() != func.dfg.block_params(succ).len() {
                continue;
            }
            let xbind = dbind[xix];
            let mut facts = FxHashMap::default();
            let mut memo = FxHashMap::default();
            if !infer_eq(func, &mut facts, &mut memo, xbind, k, 0) || facts.is_empty() {
                continue;
            }
            // Params of the dispatch evaluate through the selects their
            // unique incoming edge supplies. Params of the successor first
            // unwrap to the edge args (the rebind_exports shape), which may
            // themselves be dispatch params.
            let mut env: FxHashMap<Value, Value> = FxHashMap::default();
            for (ix, &p) in func.dfg.block_params(d).iter().enumerate() {
                env.insert(p, eval_edge(func, &facts, dbind[ix], 0));
            }
            for (ix, &p) in func.dfg.block_params(succ).iter().enumerate() {
                env.insert(p, sin[0].args[ix]);
            }
            for i in func.layout.block_insts(succ) {
                let InstructionData::Binary {
                    opcode: Opcode::Isub,
                    args,
                } = func.dfg.insts[i]
                else {
                    continue;
                };
                let ty = func.dfg.value_type(func.dfg.first_result(i));
                if !ty.is_int() || ty.is_vector() || ty.bits() > 64 {
                    continue;
                }
                let a = envv(func, &env, args[0], 0);
                let b = envv(func, &env, args[1], 0);
                if let Some(delta) = sub_delta(func, a, b, ty.bits()) {
                    edits.push((i, delta));
                }
            }
        }
    }
    let n = edits.len();
    for (i, k) in edits {
        func.dfg.insts[i] = InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm: k.into(),
        };
    }
    n
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let blocks: Vec<_> = func.layout.blocks().collect();
    let mut n = 0;
    for b in blocks {
        // c's value on each incoming edge: Some(true) = nonzero,
        // Some(false) = zero, absent = nothing known. A fact must hold
        // on every edge, so the map only ever shrinks.
        let mut facts: FxHashMap<Value, bool> = FxHashMap::default();
        let mut first = true;
        for p in cfg.pred_iter(b) {
            let mut edge: FxHashMap<Value, bool> = FxHashMap::default();
            if let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] {
                let arg = func.dfg.resolve_aliases(arg);
                // `blocks[0]` is taken when `arg != 0`.
                for (i, bc) in blocks.iter().enumerate() {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    match edge.entry(arg) {
                        Entry::Vacant(e) => {
                            e.insert(i == 0);
                        }
                        // `brif c, b, b`: the edge count can't pin c.
                        Entry::Occupied(e) => {
                            if *e.get() != (i == 0) {
                                e.remove();
                            }
                        }
                    }
                }
            }
            if first {
                facts = edge;
                first = false;
            } else {
                facts.retain(|v, k| edge.get(v) == Some(k));
            }
            if facts.is_empty() {
                break;
            }
        }
        if facts.is_empty() {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            match func.dfg.insts[i] {
                InstructionData::Ternary {
                    opcode: Opcode::Select | Opcode::SelectSpectreGuard,
                    args,
                } => {
                    if let Some(&k) = facts.get(&func.dfg.resolve_aliases(args[0])) {
                        let arm = args[usize::from(!k) + 1];
                        let rs: Vec<Value> = func.dfg.inst_results(i).to_vec();
                        func.dfg.detach_inst_results(i);
                        for r in rs {
                            func.dfg.change_to_alias(r, arm);
                        }
                        func.layout.remove_inst(i);
                        n += 1;
                    }
                }
                InstructionData::Brif { arg, blocks, .. } => {
                    if let Some(&k) = facts.get(&func.dfg.resolve_aliases(arg)) {
                        let destination = blocks[usize::from(!k)];
                        func.dfg.insts[i] = InstructionData::Jump {
                            opcode: Opcode::Jump,
                            destination,
                        };
                        n += 1;
                    }
                }
                _ => {}
            }
        }
    }
    if pliron_cfg::env_is("PLIRON_EDGESEL_REL", "1") {
        n += edge_rel(func);
    }
    n
}

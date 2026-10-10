//! Loop-carried affine-param refolding (`PLIRON_IVREFOLD`).
//!
//! A header param stepped by a constant on every back edge is an affine
//! IV `p = base + r * iv` for any sibling affine param `iv` with a
//! dividing step. Carrying `p` costs a regalloc parallel copy plus an
//! `iadd` increment per back edge; LLVM instead keeps ONE scalar IV and
//! indexes `base + iv*step` straight into the x64 amode. This pass does
//! the same refold on final CLIF: uses of the carried param `p` become
//! `bp + r * iv` (with `bp = p0 - r * i0` materialized on the entry
//! edge, so `bp` is loop-invariant and needs no param), then `p`'s slot
//! is stripped from every edge. The multi-pointer gather/scatter loops
//! lose ~4 carried values and their per-iter copy chains this way.
//!
//! Ring arithmetic makes the refold exact: `(p0 - r*i0) + r*(i0 + t*s)`
//! reduces to `p0 + r*t*s` mod 2^w with no nowrap requirement. Params
//! whose back edge passes the param itself (`jump h(p)` — a wasted
//! carried copy) are replaced by their entry value outright.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, ExceptionTableItem, Function, Inst, InstBuilder, InstructionData, Opcode,
    Value, ValueDef,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

/// `v` as `p + k` for constant `k` (`isub p, k` is `p - k`); the arg may
/// be an alias chain to the add result.
fn step_of(func: &Function, v: Value, p: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    let (a, b) = (
        func.dfg.resolve_aliases(args[0]),
        func.dfg.resolve_aliases(args[1]),
    );
    match opcode {
        Opcode::Iadd => {
            if a == p {
                iconst(func, b)
            } else if b == p {
                iconst(func, a)
            } else {
                None
            }
        }
        Opcode::Isub if a == p => iconst(func, b).map(|k| k.wrapping_neg()),
        _ => None,
    }
}

/// Edges into each block: `(inst, dest_index)` pairs (branch arg lists
/// are per-destination on brif/br_table/try_call).
fn edges_to(func: &Function) -> FxHashMap<Block, Vec<(Inst, usize)>> {
    let mut edges: FxHashMap<Block, Vec<(Inst, usize)>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for (d, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                edges
                    .entry(bc.block(&func.dfg.value_lists))
                    .or_default()
                    .push((i, d));
            }
        }
    }
    edges
}

/// Resolved `Value` args of one edge destination; `None` on non-Value
/// pseudo-args (TryCallRet/Exn) or arity mismatch.
fn edge_args(func: &Function, i: Inst, d: usize, n: usize) -> Option<Vec<Value>> {
    let bc = &func.dfg.insts[i].branch_destination(
        &func.dfg.jump_tables,
        &func.dfg.exception_tables,
    )[d];
    let mut v = Vec::with_capacity(n);
    for a in bc.args(&func.dfg.value_lists) {
        match a {
            BlockArg::Value(x) => v.push(func.dfg.resolve_aliases(x)),
            _ => return None,
        }
    }
    (v.len() == n).then_some(v)
}

/// `x * r` at cursor `pos`: `ishl` for positive powers of two so the
/// result folds straight into a base+index*scale amode.
fn emit_scaled(pos: &mut FuncCursor, x: Value, r: i64) -> Value {
    if r > 0 && (r as u64).is_power_of_two() {
        pos.ins().ishl_imm_s(x, r.trailing_zeros() as i64)
    } else {
        pos.ins().imul_imm_s(x, r)
    }
}

/// Try to refold header `h`'s carried affine params onto one anchor IV.
fn refold_header(
    func: &mut Function,
    domtree: &DominatorTree,
    edges: &FxHashMap<Block, Vec<(Inst, usize)>>,
    h: Block,
) -> usize {
    let params = func.dfg.block_params(h).to_vec();
    if params.len() < 2 {
        return 0;
    }
    let Some(es) = edges.get(&h) else {
        return 0;
    };
    let (mut entries, mut backs) = (vec![], vec![]);
    for &(i, d) in es {
        let Some(src) = func.layout.inst_block(i) else {
            return 0;
        };
        // An edge from inside `h`'s dominance region is a back edge.
        if src == h || domtree.dominates(h, src, &func.layout) {
            backs.push((i, d));
        } else {
            entries.push((i, d));
        }
    }
    // A loop with a single entry edge lets `bp = p0 - r*i0` be computed
    // once in the preheader; more entries would need a bp param of its
    // own. No back edge means no recurrence.
    if backs.is_empty() || entries.len() != 1 {
        return 0;
    }
    let mut back_args = Vec::with_capacity(backs.len());
    for &(i, d) in &backs {
        let Some(a) = edge_args(func, i, d, params.len()) else {
            return 0;
        };
        back_args.push(a);
    }
    let (ei, ed) = entries[0];
    let Some(entry_args) = edge_args(func, ei, ed, params.len()) else {
        return 0;
    };
    // Per-slot affine step: `p` itself counts as step 0 (an edge could
    // mix `p` and `p+k` — then the steps differ and the slot isn't
    // affine). Identity slots pass `p` on every back edge.
    let mut steps = Vec::with_capacity(params.len());
    for (ix, &p) in params.iter().enumerate() {
        let mut step: Option<i64> = None;
        let mut ok = true;
        for a in &back_args {
            let k = if func.dfg.resolve_aliases(a[ix]) == p {
                Some(0)
            } else {
                step_of(func, a[ix], p)
            };
            match k {
                Some(k) => match step {
                    None => step = Some(k),
                    Some(s) if s == k => {}
                    _ => {
                        ok = false;
                        break;
                    }
                },
                None => {
                    ok = false;
                    break;
                }
            }
        }
        steps.push(if ok { step } else { None });
    }
    // Anchor: an affine slot (nonzero step) whose param feeds an icmp
    // — the loop counter — else the smallest |step|.
    let mut icmp_use = vec![false; params.len()];
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::IntCompare { args, .. } = func.dfg.insts[i] {
                for &a in &args {
                    let a = func.dfg.resolve_aliases(a);
                    if let Some(ix) = params.iter().position(|&p| p == a) {
                        icmp_use[ix] = true;
                    }
                }
            }
        }
    }
    let pick = |icmp_only: bool| -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for (ix, &st) in steps.iter().enumerate() {
            let Some(s) = st else { continue };
            if s == 0 || (icmp_only && !icmp_use[ix]) {
                continue;
            }
            if best.is_none_or(|(_, b)| s.unsigned_abs() < b) {
                best = Some((ix, s.unsigned_abs()));
            }
        }
        best.map(|(ix, _)| ix)
    };
    let Some(ai) = pick(true).or_else(|| pick(false)) else {
        return 0;
    };
    let astep = steps[ai].unwrap();
    let pa = params[ai];
    let i0 = entry_args[ai];
    let aty = func.dfg.value_type(pa);
    if !aty.is_int() || aty.bits() > 64 {
        return 0;
    }
    // Eliminated slots: identity params (replace by entry arg) and
    // same-type affine params with step divisible by the anchor's.
    let mut drops: Vec<(usize, i64)> = vec![]; // (slot, ratio)
    let mut idents: Vec<usize> = vec![];
    for (ix, &p) in params.iter().enumerate() {
        if ix == ai {
            continue;
        }
        let Some(s) = steps[ix] else { continue };
        if s == 0 {
            if func.dfg.value_type(p) == aty {
                idents.push(ix);
            }
            continue;
        }
        if s % astep != 0 || func.dfg.value_type(p) != aty {
            continue;
        }
        drops.push((ix, s / astep));
    }
    if drops.is_empty() && idents.is_empty() {
        return 0;
    }
    // Precompute const-ness of the entry args before a cursor takes
    // `func`: `bp = p0 - r*i0` on the entry edge.
    let drops_pre: Vec<(usize, i64, Option<i64>, Option<i64>)> = drops
        .iter()
        .map(|&(ix, r)| (ix, r, iconst(func, i0), iconst(func, entry_args[ix])))
        .collect();
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut n = 0;
    // Phase 1: `bp` values on the entry edge (its pred dominates `h`
    // since this edge is the only way in).
    let mut bpv = Vec::with_capacity(drops.len());
    {
        let mut pos = FuncCursor::new(func).at_inst(ei);
        for &(ix, r, ki0, kp0) in &drops_pre {
            let p0 = entry_args[ix];
            let bp = if ki0 == Some(0) {
                p0
            } else if let (Some(a), Some(b)) = (kp0, ki0) {
                pos.ins().iconst(aty, a.wrapping_sub(r.wrapping_mul(b)))
            } else {
                let t = if r == 1 {
                    i0
                } else {
                    emit_scaled(&mut pos, i0, r)
                };
                pos.ins().isub(p0, t)
            };
            bpv.push(bp);
        }
    }
    // Phase 2: `pv = bp + r*iv` at the top of `h`, one cursor so the
    // emitted chain stays in def-before-use order.
    let bp_zero: Vec<bool> = bpv.iter().map(|&bp| iconst(func, bp) == Some(0)).collect();
    {
        let mut pos = FuncCursor::new(func).at_first_insertion_point(h);
        let mut scaled_cache: FxHashMap<i64, Value> = FxHashMap::default();
        for (j, &(ix, r, ..)) in drops_pre.iter().enumerate() {
            let bp = bpv[j];
            let pv = if r == 1 {
                pos.ins().iadd(bp, pa)
            } else {
                let sh = *scaled_cache
                    .entry(r)
                    .or_insert_with(|| emit_scaled(&mut pos, pa, r));
                if bp_zero[j] {
                    sh
                } else {
                    pos.ins().iadd(bp, sh)
                }
            };
            pmap.insert(params[ix], pv);
            n += 1;
        }
        for &ix in &idents {
            pmap.insert(params[ix], entry_args[ix]);
            n += 1;
        }
    }
    // Every value that resolves to a dropped param needs a map entry:
    // a use can name an alias of the param, and `dfg` is mutably
    // borrowed inside the rewrite so it can't resolve there.
    let dropped: FxHashSet<Value> = pmap.keys().copied().collect();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                let r = func.dfg.resolve_aliases(v);
                if dropped.contains(&r)
                    && let Some(&pv) = pmap.get(&r)
                {
                    pmap.insert(v, pv);
                }
            }
        }
    }
    // Global use rewrite (inst args, branch-dest args, exn contexts).
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            func.dfg.map_inst_values(i, |x| pmap.get(&x).copied().unwrap_or(x));
        }
    }
    // Strip the dropped arg slots (descending keeps indices valid).
    let mut ixs: Vec<usize> = drops
        .iter()
        .map(|&(ix, _)| ix)
        .chain(idents.iter().copied())
        .collect();
    ixs.sort_unstable_by(|x, y| y.cmp(x));
    for &(i, d) in entries.iter().chain(backs.iter()) {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[i].branch_destination_mut(
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
        )[d];
        for &ix in &ixs {
            bc.remove(ix, &mut dfg.value_lists);
        }
    }
    for &ix in &ixs {
        func.dfg.remove_block_param(params[ix]);
    }
    n
}

/// Uses of `v` that are all self-feeding into slot `ix` of header `h`:
/// a pure-inst chain whose results end up only as the back-edge arg
/// for `v`'s own slot. Edge-arg uses elsewhere or any impure use
/// makes it live. Recursive (SSA def-use is a DAG; the only cycle is
/// through `v`'s own slot, treated as a dead sink).
fn self_feeding(
    func: &Function,
    h: Block,
    ix: usize,
    v: Value,
    alias_targets: &FxHashSet<Value>,
    depth: usize,
) -> bool {
    if depth > 32 {
        return false;
    }
    // Removing `v` (or the dead chain feeding it) must not strand an alias:
    // `va -> v` survives removal and leaves any user of `va` dangling.
    if alias_targets.contains(&v) {
        return false;
    }
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            // All inst args except branch-destination args (call args are
            // variable-length, so `inst_fixed_args` would miss them).
            for &a in func.dfg.inst_args(i) {
                if func.dfg.resolve_aliases(a) != v {
                    continue;
                }
                if !crate::jumpthread::removable(func, i, true) {
                    return false;
                }
                for &r in func.dfg.inst_results(i) {
                    if !self_feeding(func, h, ix, r, alias_targets, depth + 1) {
                        return false;
                    }
                }
            }
            // Edge args: only the self-slot is a dead sink.
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let dst = bc.block(&func.dfg.value_lists);
                for (s, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    if let BlockArg::Value(x) = a
                        && func.dfg.resolve_aliases(x) == v
                        && !(dst == h && s == ix)
                    {
                        return false;
                    }
                }
            }
            // Exception contexts are always real uses.
            if let Some(et) = func.dfg.insts[i].exception_table()
                && func.dfg.exception_tables[et]
                    .items()
                    .any(|x| matches!(x, ExceptionTableItem::Context(x) if func.dfg.resolve_aliases(x) == v))
            {
                return false;
            }
        }
    }
    true
}

/// Drop loop-carried params that are dead recurrences: `p`'s only uses
/// feed the computation of `p`'s own back-edge arg (e.g. the classic
/// `p -> iadd p,k -> jump h(...)` byte-offset cursor indvars leaves
/// when the body's addressing moved to another param). Uses outside
/// that chain keep `p` live. Returns slots eliminated.
pub fn deadrecs(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let edges = edges_to(func);
    // Values that are alias targets: removing them would strand the alias.
    let alias_targets: FxHashSet<Value> = (0..func.dfg.num_values())
        .map(Value::new)
        .filter(|&v| func.dfg.value_is_alias(v))
        .map(|v| func.dfg.resolve_aliases(v))
        .collect();
    let mut n = 0;
    for h in func.layout.blocks().collect::<Vec<_>>() {
        let params = func.dfg.block_params(h).to_vec();
        if params.is_empty() {
            continue;
        }
        let Some(es) = edges.get(&h) else {
            continue;
        };
        // Only loops can hide a self-feeding recurrence.
        if !es.iter().any(|&(i, _)| {
            func.layout
                .inst_block(i)
                .is_some_and(|src| src == h || domtree.dominates(h, src, &func.layout))
        }) {
            continue;
        }
        let mut ixs = vec![];
        for (ix, &p) in params.iter().enumerate() {
            if self_feeding(func, h, ix, p, &alias_targets, 0) {
                ixs.push(ix);
            }
        }
        if ixs.is_empty() {
            continue;
        }
        ixs.sort_unstable_by(|x, y| y.cmp(x));
        for &(i, d) in es {
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[i].branch_destination_mut(
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
            )[d];
            for &ix in &ixs {
                bc.remove(ix, &mut dfg.value_lists);
            }
        }
        for &ix in &ixs {
            func.dfg.remove_block_param(params[ix]);
        }
        n += ixs.len();
    }
    if n > 0 {
        crate::jumpthread::remove_dead_insts(func, true);
    }
    n
}

/// Refold carried affine params; returns slots eliminated.
pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let edges = edges_to(func);
    let mut n = 0;
    for h in func.layout.blocks().collect::<Vec<_>>() {
        n += refold_header(func, &domtree, &edges, h);
    }
    if n > 0 {
        // The stripped increments (`p + step` edge args) are usually dead now.
        crate::jumpthread::remove_dead_insts(func, true);
    }
    n
}

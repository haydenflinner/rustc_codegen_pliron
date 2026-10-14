//! Loop-carried affine-param refolding (`PLIRON_IVREFOLD`, default on).
//!
//! A header param stepped by a constant on every back edge is an affine
//! IV `p = base + r * iv` for any sibling affine param `iv` with a
//! dividing step. Carrying `p` costs a regalloc parallel copy plus an
//! `iadd` increment per back edge; LLVM instead keeps ONE scalar IV and
//! indexes `base + iv*step` straight into the x64 amode. This pass does
//! the same refold on final CLIF: load/store uses of the carried param
//! `p` get a fresh `iadd bp, scaled(iv)` per use (folding into
//! base+index*scale amodes — no address math materializes), other uses
//! share `pv = bp + r * iv` at the header top (with `bp = p0 - r * i0`
//! materialized on the entry edge, so `bp` is loop-invariant and needs
//! no param), then `p`'s slot is stripped from every edge. When a
//! positive-step sibling yields amode-scale ratios ({1,2,4,8}) it is
//! chosen as anchor over the icmp counter.
//!
//! Ring arithmetic makes the refold exact: `(p0 - r*i0) + r*(i0 + t*s)`
//! reduces to `p0 + r*t*s` mod 2^w with no nowrap requirement. Params
//! whose back edge passes the param itself (`jump h(p)` — a wasted
//! carried copy) are replaced by their entry value outright. A
//! constant-trip counter whose only remaining use is its own step +
//! exit test is folded onto the anchor's step value and dies with
//! deadrecs.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::ir::{
    Block, BlockArg, ExceptionTableItem, Function, Inst, InstBuilder, InstructionData, Opcode,
    Value, ValueDef, condcodes::IntCC,
};
use rustc_hash::{FxHashMap, FxHashSet};

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
    let bc =
        &func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
    let mut v = Vec::with_capacity(n);
    for a in bc.args(&func.dfg.value_lists) {
        match a {
            BlockArg::Value(x) => v.push(func.dfg.resolve_aliases(x)),
            _ => return None,
        }
    }
    (v.len() == n).then_some(v)
}

/// The non-arithmetic base `v` is built on: resolve `x + k` / `x - k`
/// layers to the value they extend.
fn base_leaf(func: &Function, v: Value) -> Value {
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..8 {
        let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
            return v;
        };
        let (a, b) = match func.dfg.insts[i] {
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args: [a, b],
            } => (a, b),
            InstructionData::Binary {
                opcode: Opcode::Isub,
                args: [a, b],
            } if iconst(func, b).is_some() => (a, b),
            _ => return v,
        };
        if iconst(func, b).is_some() {
            v = func.dfg.resolve_aliases(a);
        } else if iconst(func, a).is_some() {
            v = func.dfg.resolve_aliases(b);
        } else {
            return v;
        }
    }
    v
}

/// `h`'s natural loop body: `h` plus every block that can reach a
/// back-edge source without passing through `h`.
fn loop_body(
    func: &Function,
    edges: &FxHashMap<Block, Vec<(Inst, usize)>>,
    h: Block,
    backs: &[(Inst, usize)],
) -> FxHashSet<Block> {
    let mut body: FxHashSet<Block> = [h].into_iter().collect();
    let mut wl: Vec<Block> = backs
        .iter()
        .filter_map(|&(i, _)| func.layout.inst_block(i))
        .collect();
    while let Some(b) = wl.pop() {
        if !body.insert(b) {
            continue;
        }
        for &(pi, _) in edges.get(&b).into_iter().flatten() {
            if let Some(p) = func.layout.inst_block(pi)
                && !body.contains(&p)
            {
                wl.push(p);
            }
        }
    }
    body
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
    debug: bool,
    pmap_out: &mut FxHashMap<Value, Value>,
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
    // — the loop counter — else the smallest |step|. When a positive-step
    // sibling yields more drops whose ratio is an x64 amode scale
    // ({1,2,4,8}), prefer it: those uses refold into `base + iv*scale`
    // address modes (LLVM's shape) instead of a `bp - |r|*iv` product.
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
    let good_drops = |c: usize, astep: i64| -> usize {
        let cty = func.dfg.value_type(params[c]);
        steps
            .iter()
            .enumerate()
            .filter(|&(ix, st)| {
                ix != c
                    && st.is_some_and(|s| {
                        s != 0
                            && s % astep == 0
                            && matches!(s / astep, 1 | 2 | 4 | 8)
                            && func.dfg.value_type(params[ix]) == cty
                    })
            })
            .count()
    };
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
    let mut ai_opt: Option<(usize, usize, bool, u64)> = None;
    for (ix, &st) in steps.iter().enumerate() {
        let Some(s) = st else { continue };
        if s == 0 {
            continue;
        }
        let t = func.dfg.value_type(params[ix]);
        if !t.is_int() || t.bits() > 64 {
            continue;
        }
        let g = good_drops(ix, s);
        if ai_opt.is_none_or(|(_, g2, ic, b)| {
            (g, icmp_use[ix], u64::MAX - s.unsigned_abs()) > (g2, ic, b)
        }) {
            ai_opt = Some((ix, g, icmp_use[ix], u64::MAX - s.unsigned_abs()));
        }
    }
    let Some((ai_scaled, g, ..)) = ai_opt else {
        return 0;
    };
    let ai = if g > 0 {
        ai_scaled
    } else {
        pick(true).or_else(|| pick(false)).unwrap_or(ai_scaled)
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
    // Refolding `p` replaces its carried uses with `bp + iv*r`, where
    // `bp = p0 - r*i0` folds back onto the non-arithmetic base `p0` is
    // built on. When that base is live through the loop yet unused inside
    // it, the refold recreates a cross-region liverange spanning the
    // loop — the split regalloc reconciles with per-iteration copies.
    // Keeping the carried param costs one `iadd` on the back edge but
    // leaves the loop free of the shared base entirely. Uses confined to
    // blocks dominating `h` die at loop entry — the base is not live
    // through the loop, so `bp` just trades places with the param.
    if !drops.is_empty() {
        let body = loop_body(func, edges, h, &backs);
        let mut in_use = FxHashSet::default();
        let mut out_live = FxHashSet::default();
        for b in func.layout.blocks() {
            let body_b = body.contains(&b);
            let dom_h = domtree.dominates(b, h, &func.layout);
            if body_b || dom_h {
                for i in func.layout.block_insts(b) {
                    for v in func.dfg.inst_values(i) {
                        let v = func.dfg.resolve_aliases(v);
                        if body_b {
                            in_use.insert(v);
                        }
                    }
                }
            } else {
                for i in func.layout.block_insts(b) {
                    for v in func.dfg.inst_values(i) {
                        out_live.insert(func.dfg.resolve_aliases(v));
                    }
                }
            }
        }
        drops.retain(|&(ix, _)| {
            let leaf = base_leaf(func, entry_args[ix]);
            iconst(func, leaf).is_some() || in_use.contains(&leaf) || !out_live.contains(&leaf)
        });
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
    // Load/store uses of a dropped param get a fresh `iadd bp, scaled(iv)`
    // per use: single-use chains fold into base+index*scale amodes, so
    // no address computation materializes at all. Non-address uses share
    // the `pv` materialized at the top of `h` as before.
    let drop_of: FxHashMap<Value, usize> = drops.iter().map(|&(ix, _)| (params[ix], ix)).collect();
    let mut addr_uses: Vec<(Inst, usize)> = vec![];
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let a = match func.dfg.insts[i] {
                InstructionData::Load { arg, .. } | InstructionData::LoadNoOffset { arg, .. } => {
                    arg
                }
                InstructionData::Store { args, .. }
                | InstructionData::StoreNoOffset { args, .. } => args[1],
                _ => continue,
            };
            if let Some(&ix) = drop_of.get(&func.dfg.resolve_aliases(a)) {
                addr_uses.push((i, ix));
            }
        }
    }
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
    // emitted chain stays in def-before-use order. Per slot, `slot` keeps
    // `(bp, scaled)` for the per-use amode chains below.
    let bp_zero: Vec<bool> = bpv.iter().map(|&bp| iconst(func, bp) == Some(0)).collect();
    let mut slot: FxHashMap<usize, (Value, Value)> = FxHashMap::default();
    {
        let mut pos = FuncCursor::new(func).at_first_insertion_point(h);
        let mut scaled_cache: FxHashMap<i64, Value> = FxHashMap::default();
        for (j, &(ix, r, ..)) in drops_pre.iter().enumerate() {
            let bp = bpv[j];
            let sh = if r == 1 {
                pa
            } else {
                *scaled_cache
                    .entry(r)
                    .or_insert_with(|| emit_scaled(&mut pos, pa, r))
            };
            let pv = if r == 1 {
                pos.ins().iadd(bp, pa)
            } else if bp_zero[j] {
                sh
            } else {
                pos.ins().iadd(bp, sh)
            };
            slot.insert(ix, (bp, sh));
            pmap.insert(params[ix], pv);
            n += 1;
        }
        for &ix in &idents {
            pmap.insert(params[ix], entry_args[ix]);
            n += 1;
        }
    }
    // Per-use addressing: `iadd bp, scaled(iv)` right before each
    // load/store; the single-use chain folds into a base+index*scale
    // amode so no address math materializes.
    for &(i, ix) in &addr_uses {
        let &(bp, sh) = &slot[&ix];
        let v_a = if iconst(func, bp) == Some(0) {
            sh
        } else {
            FuncCursor::new(func).at_inst(i).ins().iadd(bp, sh)
        };
        match &mut func.dfg.insts[i] {
            InstructionData::Load { arg, .. } | InstructionData::LoadNoOffset { arg, .. } => {
                *arg = v_a;
            }
            InstructionData::Store { args, .. } | InstructionData::StoreNoOffset { args, .. } => {
                args[1] = v_a;
            }
            _ => {}
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
            func.dfg
                .map_inst_values(i, |x| pmap.get(&x).copied().unwrap_or(x));
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
        let bc = &mut dfg.insts[i]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
        for &ix in &ixs {
            bc.remove(ix, &mut dfg.value_lists);
        }
    }
    for &ix in &ixs {
        func.dfg.remove_block_param(params[ix]);
    }
    // Counter elimination: when a latch brif exits on
    // `icmp ne/eq step(c), k` — or directly on `brif step(c)` (a
    // countdown-to-zero `testq` exit) — for a still-carried counter
    // param `c`, retarget the compare to the anchor's own step value
    // against `e_a + T*astep` (T = trip count). `c` then only feeds its
    // own increment, so `deadrecs` drops the carried counter entirely.
    //
    // `k`/`e_c` may be runtime values when the counter step is a power
    // of two and both are provably multiples of it (a masked trip count
    // like `(n-1)&~7`): `t = (k - e_c)/sc` materializes on the entry
    // edge next to `bound`. The retargeted exit coincides with the
    // original's while the trip count stays below the anchor's additive
    // order (≥2^57 for any nonzero step — a loop iterating past that
    // can't terminate physically anyway).
    let e_a = entry_args[ai];
    for (cix, &cp) in params.iter().enumerate() {
        if cix == ai || ixs.contains(&cix) || steps[cix].is_none_or(|s| s == 0) {
            continue;
        }
        let sc = steps[cix].unwrap();
        if func.dfg.value_type(cp) != aty {
            continue;
        }
        let e_cv = entry_args[cix];
        for (j, &(bi, d)) in backs.iter().enumerate() {
            let InstructionData::Brif { arg: cond, .. } = func.dfg.insts[bi] else {
                continue;
            };
            let cr = func.dfg.resolve_aliases(cond);
            let a_c = func.dfg.resolve_aliases(back_args[j][cix]);
            // Two exit forms: `icmp ne/eq step(c), k`, or `brif step(c)`
            // directly — a countdown exiting when the step value is zero.
            // `kv` is `None` for the brif form (k = 0).
            let (cinst, kv): (Option<Inst>, Option<Value>) = match func.dfg.value_def(cr) {
                ValueDef::Result(i, _)
                    if let InstructionData::IntCompare {
                        cond: cc,
                        args: cargs,
                        ..
                    } = func.dfg.insts[i]
                        && (d == 0 && cc == IntCC::NotEqual || d == 1 && cc == IntCC::Equal) =>
                {
                    // `h` on the then-arm needs `ne` (loop while step
                    // != k); on the else-arm `eq`.
                    if func.dfg.resolve_aliases(cargs[0]) == a_c {
                        (Some(i), Some(cargs[1]))
                    } else if func.dfg.resolve_aliases(cargs[1]) == a_c {
                        (Some(i), Some(cargs[0]))
                    } else {
                        continue;
                    }
                }
                _ => {
                    // `brif step(c)` with `h` on the then-arm exits
                    // when the step value hits zero.
                    if !(d == 0 && cr == a_c && func.dfg.value_type(cr) == aty) {
                        continue;
                    }
                    (None, None)
                }
            };
            // A runtime `k` must dominate the entry edge `ei` — a
            // loop-variant `k` isn't a fixed exit anyway, and emitting
            // `num` at `ei` would use it before its def.
            if let Some(v) = kv
                && iconst(func, v).is_none()
            {
                let db = match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
                    ValueDef::Result(i, _) => func.layout.inst_block(i),
                    ValueDef::Param(b, _) => Some(b),
                    _ => None,
                };
                let eb = func.layout.inst_block(ei);
                if !matches!((db, eb), (Some(db), Some(eb))
                        if domtree.dominates(db, eb, &func.layout))
                {
                    continue;
                }
            }
            // Trip count `t = (k - e_c)/sc` must exist exactly: constant
            // arithmetic when both bounds are iconst, else a power-of-two
            // step and both bounds provably its multiples.
            let k_iconst = match kv {
                Some(v) => iconst(func, v),
                None => Some(0),
            };
            let sa = sc.unsigned_abs();
            let t = match (k_iconst, iconst(func, e_cv)) {
                (Some(k), Some(ec)) => {
                    let num = k.wrapping_sub(ec);
                    if num.checked_rem(sc) != Some(0) {
                        continue;
                    }
                    let t = num / sc;
                    // The new test exits at `t+1 ≡ T` under the anchor's
                    // step period; for it to coincide with the original's
                    // first exit, T must be the smallest positive solution
                    // in both moduli.
                    let pb = |s: i64| 64u32 - s.trailing_zeros().min(63);
                    if t < 1 || (t as u64 as u128) >= (1u128 << pb(astep).min(pb(sc))) {
                        continue;
                    }
                    Some(t)
                }
                _ => {
                    // Runtime `t` is computed mod order(sc) = 2^64/sa via
                    // `ushr`; the retargeted test's first exit is `t mod
                    // order(astep)`. With both step periods >= 2^57 the
                    // two coincide on every physically reachable trip
                    // count — a loop iterating past that can't terminate
                    // on real hardware anyway.
                    if aty.bits() != 64
                        || !sa.is_power_of_two()
                        || sa > 128
                        || astep.trailing_zeros() > 7
                        || !(k_iconst.is_some_and(|k| (k as u64).wrapping_rem(sa) == 0)
                            || provably_div(func, edges, kv.unwrap(), sa))
                        || !provably_div(func, edges, e_cv, sa)
                    {
                        continue;
                    }
                    None
                }
            };
            // `c` must only feed its own back-edge step insts, and the
            // exit test (icmp or step value) only the latch brif.
            let step_insts: FxHashSet<Inst> = back_args
                .iter()
                .map(|a| func.dfg.resolve_aliases(a[cix]))
                .filter_map(|a| match func.dfg.value_def(a) {
                    ValueDef::Result(i, _) => Some(i),
                    _ => None,
                })
                .collect();
            let mut ok = true;
            'scan: for b in func.layout.blocks() {
                for i in func.layout.block_insts(b) {
                    for &a in func.dfg.inst_args(i) {
                        let r = func.dfg.resolve_aliases(a);
                        if r == cp && !step_insts.contains(&i) {
                            ok = false;
                            break 'scan;
                        }
                        if r == cr && i != bi {
                            ok = false;
                            break 'scan;
                        }
                    }
                    for bc in func.dfg.insts[i]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    {
                        for a in bc.args(&func.dfg.value_lists) {
                            if let BlockArg::Value(x) = a {
                                let r = func.dfg.resolve_aliases(x);
                                if r == cp || (r == cr && i != bi) {
                                    ok = false;
                                    break 'scan;
                                }
                            }
                        }
                    }
                }
            }
            if !ok {
                continue;
            }
            // Bound: `e_a + T*astep` materialized once on the entry edge
            // (`T` as `t` insts when runtime). The anchor's next value
            // `pa + astep` is re-emitted right before the test: the icmp
            // may sit in an earlier block than the latch's own step inst.
            let bound = {
                let mut pos = FuncCursor::new(func).at_inst(ei);
                match t {
                    Some(t) => {
                        let off = t.wrapping_mul(astep);
                        if off == 0 {
                            e_a
                        } else {
                            let kc = pos.ins().iconst(aty, off);
                            pos.ins().iadd(e_a, kc)
                        }
                    }
                    None => {
                        let kv2 = match k_iconst {
                            Some(k) => pos.ins().iconst(aty, k),
                            None => kv.unwrap(),
                        };
                        let mut num = pos.ins().isub(kv2, e_cv);
                        if sc < 0 {
                            num = pos.ins().ineg(num);
                        }
                        // `num` is a multiple of `sa` mod 2^64, so `ushr`
                        // gives the canonical trip count `t` in
                        // `[0, order(sc))` directly.
                        let tv = pos
                            .ins()
                            .ushr_imm_s(num, sc.unsigned_abs().trailing_zeros() as i64);
                        let prod = pos.ins().imul_imm_s(tv, astep);
                        pos.ins().iadd(e_a, prod)
                    }
                }
            };
            // The anchor's own back-edge arg is `pa + astep` — reuse it
            // when its def reaches the test instead of emitting a fresh
            // `iconst`+`iadd` (duplicate step adds confuse isel into
            // separate increment/copy pairs).
            let a_step = func.dfg.resolve_aliases(back_args[j][ai]);
            let step_reaches = |pos: Inst| -> bool {
                match func.dfg.value_def(a_step) {
                    ValueDef::Result(si, _) => domtree.dominates(si, pos, &func.layout),
                    _ => false,
                }
            };
            if let Some(cinst) = cinst {
                let a_a = if step_reaches(cinst) {
                    a_step
                } else {
                    let mut pos = FuncCursor::new(func).at_inst(cinst);
                    let kc = pos.ins().iconst(aty, astep);
                    pos.ins().iadd(pa, kc)
                };
                if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[cinst] {
                    *args = [a_a, bound];
                }
            } else {
                // `brif a_c` → `brif (icmp ne a_a, bound)` — `h` is the
                // then-arm, so the loop continues while the anchor's step
                // value differs from the bound. `a_step` is an arg of `bi`
                // itself, so its def always dominates the brif.
                let mut pos = FuncCursor::new(func).at_inst(bi);
                let cmp = pos.ins().icmp(IntCC::NotEqual, a_step, bound);
                if let InstructionData::Brif { arg, .. } = &mut func.dfg.insts[bi] {
                    *arg = cmp;
                }
            }
            if debug {
                eprintln!("ivrefold {h}: counter {cp} folded onto anchor {pa} (trips {t:?})");
            }
            n += 1;
            break;
        }
    }
    // Dropped params' replacements are value-equal (`pmap` rewrites uses),
    // so the pipeline can move their facts onto the carriers.
    pmap_out.extend(pmap);
    n
}

/// `s | v` provable for `s` a power of two: constants check the remainder;
/// `band`'s mask, `ishl`'s shift, or a multiple operand prove the low bits
/// are zero; `iadd`/`isub`/`ineg` preserve multiples; `x - (x & (2^k-1))`
/// rounds down to a multiple of `2^k`; a param of a single-pred block is
/// its incoming edge arg.
fn provably_div(
    func: &Function,
    edges: &FxHashMap<Block, Vec<(Inst, usize)>>,
    v: Value,
    s: u64,
) -> bool {
    pd(func, edges, v, s, 16)
}

fn pd(
    func: &Function,
    edges: &FxHashMap<Block, Vec<(Inst, usize)>>,
    v: Value,
    s: u64,
    depth: u8,
) -> bool {
    if s == 0 || depth == 0 {
        return false;
    }
    if let Some(k) = iconst(func, v) {
        return (k as u64).wrapping_rem(s) == 0;
    }
    let pd = |a: Value| pd(func, edges, a, s, depth - 1);
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        // A param of a single-pred block equals the arg that edge passes —
        // unless the edge is a self-loop, where the param is an IV.
        ValueDef::Param(b, ix) => match edges.get(&b) {
            Some(es) if es.len() == 1 && func.layout.inst_block(es[0].0) != Some(b) => {
                let (i, d) = es[0];
                let np = func.dfg.block_params(b).len();
                edge_args(func, i, d, np).is_some_and(|a| pd(a[ix]))
            }
            _ => false,
        },
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::Binary { opcode, args } => match opcode {
                Opcode::Band | Opcode::Imul => args.iter().any(|&a| pd(a)),
                Opcode::Ishl => iconst(func, args[1])
                    .is_some_and(|sh| sh >= 0 && (1u128 << sh.min(127)) % (s as u128) == 0),
                Opcode::Iadd => args.iter().all(|&a| pd(a)),
                Opcode::Isub => {
                    args.iter().all(|&a| pd(a)) || {
                        // `x - (x & m)` for `m = s*2^j - 1` rounds `x` down
                        // to a multiple of `m+1`, hence of `s`.
                        let x = func.dfg.resolve_aliases(args[0]);
                        let ValueDef::Result(j, _) =
                            func.dfg.value_def(func.dfg.resolve_aliases(args[1]))
                        else {
                            return false;
                        };
                        let InstructionData::Binary {
                            opcode: Opcode::Band,
                            args: ba,
                        } = func.dfg.insts[j]
                        else {
                            return false;
                        };
                        let m_ok = |a: Value| {
                            iconst(func, a).is_some_and(|k| {
                                let m1 = (k as u64).wrapping_add(1);
                                m1.is_power_of_two() && m1 % s == 0
                            })
                        };
                        (func.dfg.resolve_aliases(ba[0]) == x && m_ok(ba[1]))
                            || (func.dfg.resolve_aliases(ba[1]) == x && m_ok(ba[0]))
                    }
                }
                _ => false,
            },
            InstructionData::Unary {
                opcode: Opcode::Ineg,
                arg,
            } => pd(arg),
            _ => false,
        },
        _ => false,
    }
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
pub fn deadrecs(func: &mut Function, an: &mut crate::pipeline::Analysis) -> usize {
    let (_, domtree) = an.cfg_dt(func);
    let edges = edges_to(func);
    // Values that are alias targets: removing them would strand the
    // alias. `Alias{INVALID, reserved}` tombstones read as aliases but
    // panic `resolve_aliases` — skip them by their immediate dest.
    let alias_targets: FxHashSet<Value> = (0..func.dfg.num_values())
        .map(Value::new)
        .filter(|&v| {
            func.dfg.value_is_alias(v)
                && func
                    .dfg
                    .value_alias_dest_for_serialization(v)
                    .is_some_and(|o| o != Value::reserved_value())
        })
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
            let bc = &mut dfg.insts[i]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
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
pub fn run(
    func: &mut Function,
    an: &mut crate::pipeline::Analysis,
) -> (usize, FxHashMap<Value, Value>) {
    let (_, domtree) = an.cfg_dt(func);
    let edges = edges_to(func);
    let debug = crate::env_set("PLIRON_IVREFOLD_DEBUG");
    let mut n = 0;
    let mut pmap = FxHashMap::default();
    for h in func.layout.blocks().collect::<Vec<_>>() {
        n += refold_header(func, domtree, &edges, h, debug, &mut pmap);
    }
    if n > 0 {
        // The stripped increments (`p + step` edge args) are usually dead now.
        crate::jumpthread::remove_dead_insts(func, true);
    }
    (n, pmap)
}

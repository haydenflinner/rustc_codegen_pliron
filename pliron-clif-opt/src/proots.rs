//! Uniform-source block-param analysis, shared by `param_src`,
//! `param_coeffs`, and the acc-class queries in loopvec's `AccCx`. One
//! Ascent fixpoint replaces the per-pass hand-rolled param scans: edge
//! args flow in as facts, the derived relations answer "does every
//! in-edge agree" — on the value (`alias`), or on carrying one chosen
//! value (`like`, see [`like_map`]).
//!
//! Fact extraction is the interesting half: edge args are
//! alias-resolved, self-carries (`edge(p, p)` — "keep my previous
//! value", which makes every uniform reading inductively exact) are
//! dropped, non-`Value` args mark the param `blocked`, and unreachable
//! predecessors contribute nothing.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::InstructionData;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, Opcode, Value};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

/// Extracted edge facts for one function. `detail[p]` mirrors the
/// `(terminator, dest slot, source block, arg)` tuples `AccCx::in_args`
/// walks — one CFG pass feeds both the Ascent relations and that query.
pub struct Facts {
    /// `(param, arg)` on every reachable in-edge (aliases resolved,
    /// `arg != param`).
    pub edge: Vec<(Value, Value)>,
    /// Params with a non-`Value` arg somewhere — opaque to every class.
    pub blocked: Vec<Value>,
    /// `param → [(terminator inst, dest slot, pred block, arg)]`.
    pub detail: FxHashMap<Value, Vec<(Inst, usize, Block, Value)>>,
}

pub fn facts(func: &Function) -> Facts {
    let Some(entry) = func.layout.entry_block() else {
        return Facts {
            edge: Vec::new(),
            blocked: Vec::new(),
            detail: FxHashMap::default(),
        };
    };
    // Reachable = BFS over the CFG (equivalent to `dt.is_reachable`,
    // without needing the dominator tree).
    let cfg = ControlFlowGraph::with_function(func);
    let mut reach: FxHashSet<Block> = [entry].into_iter().collect();
    let mut stack = vec![entry];
    while let Some(b) = stack.pop() {
        for s in cfg.succ_iter(b) {
            if reach.insert(s) {
                stack.push(s);
            }
        }
    }
    let mut f = Facts {
        edge: Vec::new(),
        blocked: Vec::new(),
        detail: FxHashMap::default(),
    };
    for b in func.layout.blocks() {
        for p in func.dfg.block_params(b) {
            f.detail.entry(*p).or_default();
        }
    }
    // Layout order (not the `reach` set's iteration order) keeps `edge`
    // and `detail` deterministic — `acc_leaves`'s leaf order feeds
    // downstream plan decisions.
    for b in func.layout.blocks() {
        if !reach.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            for (slot, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                let dst = bc.block(&func.dfg.value_lists);
                let params = func.dfg.block_params(dst);
                for (k, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    let Some(&p) = params.get(k) else {
                        break;
                    };
                    match a {
                        BlockArg::Value(v) => {
                            let v = func.dfg.resolve_aliases(v);
                            if v != p {
                                f.edge.push((p, v));
                                f.detail.entry(p).or_default().push((i, slot, b, v));
                            }
                        }
                        _ => f.blocked.push(p),
                    }
                }
            }
        }
    }
    f
}

ascent::ascent! {
    struct Proots;

    /// param → in-edge arg.
    relation edge_arg(Value, Value);
    /// param with an opaque (non-`Value`) edge arg.
    relation blocked(Value);

    /// param fed ≥2 distinct values.
    relation diffs(Value);
    diffs(p) <-- edge_arg(p, a1), edge_arg(p, a2), if a1 != a2;

    /// Every in-edge delivers the same value — `param_src`'s relation.
    relation alias(Value, Value);
    alias(p, a) <-- edge_arg(p, a), !diffs(p), !blocked(p);
}

/// The alias relation over pre-extracted `facts` — `param_src`'s job.
pub fn alias_of(f: &Facts) -> FxHashMap<Value, Value> {
    let mut p = Proots::default();
    p.edge_arg = f.edge.clone();
    p.blocked = f.blocked.iter().map(|&v| (v,)).collect();
    p.run();
    p.alias.into_iter().collect()
}

/// The alias relation alone — `param_src`'s job.
pub fn alias_map(func: &Function) -> FxHashMap<Value, Value> {
    alias_of(&facts(func))
}

/// `v = base + k` definitions: `iadd base, iconst k` (either operand
/// order), `isub base, iconst k` → `k` negated. Operands are
/// alias-resolved; the result must be single (it always is for these).
pub fn aff_facts(func: &Function) -> Vec<(Value, Value, i64)> {
    let mut f = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
                continue;
            };
            let kof = |v: Value| {
                let d = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
                match func.dfg.insts[d] {
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                    } => Some(imm.bits()),
                    _ => None,
                }
            };
            let (base, k) = match opcode {
                Opcode::Iadd => match (kof(args[1]), kof(args[0])) {
                    (Some(k), _) => (args[0], k),
                    (None, Some(k)) => (args[1], k),
                    _ => continue,
                },
                Opcode::Isub => match kof(args[1]) {
                    Some(k) => (args[0], k.wrapping_neg()),
                    None => continue,
                },
                _ => continue,
            };
            let Some(&res) = func.dfg.inst_results(i).first() else {
                continue;
            };
            f.push((res, func.dfg.resolve_aliases(base), k));
        }
    }
    f
}

ascent::ascent! {
    /// Affine normal forms for phi-merge: `off(v, r, k)` reads
    /// "`v ≡ r + k` (wrapping)" where `r` is not itself an
    /// `iadd`/`isub`-by-const result. Values, params and non-affine defs
    /// have no `off` row and act as their own `(v, 0)` root. A chain
    /// `iadd(iadd x, 8), 4` collapses to `off(·, x, 12)`: two syntactically
    /// distinct values in one `(r, k)` class are interchangeable, which is
    /// what makes taildup's per-edge arg clones mergeable again.
    struct AffN;

    /// `v = base + k`.
    relation aff(Value, Value, i64);
    /// `v = root + k` collapsed transitively.
    relation off(Value, Value, i64);

    // Non-affine bases are their own roots; chain heads extend by the
    // child's collapsed offset. One `aff` row per `v` (SSA) keeps `off`
    // functional: a value has exactly one normal form.
    off(m, m, 0i64) <-- aff(_, m, _), !aff(m, _, _);
    off(v, r, km.wrapping_add(*kr)) <-- aff(v, m, km), off(m, r, kr);
}

/// `v ↦ (root, k)` affine normal form over the whole function.
pub fn affine_nf(func: &Function) -> FxHashMap<Value, (Value, i64)> {
    let mut p = AffN::default();
    p.aff = aff_facts(func);
    p.run();
    p.off.into_iter().map(|(v, r, k)| (v, (r, k))).collect()
}

ascent::ascent! {
    /// "Always carries `acc`" for one acc candidate — loopvec's `AccCx`
    /// is_acc as a stratified fixpoint: an interior param is `bad` when a
    /// reachable edge feeds it a concretely non-acc or opaque arg, or any
    /// already-bad param; `like` is what remains. The depth bound and
    /// phi-cycle false negatives of the recursive version disappear.
    struct AccR;

    relation edge_arg(Value, Value);
    /// Params of in-body, non-header blocks.
    relation interior(Value);
    /// `{p}` — the acc itself.
    relation accv(Value);
    /// Params with an opaque (non-`Value`) edge arg.
    relation blocked(Value);

    relation bad(Value);
    bad(q) <-- interior(q), edge_arg(q, a), !interior(a), !accv(a);
    bad(q) <-- interior(q), blocked(q);
    // No trackable edges (self-carry only or unreachable preds): the
    // value is indeterminate, and feeding params must see it as non-acc.
    bad(q) <-- interior(q), !edge_arg(q, _);
    bad(q) <-- edge_arg(q, a), bad(a);

    /// Interior param whose every reachable in-edge carries acc.
    /// Requiring an `edge_arg` keeps unreachable/self-only params out.
    relation like(Value);
    like(q) <-- interior(q), edge_arg(q, _), !bad(q);
}

/// Interior params that provably always carry `acc`.
pub fn like_map(f: &Facts, interior: impl Iterator<Item = Value>, acc: Value) -> FxHashSet<Value> {
    let mut p = AccR::default();
    p.edge_arg = f.edge.clone();
    p.blocked = f.blocked.iter().map(|&v| (v,)).collect();
    p.interior = interior.map(|v| (v,)).collect();
    p.accv = vec![(acc,)];
    p.run();
    p.like.into_iter().map(|(v,)| v).collect()
}

/// Sound unsigned upper bounds, `Value → v ≤ bound`. `loadfwd` peels an
/// `iadd` whose other operand has a small bound: the addend is then
/// provably `≥ 0` and too small to wrap a real pointer, so the sum's
/// byte offset in the pointer side's root can't shrink. A store
/// through such an address provably starts at or above the inner
/// offset — that kills the "opaque scatter store invalidates the
/// pivot" problem in partition loops (`8*i + base + 8` can't reach
/// `base + 0`).
///
/// Seeds: `iconst c ≥ 0` → c; every int-typed value of bits < 64 →
/// `2^bits - 1`; `uextend` → `2^srcbits - 1`. Rules: `iadd`/`imul`
/// (saturating), `band`/`umin` (min), `bor`/`bxor` (≤ a+b), `ushr` by a
/// const, `select`/`umax` (max of data arms), and block params — every
/// non-`blocked` in-edge source bounded → their max (self-carry
/// doesn't change the value, so `edge`'s skipping is exact here too).
/// Bounds only tighten on update, so a worklist over def→use edges
/// reaches the least fixpoint directly.
pub fn bounds(func: &Function, f: &Facts) -> FxHashMap<Value, u64> {
    let ra = |v: Value| func.dfg.resolve_aliases(v);
    let tbound = |v: Value| {
        let b = func.dfg.value_type(v).bits();
        (b < 64).then(|| (1u64 << b) - 1)
    };
    let mut bnd: FxHashMap<Value, u64> = FxHashMap::default();
    // `users[a]` = values whose bound can tighten when `a`'s does.
    let mut users: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let Some(&res) = func.dfg.inst_results(i).first() else {
                continue;
            };
            for &a in func.dfg.inst_args(i) {
                users.entry(ra(a)).or_default().push(res);
            }
        }
    }
    // Param sources: `f.edge` grouped by param, minus `blocked` params
    // (their edge list is partial — a `non-Value` source is unbounded).
    let mut pargs: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    for &(p, a) in &f.edge {
        pargs.entry(p).or_default().push(a);
        users.entry(a).or_default().push(p);
    }
    let blocked: FxHashSet<Value> = f.blocked.iter().copied().collect();
    let konst = |v: Value| {
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => u64::try_from(imm.bits()).ok(),
            _ => None,
        }
    };
    // One bound candidate for `v` from its definition; `None` when the
    // def can't bound it. Param candidates come from the edge group.
    let eval = |bnd: &FxHashMap<Value, u64>, v: Value| -> Option<u64> {
        if let Some(srcs) = pargs.get(&v) {
            if blocked.contains(&v) {
                return None;
            }
            let mut m = None;
            for &a in srcs {
                m = Some(m.unwrap_or(0).max(*bnd.get(&a)?));
            }
            return m;
        }
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::Binary { opcode, args } => {
                let (x, y) = (*bnd.get(&ra(args[0]))?, *bnd.get(&ra(args[1]))?);
                Some(match opcode {
                    Opcode::Iadd | Opcode::Bor | Opcode::Bxor => x.saturating_add(y),
                    Opcode::Imul => x.saturating_mul(y),
                    Opcode::Band | Opcode::Umin => x.min(y),
                    Opcode::Umax => x.max(y),
                    Opcode::Ushr => return konst(ra(args[1])).map(|k| x >> (k % 64)),
                    _ => return None,
                })
            }
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } => Some((*bnd.get(&ra(args[1]))?).max(*bnd.get(&ra(args[2]))?)),
            _ => None,
        }
    };
    // Seeds.
    let mut wl: VecDeque<Value> = VecDeque::new();
    for b in func.layout.blocks() {
        for &p in func.dfg.block_params(b) {
            if let Some(t) = tbound(p) {
                bnd.entry(ra(p)).or_insert(t);
            }
        }
        for i in func.layout.block_insts(b) {
            for &res in func.dfg.inst_results(i) {
                let v = ra(res);
                let mut seed = tbound(v);
                match func.dfg.insts[i] {
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                    } => {
                        if let Ok(c) = u64::try_from(imm.bits()) {
                            seed = Some(seed.unwrap_or(u64::MAX).min(c));
                        }
                    }
                    InstructionData::Unary {
                        opcode: Opcode::Uextend,
                        arg,
                    } => {
                        let src = func.dfg.value_type(arg).bits();
                        if src < 64 {
                            seed = Some(seed.unwrap_or(u64::MAX).min((1u64 << src) - 1));
                        }
                    }
                    _ => {}
                }
                if let Some(c) = seed {
                    bnd.insert(v, c);
                }
            }
        }
    }
    wl.extend(bnd.keys().copied());
    while let Some(x) = wl.pop_front() {
        let Some(us) = users.get(&x) else { continue };
        for &u in us {
            // `eval` needs the raw value (its def/param group); `bnd` is
            // keyed resolved like the seeds and all lookups.
            let ru = ra(u);
            if let Some(c) = eval(&bnd, u)
                && bnd.get(&ru).is_none_or(|&b| c < b)
            {
                bnd.insert(ru, c);
                wl.push_back(ru);
            }
        }
    }
    bnd
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::types;
    use cranelift_codegen::ir::{AbiParam, Function, Signature, UserFuncName};
    use cranelift_codegen::ir::InstBuilder;
    use cranelift_codegen::isa::CallConv;

    /// `uextend`/`imul`/`iadd`/`select` chains bound; an i64 param fed a
    /// bounded edge takes the source max; a self-incrementing loop
    /// counter stays unbounded (its bound is a cyclic equation with no
    /// finite solution — the honest answer).
    #[test]
    fn bounds_propagates_and_counters_stay_unbounded() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let head = func.dfg.make_block();
        let tail = func.dfg.make_block();
        for b in [entry, head, tail] {
            func.layout.append_block(b);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let cnt = func.dfg.append_block_param(head, types::I64);
        let small = func.dfg.append_block_param(head, types::I64);
        let r = func.dfg.append_block_param(tail, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let b8 = cur.ins().iconst(types::I8, 7);
        let w = cur.ins().uextend(types::I64, b8);
        let e = cur.ins().iconst(types::I64, 8);
        let m = cur.ins().imul(w, e);
        let two = cur.ins().iconst(types::I64, 2);
        let s = cur.ins().iadd(m, two);
        cur.ins().jump(head, &[x.into(), s.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(head);
        let e2 = cur.ins().iconst(types::I64, 8);
        let c2 = cur.ins().iadd(cnt, e2);
        let done = cur.ins().icmp(IntCC::Equal, c2, small);
        cur.ins().brif(
            done,
            head,
            &[c2.into(), small.into()],
            tail,
            &[small.into()],
        );

        FuncCursor::new(&mut func).at_bottom(tail).ins().return_(&[r]);

        let f = facts(&func);
        let bnd = bounds(&func, &f);
        assert_eq!(bnd[&w], 255);
        assert_eq!(bnd[&m], 2040);
        assert_eq!(bnd[&s], 2042);
        assert_eq!(bnd[&small], 2042);
        assert!(!bnd.contains_key(&cnt));
        assert!(!bnd.contains_key(&c2));
    }
}

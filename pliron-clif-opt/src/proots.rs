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

    /// Canonical member of a param's equivalence web: chase uniform
    /// sources transitively until a value with no uniform source (a
    /// param's source that is itself merged is not the web's root —
    /// `q` feeding `p` everywhere makes `q` `p`'s representative, and
    /// if `q` is uniform-sourced too, *its* root is). Cyclic webs emit
    /// no row, which is sound — a self-sustaining alias cycle has no
    /// entry seed anyway.
    relation canon(Value, Value);
    canon(p, a) <-- alias(p, a), !alias(a, _);
    canon(p, r) <-- alias(p, m), canon(m, r);
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

/// Param → its web's canonical representative (the transitive uniform
/// source). Used to normalize `Key::V` leaves in `key_of` so params on
/// different in-edge paths that provably carry the same value compare
/// congruent.
pub fn canon_map(func: &Function) -> FxHashMap<Value, Value> {
    let f = facts(func);
    let mut p = Proots::default();
    p.edge_arg = f.edge.clone();
    p.blocked = f.blocked.iter().map(|&v| (v,)).collect();
    p.run();
    p.canon.into_iter().collect()
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

/// `affine_nf` lifted through params — the phi web proper: a param whose
/// reachable in-edge args all land in one `(root, k)` class *is* `root+k`
/// at its block's entry, so it takes that class too (a downstream edge
/// passing `p` then keys through `p`'s class — multi-hop congruence).
/// This is genuinely a fixpoint over params but not expressible as one
/// monotone Ascent rule — "all edges agree" is a negation through the
/// `off` recursion — so it runs as a bounded Rust fixpoint instead:
/// rows are only ever added (`p ↦ (r,k)` where `r != p`), one row per
/// param, so the loop terminates. Self-edge args of the form `p + d`
/// correctly keep `p` out of any class rooted at `p` itself, and a
/// self-carry edge contributes no fact at all — under induction the
/// carried value stays in the class the other edges established.
pub fn web_nf(func: &Function) -> FxHashMap<Value, (Value, i64)> {
    let mut nf = affine_nf(func);
    let f = facts(func);
    let blocked: FxHashSet<Value> = f.blocked.iter().copied().collect();
    loop {
        let mut grew = false;
        for (&p, es) in &f.detail {
            if es.is_empty() || blocked.contains(&p) || nf.contains_key(&p) {
                continue;
            }
            let mut cls: Option<(Value, i64)> = None;
            let mut uniform = true;
            for &(_, _, _, a) in es {
                let c = nf.get(&a).copied().unwrap_or((a, 0));
                match cls {
                    None => cls = Some(c),
                    Some(t) if t != c => {
                        uniform = false;
                        break;
                    }
                    _ => {}
                }
            }
            if uniform
                && let Some((r, k)) = cls
                && r != p
            {
                nf.insert(p, (r, k));
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    nf
}

/// Congruence key for the phi-web merges: two values with equal `Key`s
/// compute the same thing. `V` is an opaque leaf (param or non-pure
/// result — identity only), `K` a materializable `iconst`, `A` an
/// `iadd`/`isub`-const chain collapsed by `off` (`root + k`, k ≠ 0),
/// `N` a pure inst node keyed on its operands' keys — so congruence is
/// transitive through arbitrary depth, not the one-level `arg_shape`
/// check `arghoist` makes. `extra` carries the discriminator fields
/// (`icmp`'s cond, `stack_addr`'s slot) that opcode alone doesn't pin.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    V(Value),
    K(cranelift_codegen::ir::Type, i64),
    A(cranelift_codegen::ir::Type, Box<Key>, i64),
    N(Opcode, cranelift_codegen::ir::Type, Box<[Key]>, u64),
    /// "Whatever arrived in param slot `i` on this edge" — a leaf for
    /// sibling-slot congruence (`paramize`), so `p_j = f(p_i)` merges
    /// key as `f`'s tree over `P(i)` on every edge.
    P(u32),
}

/// `v`'s congruence key: aliases resolved, affine chains collapsed via
/// `off`, pure ops keyed structurally on their operands' keys, and `V`
/// leaves normalized through `canon` — a param's leaf is its web's
/// representative, so the same logical value arriving through different
/// merged-away params still keys equal. Recursion bottoms out at
/// params/`V` leaves — SSA def chains are finite.
pub fn key_of(
    func: &Function,
    nf: &FxHashMap<Value, (Value, i64)>,
    canon: &FxHashMap<Value, Value>,
    memo: &mut FxHashMap<Value, Key>,
    v: Value,
) -> Key {
    fn go(
        func: &Function,
        nf: &FxHashMap<Value, (Value, i64)>,
        canon: &FxHashMap<Value, Value>,
        memo: &mut FxHashMap<Value, Key>,
        v: Value,
        d: usize,
    ) -> Key {
        let v = func.dfg.resolve_aliases(v);
        let v = canon.get(&v).copied().unwrap_or(v);
        if d > 64 {
            return Key::V(v);
        }
        if let Some(k) = memo.get(&v) {
            return k.clone();
        }
        let k = go_uncached(func, nf, canon, memo, v, d);
        memo.insert(v, k.clone());
        return k;
    }
    fn go_uncached(
        func: &Function,
        nf: &FxHashMap<Value, (Value, i64)>,
        canon: &FxHashMap<Value, Value>,
        memo: &mut FxHashMap<Value, Key>,
        v: Value,
        d: usize,
    ) -> Key {
        if let Some(&(r, k)) = nf.get(&v)
            && r != v
        {
            let rk = go(func, nf, canon, memo, r, d + 1);
            let ty = func.dfg.value_type(v);
            return if k == 0 {
                rk
            } else {
                Key::A(ty, Box::new(rk), k)
            };
        }
        let Some(di) = func.dfg.value_def(v).inst() else {
            return Key::V(v);
        };
        let mut args = |vs: &[Value]| -> Box<[Key]> {
            vs.iter()
                .map(|&a| {
                    let a = func.dfg.resolve_aliases(a);
                    go(func, nf, canon, memo, a, d + 1)
                })
                .collect()
        };
        let rty = func.dfg.value_type(v);
        match func.dfg.insts[di] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Key::K(rty, imm.bits()),
            InstructionData::Binary { opcode, args: a }
                if matches!(
                    opcode,
                    Opcode::Band
                        | Opcode::Bor
                        | Opcode::Bxor
                        | Opcode::Iadd
                        | Opcode::Isub
                        | Opcode::Ishl
                        | Opcode::Ushr
                ) =>
            {
                Key::N(opcode, rty, args(&a), 0)
            }
            InstructionData::Unary { opcode, arg }
                if matches!(
                    opcode,
                    Opcode::Bnot | Opcode::Uextend | Opcode::Sextend | Opcode::Ireduce
                ) =>
            {
                Key::N(opcode, rty, args(&[arg]), 0)
            }
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                args: a,
                cond,
            } => Key::N(Opcode::Icmp, rty, args(&a), cond as u64),
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args: a,
            } => Key::N(Opcode::Select, rty, args(&a), 0),
            InstructionData::StackAddr {
                stack_slot,
                offset,
                ..
            } => Key::N(
                Opcode::StackAddr,
                rty,
                Box::new([Key::K(
                    cranelift_codegen::ir::types::I32,
                    i32::from(offset) as i64,
                )]),
                stack_slot.as_u32() as u64,
            ),
            _ => Key::V(v),
        }
    }
    go(func, nf, canon, memo, v, 0)
}

/// Rewrite `k` slot-relative: any subtree equal to `slots[i]`'s key
/// becomes `P(i)` — "this value is whatever slot `i` carried on this
/// edge". Subtrees are matched on keys, not values, so an edge arg like
/// `imul (iadd x,8), 2` paramizes against a sibling slot carrying
/// `iadd x,8` even though the SSA names differ. `skip` excludes the
/// slot being keyed (a `p_j` referencing itself is a real phi). `None`
/// slots — edge args that aren't SSA values — never produce a `P` leaf.
pub fn paramize(k: &Key, slots: &[Option<Key>], skip: usize) -> Key {
    if let Some(i) = slots
        .iter()
        .enumerate()
        .find(|(i, s)| *i != skip && s.as_ref() == Some(k))
        .map(|(i, _)| i)
    {
        return Key::P(i as u32);
    }
    match k {
        Key::A(t, rk, off) => Key::A(*t, Box::new(paramize(rk, slots, skip)), *off),
        Key::N(op, t, args, extra) => Key::N(
            *op,
            *t,
            args.iter().map(|a| paramize(a, slots, skip)).collect(),
            *extra,
        ),
        _ => k.clone(),
    }
}

/// Rewrite `V` leaves naming `ps` members through `rep` — under a
/// param bisimulation hypothesis a leaf referencing slot `i` becomes
/// its class leader's leaf, so per-edge signatures compare structurally
/// (a back-edge arg `p_j` keys as `V(rep)` just like `p_i`'s does).
pub fn key_rep(k: &Key, ps: &[Value], rep: &dyn Fn(Value) -> Value) -> Key {
    match k {
        Key::V(w) if ps.contains(w) => Key::V(rep(*w)),
        Key::A(t, rk, o) => Key::A(*t, Box::new(key_rep(rk, ps, rep)), *o),
        Key::N(op, t, args, x) => Key::N(
            *op,
            *t,
            args.iter().map(|a| key_rep(a, ps, rep)).collect(),
            *x,
        ),
        _ => k.clone(),
    }
}

/// Expand every `P(i)` leaf via `resolve(i)`, which returns the slot's
/// materialized form: a carrier `V`, or the slot's own key (already
/// `P`-free or resolved recursively) — `None` if the slot can't be
/// used. The `stack` guard breaks `P`-cycles (`p_i = f(p_j)` while
/// `p_j = g(p_i)`): revisiting an in-progress slot fails the resolve.
pub fn resolve_p(
    k: &Key,
    resolve: &mut dyn FnMut(u32, &mut Vec<u32>) -> Option<Key>,
    stack: &mut Vec<u32>,
) -> Option<Key> {
    Some(match k {
        Key::P(i) => {
            if stack.contains(i) {
                return None;
            }
            stack.push(*i);
            let r = resolve(*i, stack);
            stack.pop();
            r?
        }
        Key::A(t, rk, off) => Key::A(*t, Box::new(resolve_p(rk, resolve, stack)?), *off),
        Key::N(op, t, args, extra) => {
            let mut as_ = Vec::with_capacity(args.len());
            for a in args.iter() {
                as_.push(resolve_p(a, resolve, stack)?);
            }
            Key::N(*op, *t, as_.into_boxed_slice(), *extra)
        }
        _ => k.clone(),
    })
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

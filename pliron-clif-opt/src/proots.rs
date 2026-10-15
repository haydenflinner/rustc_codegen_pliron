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

    /// param fed ≥2 distinct values — computed in Rust (`alias_of`),
    /// an O(E) scan instead of the O(E_p²) self-join.
    relation diffs(Value);

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

/// `diffs` as a datalog join costs Σ E_p² comparisons (all arg pairs
/// per param) just to prove existence; a single scan is O(E). Every
/// `Proots` run must seed it as an input.
fn diffs_rows(f: &Facts) -> Vec<(Value,)> {
    let mut first: FxHashMap<Value, Value> = FxHashMap::default();
    let mut diffs: Vec<(Value,)> = Vec::new();
    for &(p, v) in &f.edge {
        match first.entry(p) {
            std::collections::hash_map::Entry::Occupied(e) => {
                if *e.get() != v {
                    diffs.push((p,));
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(v);
            }
        }
    }
    diffs
}

/// The alias relation over pre-extracted `facts` — `param_src`'s job.
pub fn alias_of(f: &Facts) -> FxHashMap<Value, Value> {
    let mut p = Proots::default();
    p.edge_arg = f.edge.clone();
    p.blocked = f.blocked.iter().map(|&v| (v,)).collect();
    p.diffs = diffs_rows(f);
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
    p.diffs = diffs_rows(&f);
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
            // A self-row `(p, p, 0)` from `affine_nf` — p used as an
            // `iadd`/`isub` base anywhere — is no class at all; only a
            // real row (`r != p`) settles p's membership.
            if es.is_empty()
                || blocked.contains(&p)
                || nf.get(&p).is_some_and(|&(r, _)| r != p)
            {
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
    // A row `p ↦ (r,k)` inserted while `r` was still its own leaf stays
    // unnormalized when `r` later gains a row — equal values would key
    // differently. Chase roots through `nf` until each row names the
    // final root; cycles (p ↦ r ↦ p) keep the last true equivalence.
    for _ in 0..nf.len().max(1) {
        let mut moved = false;
        let keys: Vec<Value> = nf.keys().copied().collect();
        for p in keys {
            let (mut r, mut k) = nf[&p];
            let mut seen: FxHashSet<Value> = [p].into_iter().collect();
            while seen.insert(r) {
                match nf.get(&r) {
                    Some(&(r2, k2)) if r2 != r => {
                        k = k.wrapping_add(k2);
                        r = r2;
                    }
                    _ => break,
                }
            }
            if r != p && nf[&p] != (r, k) {
                nf.insert(p, (r, k));
                moved = true;
            }
        }
        if !moved {
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
    A(cranelift_codegen::ir::Type, std::rc::Rc<Key>, i64),
    N(Opcode, cranelift_codegen::ir::Type, std::rc::Rc<[Key]>, u64),
    /// "Whatever arrived in param slot `i` on this edge" — a leaf for
    /// sibling-slot congruence (`paramize`), so `p_j = f(p_i)` merges
    /// key as `f`'s tree over `P(i)` on every edge.
    P(u32),
    /// Opaque non-`Value` edge arg (`TryCallRet`/`TryCallExn`), keyed by
    /// `(feeding inst, tag, payload)` — edge-scoped, so identity holds
    /// only for args carried by the same `BlockCall`. Two slots fed the
    /// same pseudo-arg dedup via `P`; a uniform-`O` slot can never
    /// materialize, so `mat_ok`/`emit_key` reject it.
    O(u64),
    /// "The value param `i` holds at this edge's source" — distinct
    /// from `P(i)` (the value *passed into* slot `i` on this edge):
    /// they coincide only when the edge echoes the param back. An arg
    /// computed from `p_i` keys with `Q(i)` leaves, which is how the
    /// relational merge recognizes recurrence steps like `p_j + 8` on
    /// a back-edge.
    Q(u32),
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
                Key::A(ty, std::rc::Rc::new(rk), k)
            };
        }
        let Some(di) = func.dfg.value_def(v).inst() else {
            return Key::V(v);
        };
        let mut args = |vs: &[Value]| -> std::rc::Rc<[Key]> {
            vs.iter()
                .map(|&a| {
                    let a = func.dfg.resolve_aliases(a);
                    go(func, nf, canon, memo, a, d + 1)
                })
                .collect()
        };
        let rty = func.dfg.value_type(v);
        // Non-pure insts — side effects, multi-results, defined-trapping
        // ops — stay opaque `V` leaves. Everything pure keys
        // structurally, with non-value fields packed into `extra` (tag
        // byte low, discriminator high) and >32-bit immediates as
        // trailing `K` pseudo-args. Purity mirrors
        // `inst_predicates::is_pure_for_egraph` (private upstream).
        let op = func.dfg.insts[di].opcode();
        let pure_load = match func.dfg.insts[di] {
            InstructionData::Load { flags, .. }
            | InstructionData::LoadNoOffset { flags, .. } => {
                let f = func.dfg.mem_flags[flags];
                f.readonly() && f.notrap() && f.can_move()
            }
            _ => false,
        };
        if func.dfg.inst_results(di).len() != 1
            || !(pure_load
                || (!op.can_load()
                    && !(op.is_call()
                        || op.is_branch()
                        || op.is_terminator()
                        || op.is_return()
                        || op.can_trap()
                        || op.other_side_effects()
                        || op.can_store())))
        {
            return Key::V(v);
        }
        match func.dfg.insts[di] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Key::K(rty, imm.bits()),
            InstructionData::Binary { opcode, args: a } => {
                Key::N(opcode, rty, args(&a), tag::BIN)
            }
            InstructionData::Unary { opcode, arg } => {
                Key::N(opcode, rty, args(&[arg]), tag::UNARY)
            }
            InstructionData::Ternary { opcode, args: a } => {
                Key::N(opcode, rty, args(&a), tag::TERN)
            }
            InstructionData::BinaryImm8 { opcode, arg, imm } => Key::N(
                opcode,
                rty,
                args(&[arg]),
                tag::BIMM8 | (imm as u64) << 8,
            ),
            InstructionData::IntCompare { opcode, args: a, cond } => Key::N(
                opcode,
                rty,
                args(&a),
                tag::ICMP | (cond as u64) << 8,
            ),
            InstructionData::FloatCompare { opcode, args: a, cond } => Key::N(
                opcode,
                rty,
                args(&a),
                tag::FCMP | (cond as u64) << 8,
            ),
            InstructionData::StackAddr {
                opcode,
                stack_slot,
                offset,
            } => Key::N(
                opcode,
                rty,
                std::rc::Rc::new([Key::K(
                    cranelift_codegen::ir::types::I32,
                    i32::from(offset) as i64,
                )]),
                tag::SADDR | (stack_slot.as_u32() as u64) << 8,
            ),
            InstructionData::Load {
                opcode,
                arg,
                flags,
                offset,
            } => {
                let mut a = args(&[arg]).to_vec();
                a.push(Key::K(
                    cranelift_codegen::ir::types::I32,
                    i32::from(offset) as i64,
                ));
                Key::N(
                    opcode,
                    rty,
                    std::rc::Rc::from(a),
                    tag::LOAD | (flags.as_u32() as u64) << 8,
                )
            }
            InstructionData::LoadNoOffset { opcode, arg, flags } => Key::N(
                opcode,
                rty,
                args(&[arg]),
                tag::LOADNOFF | (flags.as_u32() as u64) << 8,
            ),
            InstructionData::UnaryIeee16 { opcode, imm } => Key::N(
                opcode,
                rty,
                std::rc::Rc::new([Key::K(
                    cranelift_codegen::ir::types::I64,
                    imm.bits() as i64,
                )]),
                tag::UIEEE,
            ),
            InstructionData::UnaryIeee32 { opcode, imm } => Key::N(
                opcode,
                rty,
                std::rc::Rc::new([Key::K(
                    cranelift_codegen::ir::types::I64,
                    imm.bits() as i64,
                )]),
                tag::UIEEE,
            ),
            InstructionData::UnaryIeee64 { opcode, imm } => Key::N(
                opcode,
                rty,
                std::rc::Rc::new([Key::K(cranelift_codegen::ir::types::I64, imm.bits() as i64)]),
                tag::UIEEE,
            ),
            _ => Key::V(v),
        }
    }
    go(func, nf, canon, memo, v, 0)
}

/// `v`'s structural hash — exactly `khash_m(key_of(v))` would produce,
/// computed without materializing the `Key`: same alias/canon/nf
/// normalization, same tag packing, same `K` immediates. Callers use it
/// to gate *whether* a slot's keys are worth building at all — the
/// uniform-arg check and the size caps answer from hashes and capped
/// value-walks instead of building 10⁵-node trees that never merge.
/// Because it is by-construction the `khash_m` of the key, `hmap`
/// probes inside `paramize` stay consistent.
pub fn vhash(
    func: &Function,
    nf: &FxHashMap<Value, (Value, i64)>,
    canon: &FxHashMap<Value, Value>,
    memo: &mut FxHashMap<Value, u64>,
    v: Value,
) -> u64 {
    use std::hash::{Hash, Hasher};
    fn hv(v: Value) -> u64 {
        // khash_m's leaf arm: discriminant + fields.
        let mut h = rustc_hash::FxHasher::default();
        std::mem::discriminant(&Key::V(v)).hash(&mut h);
        v.hash(&mut h);
        h.finish()
    }
    fn hk(t: cranelift_codegen::ir::Type, i: i64) -> u64 {
        let mut h = rustc_hash::FxHasher::default();
        std::mem::discriminant(&Key::K(t, i)).hash(&mut h);
        t.hash(&mut h);
        i.hash(&mut h);
        h.finish()
    }
    fn go(
        func: &Function,
        nf: &FxHashMap<Value, (Value, i64)>,
        canon: &FxHashMap<Value, Value>,
        memo: &mut FxHashMap<Value, u64>,
        v: Value,
        d: usize,
    ) -> u64 {
        let v = func.dfg.resolve_aliases(v);
        let v = canon.get(&v).copied().unwrap_or(v);
        if d > 64 {
            return hv(v);
        }
        if let Some(&h) = memo.get(&v) {
            return h;
        }
        let h = go_uncached(func, nf, canon, memo, v, d);
        memo.insert(v, h);
        h
    }
    fn go_uncached(
        func: &Function,
        nf: &FxHashMap<Value, (Value, i64)>,
        canon: &FxHashMap<Value, Value>,
        memo: &mut FxHashMap<Value, u64>,
        v: Value,
        d: usize,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        if let Some(&(r, k)) = nf.get(&v)
            && r != v
        {
            let rh = go(func, nf, canon, memo, r, d + 1);
            let ty = func.dfg.value_type(v);
            if k == 0 {
                return rh;
            }
            // Key::A arm
            0u8.hash(&mut h);
            ty.hash(&mut h);
            rh.hash(&mut h);
            k.hash(&mut h);
            return h.finish();
        }
        let Some(di) = func.dfg.value_def(v).inst() else {
            return hv(v);
        };
        let rty = func.dfg.value_type(v);
        let op = func.dfg.insts[di].opcode();
        let pure_load = match func.dfg.insts[di] {
            InstructionData::Load { flags, .. }
            | InstructionData::LoadNoOffset { flags, .. } => {
                let f = func.dfg.mem_flags[flags];
                f.readonly() && f.notrap() && f.can_move()
            }
            _ => false,
        };
        if func.dfg.inst_results(di).len() != 1
            || !(pure_load
                || (!op.can_load()
                    && !(op.is_call()
                        || op.is_branch()
                        || op.is_terminator()
                        || op.is_return()
                        || op.can_trap()
                        || op.other_side_effects()
                        || op.can_store())))
        {
            return hv(v);
        }
        // Key::N arm: 1u8 + op + ty + args + extra — args are value
        // hashes followed by `K` immediates (each carrying its key type).
        let mut n = |extra: u64, vals: &[Value], imms: &[(cranelift_codegen::ir::Type, i64)]| {
            1u8.hash(&mut h);
            op.hash(&mut h);
            rty.hash(&mut h);
            for &a in vals {
                let a = func.dfg.resolve_aliases(a);
                go(func, nf, canon, memo, a, d + 1).hash(&mut h);
            }
            for &(t, i) in imms {
                hk(t, i).hash(&mut h);
            }
            extra.hash(&mut h);
            h.finish()
        };
        let i64t = cranelift_codegen::ir::types::I64;
        let i32t = cranelift_codegen::ir::types::I32;
        match func.dfg.insts[di] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => hk(rty, imm.bits()),
            InstructionData::Binary { args: a, .. } => n(tag::BIN, &a, &[]),
            InstructionData::Unary { arg, .. } => n(tag::UNARY, &[arg], &[]),
            InstructionData::Ternary { args: a, .. } => n(tag::TERN, &a, &[]),
            InstructionData::BinaryImm8 { arg, imm, .. } => {
                n(tag::BIMM8 | (imm as u64) << 8, &[arg], &[])
            }
            InstructionData::IntCompare { args: a, cond, .. } => {
                n(tag::ICMP | (cond as u64) << 8, &a, &[])
            }
            InstructionData::FloatCompare { args: a, cond, .. } => {
                n(tag::FCMP | (cond as u64) << 8, &a, &[])
            }
            InstructionData::StackAddr {
                stack_slot, offset, ..
            } => n(
                tag::SADDR | (stack_slot.as_u32() as u64) << 8,
                &[],
                &[(i32t, i32::from(offset) as i64)],
            ),
            InstructionData::Load {
                arg, flags, offset, ..
            } => n(
                tag::LOAD | (flags.as_u32() as u64) << 8,
                &[arg],
                &[(i32t, i32::from(offset) as i64)],
            ),
            InstructionData::LoadNoOffset { arg, flags, .. } => {
                n(tag::LOADNOFF | (flags.as_u32() as u64) << 8, &[arg], &[])
            }
            InstructionData::UnaryIeee16 { imm, .. } => {
                n(tag::UIEEE, &[], &[(i64t, imm.bits() as i64)])
            }
            InstructionData::UnaryIeee32 { imm, .. } => {
                n(tag::UIEEE, &[], &[(i64t, imm.bits() as i64)])
            }
            InstructionData::UnaryIeee64 { imm, .. } => {
                n(tag::UIEEE, &[], &[(i64t, imm.bits() as i64)])
            }
            _ => hv(v),
        }
    }
    go(func, nf, canon, memo, v, 0)
}

/// `k`'s node count were it built — `nodes(key_of(v))` without the
/// allocation. Same traversal as `vhash`: the `within` caps that gate
/// relational probing must see the key's size, not the def-DAG's.
pub fn vnodes(
    func: &Function,
    nf: &FxHashMap<Value, (Value, i64)>,
    canon: &FxHashMap<Value, Value>,
    memo: &mut FxHashMap<Value, usize>,
    v: Value,
) -> usize {
    fn go(
        func: &Function,
        nf: &FxHashMap<Value, (Value, i64)>,
        canon: &FxHashMap<Value, Value>,
        memo: &mut FxHashMap<Value, usize>,
        v: Value,
        d: usize,
    ) -> usize {
        let v = func.dfg.resolve_aliases(v);
        let v = canon.get(&v).copied().unwrap_or(v);
        if d > 64 {
            return 1;
        }
        if let Some(&n) = memo.get(&v) {
            return n;
        }
        let n = unc(func, nf, canon, memo, v, d);
        memo.insert(v, n);
        n
    }
    fn unc(
        func: &Function,
        nf: &FxHashMap<Value, (Value, i64)>,
        canon: &FxHashMap<Value, Value>,
        memo: &mut FxHashMap<Value, usize>,
        v: Value,
        d: usize,
    ) -> usize {
        if let Some(&(r, k)) = nf.get(&v)
            && r != v
        {
            let n = go(func, nf, canon, memo, r, d + 1);
            return if k == 0 { n } else { n + 1 };
        }
        let Some(di) = func.dfg.value_def(v).inst() else {
            return 1;
        };
        let op = func.dfg.insts[di].opcode();
        let pure_load = match func.dfg.insts[di] {
            InstructionData::Load { flags, .. }
            | InstructionData::LoadNoOffset { flags, .. } => {
                let f = func.dfg.mem_flags[flags];
                f.readonly() && f.notrap() && f.can_move()
            }
            _ => false,
        };
        if func.dfg.inst_results(di).len() != 1
            || !(pure_load
                || (!op.can_load()
                    && !(op.is_call()
                        || op.is_branch()
                        || op.is_terminator()
                        || op.is_return()
                        || op.can_trap()
                        || op.other_side_effects()
                        || op.can_store())))
        {
            return 1;
        }
        match func.dfg.insts[di] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                ..
            } => 1,
            InstructionData::Binary { args, .. }
            | InstructionData::IntCompare { args, .. }
            | InstructionData::FloatCompare { args, .. } => {
                let a0 = func.dfg.resolve_aliases(args[0]);
                let a1 = func.dfg.resolve_aliases(args[1]);
                1 + go(func, nf, canon, memo, a0, d + 1)
                    + go(func, nf, canon, memo, a1, d + 1)
            }
            InstructionData::Ternary { args, .. } => {
                let a0 = func.dfg.resolve_aliases(args[0]);
                let a1 = func.dfg.resolve_aliases(args[1]);
                let a2 = func.dfg.resolve_aliases(args[2]);
                1 + go(func, nf, canon, memo, a0, d + 1)
                    + go(func, nf, canon, memo, a1, d + 1)
                    + go(func, nf, canon, memo, a2, d + 1)
            }
            InstructionData::Unary { arg, .. } | InstructionData::BinaryImm8 { arg, .. } => {
                let a = func.dfg.resolve_aliases(arg);
                1 + go(func, nf, canon, memo, a, d + 1)
            }
            // `K` imm pseudo-args each count as a node.
            InstructionData::StackAddr { .. }
            | InstructionData::UnaryIeee16 { .. }
            | InstructionData::UnaryIeee32 { .. }
            | InstructionData::UnaryIeee64 { .. } => 2,
            InstructionData::Load { arg, .. } => {
                let a = func.dfg.resolve_aliases(arg);
                2 + go(func, nf, canon, memo, a, d + 1)
            }
            InstructionData::LoadNoOffset { arg, .. } => {
                let a = func.dfg.resolve_aliases(arg);
                1 + go(func, nf, canon, memo, a, d + 1)
            }
            _ => 1,
        }
    }
    go(func, nf, canon, memo, v, 0)
}

/// `Key::N`'s `extra` low byte — which `InstructionData` variant the
/// node was. `emit_key` needs the exact variant to rebuild; fields that
/// don't fit in args (cond codes, mem flags, lane indices) ride in the
/// remaining bits.
pub mod tag {
    pub const BIN: u64 = 0;
    pub const UNARY: u64 = 1;
    pub const TERN: u64 = 2;
    pub const ICMP: u64 = 3;
    pub const FCMP: u64 = 4;
    pub const BIMM8: u64 = 5;
    pub const SADDR: u64 = 8;
    pub const LOAD: u64 = 9;
    pub const UIEEE: u64 = 10;
    pub const LOADNOFF: u64 = 11;
}

/// Rewrite `k` slot-relative: any subtree equal to `slots[i]`'s key —
/// or naming `ps[i]` itself — becomes `P(i)`, "whatever arrived in slot
/// `i` on this edge"; a `V(p_i)` leaf becomes `Q(i)`, the param's
/// *source* value — it resolves through `pend[i]` and the `resolve_p`
/// stack cycle-rejects it, so a merge keyed on its own param can never
/// commit an emitted tree that would dangle once `p_i` is gone.
/// Subtrees match on keys, not values, so an edge arg like
/// `imul (iadd x,8), 2` paramizes against a sibling slot carrying
/// `iadd x,8` even though the SSA names differ. `skip` excludes the
/// slot being keyed from *arg-key* matching only (slot `i`'s own key
/// trivially equals itself). `kmap`/`vmap` are the O(1) lookup forms
/// of `slots`/`ps` — the linear scans with deep `Key` equality this
/// replaces were the pass's dominant cost on param-heavy blocks.
pub fn paramize<'k>(
    k: &Key,
    nslots: usize,
    slotk: &dyn Fn(usize) -> &'k Key,
    ps: &[Value],
    skip: usize,
    hmap: &FxHashMap<u64, u32>,
    vmap: &FxHashMap<Value, u32>,
    hmemo: &mut FxHashMap<*const Key, u64>,
) -> Key {
    // A subtree equal to a sibling slot's whole key is that slot's
    // incoming value — `P(i)`. `hmap` is keyed on *hashes* because a
    // `FxHashMap<&Key, _>` lookup hashes `k` — an O(subtree) walk
    // repeated at every recursion level made paramize quadratic on
    // deep keys. `hmemo` gives per-node hashing in O(1) amortized
    // (shared `Rc` subtrees hit the cache); a hash hit is verified
    // with `==` before rewriting, so stale entries from recycled
    // `Rc` addresses can only miss a match, never make a wrong one.
    // `slotk` materializes sibling keys lazily — a hit is the only
    // point they're needed.
    if let Some(&i) = hmap.get(&khash_m(k, hmemo)) {
        let mut i = i as usize;
        if i == skip || slotk(i) != k {
            // The first-match slot is the one being keyed, or the hash
            // collided — rescan for another slot with an equal key.
            match (0..nslots).find(|&j| j != skip && slotk(j) == k) {
                Some(j) => i = j,
                None => i = usize::MAX,
            }
        }
        if i != usize::MAX {
            return Key::P(i as u32);
        }
    }
    if let Key::V(w) = k
        && let Some(&i) = vmap.get(w)
    {
        return Key::Q(i);
    }
    match k {
        Key::A(t, rk, off) => Key::A(
            *t,
            std::rc::Rc::new(paramize(rk, nslots, slotk, ps, skip, hmap, vmap, hmemo)),
            *off,
        ),
        Key::N(op, t, args, extra) => Key::N(
            *op,
            *t,
            args.iter()
                .map(|a| paramize(a, nslots, slotk, ps, skip, hmap, vmap, hmemo))
                .collect(),
            *extra,
        ),
        _ => k.clone(),
    }
}

/// Hash of an `O` pseudo-arg key — matches `khash_m(&Key::O(x))`, for
/// callers building hash maps without materializing the key.
pub fn ohash(x: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    std::mem::discriminant(&Key::O(x)).hash(&mut h);
    x.hash(&mut h);
    h.finish()
}

/// Memoized structural hash — `Rc`-shared subtrees hash once, so
/// `paramize`'s per-node slot probe is O(nodes) total instead of
/// O(nodes × depth). Keyed on node address; callers treat hits as
/// candidates and confirm with `==`.
pub fn khash_m(k: &Key, memo: &mut FxHashMap<*const Key, u64>) -> u64 {
    use std::hash::{Hash, Hasher};
    if let Some(&h) = memo.get(&(k as *const Key)) {
        return h;
    }
    let mut h = rustc_hash::FxHasher::default();
    match k {
        Key::A(t, b, off) => {
            0u8.hash(&mut h);
            t.hash(&mut h);
            khash_m(b, memo).hash(&mut h);
            off.hash(&mut h);
        }
        Key::N(op, t, args, extra) => {
            1u8.hash(&mut h);
            op.hash(&mut h);
            t.hash(&mut h);
            for a in args.iter() {
                khash_m(a, memo).hash(&mut h);
            }
            extra.hash(&mut h);
        }
        _ => {
            std::mem::discriminant(k).hash(&mut h);
            match k {
                Key::V(v) => v.hash(&mut h),
                Key::K(t, i) => {
                    t.hash(&mut h);
                    i.hash(&mut h);
                }
                Key::P(i) | Key::Q(i) => i.hash(&mut h),
                Key::O(x) => x.hash(&mut h),
                _ => {}
            }
        }
    }
    let h = h.finish();
    memo.insert(k as *const Key, h);
    h
}

/// Rewrite `V` leaves naming `ps` members through `rep` — under a
/// param bisimulation hypothesis a leaf referencing slot `i` becomes
/// its class leader's leaf, so per-edge signatures compare structurally
/// (a back-edge arg `p_j` keys as `V(rep)` just like `p_i`'s does).
pub fn key_rep(k: &Key, ps: &[Value], rep: &dyn Fn(Value) -> Value) -> Key {
    match k {
        Key::V(w) if ps.contains(w) => Key::V(rep(*w)),
        Key::A(t, rk, o) => Key::A(*t, std::rc::Rc::new(key_rep(rk, ps, rep)), *o),
        Key::N(op, t, args, x) => Key::N(
            *op,
            *t,
            args.iter().map(|a| key_rep(a, ps, rep)).collect(),
            *x,
        ),
        _ => k.clone(),
    }
}

/// Peel an `A`-chain to (base, total offset). Non-`A` keys peel to
/// `(k, 0)`.
pub fn peel_off(k: &Key) -> (&Key, i64) {
    match k {
        Key::A(_, b, o) => {
            let (r, o2) = peel_off(b);
            (r, o + o2)
        }
        _ => (k, 0),
    }
}

/// Structural size — `Key` nodes, shared `Rc` subtrees counted per
/// reference (the same measure `==` and `key_sub` pay per visit). Used
/// to gate the relational oracles: relations worth materializing are
/// small, and walking 10⁴-node keys quadratically per edge is not.
pub fn nodes(k: &Key) -> usize {
    match k {
        Key::A(_, b, _) => 1 + nodes(b),
        Key::N(_, _, args, _) => 1 + args.iter().map(nodes).sum::<usize>(),
        _ => 1,
    }
}

/// `nodes(k) <= cap`, but bails the moment the running count passes
/// `cap` — callers gating on a size bound don't pay to count the rest
/// of a huge key.
pub fn within(k: &Key, cap: usize) -> bool {
    fn go(k: &Key, left: &mut usize) -> bool {
        if *left == 0 {
            return false;
        }
        *left -= 1;
        match k {
            Key::A(_, b, _) => go(b, left),
            Key::N(_, _, args, _) => args.iter().all(|a| go(a, left)),
            _ => true,
        }
    }
    let mut left = cap + 1;
    go(k, &mut left)
}

/// The set of slot indices `k` references — `P` and `Q` leaves both
/// name slot `i`'s incoming value, so both count.
pub fn pset(k: &Key) -> FxHashSet<u32> {
    let mut s = FxHashSet::default();
    fn go(k: &Key, s: &mut FxHashSet<u32>) {
        match k {
            Key::P(i) | Key::Q(i) => {
                s.insert(*i);
            }
            Key::A(_, b, _) => go(b, s),
            Key::N(_, _, args, _) => args.iter().for_each(|a| go(a, s)),
            _ => {}
        }
    }
    go(k, &mut s);
    s
}

/// The affine slope of `k` with respect to slot `i` — `Some(c)` when
/// `k` is linear in `i`'s incoming value (`P(i)`/`Q(i)` → 1, other
/// leaves → 0, `iadd`/`isub`/`A` combine pointwise). `None` when slot
/// `i` appears under a non-affine op where no constant slope exists.
pub fn slope(k: &Key, i: u32) -> Option<i64> {
    match k {
        Key::P(x) | Key::Q(x) => Some((*x == i) as i64),
        Key::A(_, b, _) => slope(b, i),
        Key::N(op, _, args, _) => {
            if !pset(k).contains(&i) {
                Some(0)
            } else if args.len() == 2 {
                match op {
                    Opcode::Iadd => Some(slope(&args[0], i)? + slope(&args[1], i)?),
                    Opcode::Isub => Some(slope(&args[0], i)? - slope(&args[1], i)?),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => Some(0),
    }
}

/// `x - y` as a key: the semantic-difference oracle relparm's `diff_b`
/// implements over SSA values, lifted to key space. Recognizes the
/// algebraic shapes — `y = x - d`, `x = y + d`, the nested
/// `y0 + (x - d)` byte-length form, affine cancellation, and
/// shared-operand strips `f(a, c) - f(b, c) = a - b` — through equal
/// *keys*, so congruent-but-distinct SSA names cancel too. `ty` types
/// the zero/const results; `depth` bounds the rewrite recursion.
pub fn key_sub(x: &Key, y: &Key, ty: cranelift_codegen::ir::Type, depth: u32) -> Option<Key> {
    key_sub_h(x, y, ty, depth, &mut FxHashMap::default())
}

/// `key_sub` with a shared `khash_m` memo: every structural `==` is
/// gated on subtree-hash equality first, turning the failed compares
/// that dominate wide sibling probes into O(1) lookups. Hash-equal
/// pairs still run `==` — the gate only ever skips certain negatives.
pub fn key_sub_h(
    x: &Key,
    y: &Key,
    ty: cranelift_codegen::ir::Type,
    depth: u32,
    hm: &mut FxHashMap<*const Key, u64>,
) -> Option<Key> {
    let heq = |a: &Key, b: &Key, hm: &mut FxHashMap<*const Key, u64>| {
        khash_m(a, hm) == khash_m(b, hm) && a == b
    };
    let k0 = || Some(Key::K(ty, 0));
    if heq(x, y, hm) {
        return k0();
    }
    if depth == 0 {
        return None;
    }
    fn isub(k: &Key) -> Option<(&Key, &Key)> {
        match k {
            Key::N(Opcode::Isub, _, a, _) => Some((&a[0], &a[1])),
            _ => None,
        }
    }
    fn iadd(k: &Key) -> Option<(&Key, &Key)> {
        match k {
            Key::N(Opcode::Iadd, _, a, _) => Some((&a[0], &a[1])),
            _ => None,
        }
    }
    // `y = 0` ⇒ x - y = x.
    if matches!(y, Key::K(_, 0)) {
        return Some(x.clone());
    }
    // `y = x - d` ⇒ x - y = d.
    if let Some((a, d)) = isub(y)
        && heq(a, x, hm)
    {
        return Some(d.clone());
    }
    // `x = y + d` ⇒ x - y = d.
    if let Some((a, d)) = iadd(x) {
        if heq(a, y, hm) {
            return Some(d.clone());
        }
        if heq(d, y, hm) {
            return Some(a.clone());
        }
    }
    // `y = y0 + (x - d)` ⇒ x - y = d - y0; `x = x0 + (y - d)` ⇒ x - y = x0 - d.
    for (o, m) in iadd(y).into_iter().flat_map(|(o, m)| [(o, m), (m, o)]) {
        if let Some((a, d)) = isub(m)
            && heq(a, x, hm)
        {
            return key_sub_h(d, o, ty, depth - 1, hm);
        }
    }
    for (o, m) in iadd(x).into_iter().flat_map(|(o, m)| [(o, m), (m, o)]) {
        if let Some((a, d)) = isub(m)
            && heq(a, y, hm)
        {
            return key_sub_h(o, d, ty, depth - 1, hm);
        }
    }
    // Affine cancellation: matching roots drop to the offset difference.
    let (bx, ox) = peel_off(x);
    let (by, oy) = peel_off(y);
    if heq(bx, by, hm) && (ox != 0 || oy != 0) {
        return Some(Key::K(ty, ox - oy));
    }
    // Shared-operand strip: `(a + c) - (b + c) = a - b`,
    // `(a - c) - (b - c) = a - b`.
    match (x, y) {
        (Key::N(Opcode::Iadd, _, xa, _), Key::N(Opcode::Iadd, _, ya, _)) => {
            for ((a, c), (b, c2)) in [
                ((xa[0].clone(), xa[1].clone()), (ya[0].clone(), ya[1].clone())),
                ((xa[0].clone(), xa[1].clone()), (ya[1].clone(), ya[0].clone())),
                ((xa[1].clone(), xa[0].clone()), (ya[0].clone(), ya[1].clone())),
                ((xa[1].clone(), xa[0].clone()), (ya[1].clone(), ya[0].clone())),
            ] {
                if heq(&c, &c2, hm) {
                    return key_sub_h(&a, &b, ty, depth - 1, hm);
                }
            }
        }
        (Key::N(Opcode::Isub, _, xa, _), Key::N(Opcode::Isub, _, ya, _))
            if heq(&xa[1], &ya[1], hm) =>
        {
            return key_sub_h(&xa[0], &ya[0], ty, depth - 1, hm);
        }
        _ => {}
    }
    None
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
        Key::P(i) | Key::Q(i) => {
            if stack.contains(i) {
                return None;
            }
            stack.push(*i);
            let r = resolve(*i, stack);
            stack.pop();
            r?
        }
        Key::A(t, rk, off) => Key::A(*t, std::rc::Rc::new(resolve_p(rk, resolve, stack)?), *off),
        Key::N(op, t, args, extra) => {
            let mut as_ = Vec::with_capacity(args.len());
            for a in args.iter() {
                as_.push(resolve_p(a, resolve, stack)?);
            }
            Key::N(*op, *t, std::rc::Rc::from(as_), *extra)
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

    /// `vhash` must equal `khash_m(key_of(v))` — the lazy gates in
    /// `webmerge` only ever *miss* merges on a mismatch (soundness is
    /// re-verified structurally), but a drifted normalization would
    /// silently degrade every `hmap` probe. Cross-check both paths on a
    /// function touching every keyed variant.
    #[test]
    fn vhash_matches_key_hash() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let x = func.dfg.append_block_param(entry, types::I64);
        let y = func.dfg.append_block_param(entry, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        // Iconst / Binary / BinaryImm8 / Unary / Ternary / compares /
        // imm pseudo-args all get exercised.
        let k7 = cur.ins().iconst(types::I64, 7);
        let a = cur.ins().iadd(x, y);
        let s = cur.ins().isub(a, k7);
        let m = cur.ins().imul(s, x);
        let three = cur.ins().iconst(types::I64, 3);
        let sh = cur.ins().ushr(m, three);
        let n = cur.ins().ineg(sh);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, m, k7);
        let sel = cur.ins().select(c, n, a);
        let band = cur.ins().band(sel, m);
        let fc = cur.ins().f64const(1.5);
        cur.ins().return_(&[band]);
        let _ = fc;

        let nf = web_nf(&func);
        let canon = canon_map(&func);
        let mut km: FxHashMap<Value, Key> = FxHashMap::default();
        let mut hm: FxHashMap<Value, u64> = FxHashMap::default();
        let mut nm: FxHashMap<Value, usize> = FxHashMap::default();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    let k = key_of(&func, &nf, &canon, &mut km, v);
                    // `k` is a stack temp — `khash_m`'s pointer-keyed
                    // memo would alias its reused address, so hash with
                    // a fresh memo (children's `Rc` targets are stable).
                    assert_eq!(
                        vhash(&func, &nf, &canon, &mut hm, v),
                        khash_m(&k, &mut FxHashMap::default()),
                        "vhash drifted from key_of on {v}"
                    );
                    assert_eq!(
                        vnodes(&func, &nf, &canon, &mut nm, v),
                        nodes(&k),
                        "vnodes drifted on {v}"
                    );
                }
            }
        }
    }
}

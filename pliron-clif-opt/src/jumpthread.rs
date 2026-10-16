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
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, ExceptionTableItem, Function, Inst, InstBuilder, InstructionData,
    Opcode, ProgramPoint, Type, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

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
pub fn known(func: &Function, env: &FxHashMap<Value, u64>, v: Value) -> Option<u64> {
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
pub fn eval(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    nz: &dyn Fn(Value) -> bool,
    rg: &dyn Fn(Value) -> Option<(u64, u64)>,
    inst: Inst,
) -> Option<u64> {
    static ARITH: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| crate::pass_enabled("PLIRON_EVAL_ARITH"));
    let res = func.dfg.first_result(inst);
    let w = bits(func, res)?;
    let r = match func.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits() as u64,
        InstructionData::IntCompare { cond, args, .. } => {
            let aw = bits(func, args[0])?;
            let ranged = range_cmp(
                cond,
                known(func, env, args[0]),
                known(func, env, args[1]),
                rg(args[0]),
                rg(args[1]),
                aw,
            );
            match (known(func, env, args[0]), known(func, env, args[1])) {
                _ if ranged.is_some() => u64::from(ranged == Some(true)),
                (Some(a), Some(b)) => icmp(cond, a, b, aw) as u64,
                (Some(0), None) | (None, Some(0)) => {
                    let x = if known(func, env, args[0]).is_some() {
                        args[1]
                    } else {
                        args[0]
                    };
                    match (cond, nz(x)) {
                        (IntCC::Equal, true) => 0,
                        (IntCC::NotEqual, true) => 1,
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        InstructionData::Unary { opcode, arg } => {
            let aw = bits(func, arg)?;
            let a = known(func, env, arg)?;
            match opcode {
                Opcode::Uextend | Opcode::Ireduce => a,
                Opcode::Sextend => sext(a, aw) as u64,
                Opcode::Bnot => !a,
                Opcode::Ineg => a.wrapping_neg(),
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
                Opcode::Imul if *ARITH => a.wrapping_mul(b),
                Opcode::Udiv if *ARITH && b != 0 => a / b,
                Opcode::Urem if *ARITH && b != 0 => a % b,
                Opcode::Ishl if *ARITH => a << (b & u64::from(w - 1)),
                Opcode::Ushr if *ARITH => a >> (b & u64::from(w - 1)),
                Opcode::Sshr if *ARITH => (sext(a, w) >> (b & u64::from(w - 1))) as u64,
                Opcode::Rotl | Opcode::Rotr if *ARITH => {
                    let s = (b & u64::from(w - 1)) as u32;
                    let (l, r) = if opcode == Opcode::Rotl {
                        (s, w - s)
                    } else {
                        (w - s, s)
                    };
                    if s == 0 { a } else { (a << l) | (a >> r) }
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(mask(r, w))
}

type KBits = FxHashMap<Value, (u64, u64)>;

/// Known `(zero, one)` bit masks of `v` from constants, `env`, `kenv` and
/// bitwise ops / constant shifts / extends on them.
fn kbits(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    kenv: &KBits,
    v: Value,
    depth: u32,
) -> (u64, u64) {
    let v = func.dfg.resolve_aliases(v);
    let Some(w) = bits(func, v) else {
        return (0, 0);
    };
    let full = mask(u64::MAX, w);
    if let Some(c) = known(func, env, v) {
        return (!c & full, c);
    }
    if let Some(&k) = kenv.get(&v) {
        return k;
    }
    let Some(i) = func.dfg.value_def(v).inst() else {
        return (0, 0);
    };
    if depth >= 6 {
        return (0, 0);
    }
    let sub = |x: Value| kbits(func, env, kenv, x, depth + 1);
    let op = |opcode: Opcode, a: (u64, u64), b: (u64, u64)| -> (u64, u64) {
        let s = (b.0 | b.1 == full)
            .then_some(b.1)
            .filter(|&s| s < u64::from(w));
        match (opcode, s) {
            (Opcode::Band, _) => (a.0 | b.0, a.1 & b.1),
            (Opcode::Bor, _) => (a.0 & b.0, a.1 | b.1),
            (Opcode::Bxor, _) => ((a.0 & b.0) | (a.1 & b.1), (a.0 & b.1) | (a.1 & b.0)),
            (Opcode::Ishl, Some(s)) => ((a.0 << s) | ((1u64 << s) - 1), a.1 << s),
            (Opcode::Ushr, Some(s)) => ((a.0 >> s) | !(full >> s), a.1 >> s),
            _ => (0, 0),
        }
    };
    let r = match func.dfg.insts[i] {
        InstructionData::Binary { opcode, args } => op(opcode, sub(args[0]), sub(args[1])),
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => {
            let a = sub(arg);
            let aw = bits(func, arg).unwrap_or(w);
            (a.0 | (full & !mask(u64::MAX, aw)), a.1)
        }
        InstructionData::Unary {
            opcode: Opcode::Ireduce,
            arg,
        } => sub(arg),
        // Zero-extending loads: the upper bits are known zero.
        InstructionData::Load { opcode, .. } => match opcode {
            Opcode::Uload8 => (full & !0xFFu64, 0),
            Opcode::Uload16 => (full & !0xFFFFu64, 0),
            Opcode::Uload32 => (full & !0xFFFF_FFFFu64, 0),
            _ => (0, 0),
        },
        _ => (0, 0),
    };
    (r.0 & full, r.1 & full)
}

/// Constant result of body instruction `i` from known bits: fully known
/// values, and `x ==/!= k` where a known bit of `x` differs from `k`.
fn kfold(func: &Function, env: &FxHashMap<Value, u64>, kenv: &KBits, i: Inst) -> Option<u64> {
    if kenv.is_empty() {
        return None;
    }
    if let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] {
        if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
            return None;
        }
        let (x, k) = match (known(func, env, args[0]), known(func, env, args[1])) {
            (None, Some(k)) => (args[0], k),
            (Some(k), None) => (args[1], k),
            _ => return None,
        };
        let (z, o) = kbits(func, env, kenv, x, 0);
        return ((o & !k) | (z & k) != 0).then_some(u64::from(cond == IntCC::NotEqual));
    }
    let r = func.dfg.first_result(i);
    let full = mask(u64::MAX, bits(func, r)?);
    let (z, o) = kbits(func, env, kenv, r, 0);
    (z | o == full).then_some(o)
}

pub fn pure_op(func: &Function, inst: Inst) -> bool {
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
            | Ishl
            | Ushr
            | StackAddr
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

/// Values known non-null: `!nonnull` loads, inbounds offsets of non-null
/// pointers, and block params whose every incoming argument is non-null
/// (greatest fixpoint).
pub struct NonNull<'a> {
    loads: &'a FxHashSet<Value>,
    derived: &'a FxHashMap<Value, Value>,
    params: FxHashSet<Value>,
    /// `PLIRON_NN_ADDR`, read once — `pass_enabled` does a TLS-tracked
    /// lookup per call, which is too hot inside `is_deps`.
    nn_addr: bool,
}

impl<'a> NonNull<'a> {
    pub fn new(
        func: &Function,
        loads: &'a FxHashSet<Value>,
        derived: &'a FxHashMap<Value, Value>,
    ) -> Self {
        let mut nn = NonNull {
            loads,
            derived,
            params: FxHashSet::default(),
            nn_addr: crate::pass_enabled("PLIRON_NN_ADDR"),
        };
        if loads.is_empty() {
            return nn;
        }
        let entry = func.layout.entry_block();
        for b in func.layout.blocks() {
            if Some(b) != entry {
                nn.params.extend(
                    func.dfg
                        .block_params(b)
                        .iter()
                        .copied()
                        .filter(|&p| func.dfg.value_type(p) == types::I64),
                );
            }
        }
        // Each seeded param survives iff every edge arg feeding it
        // proves non-null on that edge. Evaluate each (param, edge)
        // check once and record which params the proof relied on;
        // evictions then propagate only to dependent checks instead of
        // rescanning every terminator per fixpoint round. `params`
        // only shrinks, so a positive `params` hit is the only kind of
        // dependence — a miss can never become a hit.
        struct Chk {
            p: Value,
            t: Inst,
            ei: usize,
            v: Value,
            deps: Vec<Value>,
        }
        let mut checks: Vec<Chk> = Vec::new();
        let mut dependents: FxHashMap<Value, Vec<usize>> = FxHashMap::default();
        let mut evict: Vec<Value> = Vec::new();
        // Reused proof-scratch set — `Chk` keeps a Vec copy, so the set
        // itself is per-(param, edge) scratch, not a fresh alloc.
        let mut deps = FxHashSet::default();
        // `is_deps` results are stable while `params` is frozen — memoize
        // so a value feeding many edges is proven once. Cleared before the
        // eviction pass, which re-proves against the shrinking set.
        let mut memo = Some(FxHashMap::default());
        for b in func.layout.blocks() {
            let Some(t) = func.layout.last_inst(b) else {
                continue;
            };
            for (ei, bc) in func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .into_iter()
                .enumerate()
            {
                let d = bc.block(&func.dfg.value_lists);
                // Most edges feed blocks with no candidate params — collect
                // the edge args only once one matches.
                let mut args: Option<Vec<BlockArg>> = None;
                for (i, &p) in func.dfg.block_params(d).iter().enumerate() {
                    if !nn.params.contains(&p) {
                        continue;
                    }
                    let args = args.get_or_insert_with(|| bc.args(&func.dfg.value_lists).collect());
                    let Some(&BlockArg::Value(v)) = args.get(i) else {
                        evict.push(p);
                        continue;
                    };
                    let v = func.dfg.resolve_aliases(v);
                    deps.clear();
                    if nn.edge_aux_deps(func, t, ei, v, 0, &mut deps, &mut memo) {
                        dependents_entry(&mut dependents, &deps, checks.len());
                        checks.push(Chk {
                            p,
                            t,
                            ei,
                            v,
                            deps: deps.iter().copied().collect(),
                        });
                    } else {
                        evict.push(p);
                    }
                }
            }
        }
        memo = None;
        let mut dead: FxHashSet<Value> = FxHashSet::default();
        while let Some(q) = evict.pop() {
            if pliron_cfg::tracing_on() { tracing::trace!(target: "pliron::jumpthread", "nn evict {q}") };
            nn.params.remove(&q);
            dead.insert(q);
            for ci in dependents
                .get(&q)
                .into_iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>()
            {
                let c = &mut checks[ci];
                if dead.contains(&c.p) {
                    continue;
                }
                deps.clear();
                if nn.edge_aux_deps(func, c.t, c.ei, c.v, 0, &mut deps, &mut memo) {
                    // Still proven — the dep set may have moved, so
                    // re-point the check at what it now relies on.
                    c.deps = deps.iter().copied().collect();
                    dependents_entry(&mut dependents, &deps, ci);
                } else {
                    dead.insert(c.p);
                    evict.push(c.p);
                }
            }
        }
        nn
    }

    pub fn is(&self, func: &Function, v: Value) -> bool {
        self.is_deps(func, v, &mut FxHashSet::default())
    }

    /// `is_deps` behind a per-value memo — legal only while `params` is
    /// frozen (the `new` seeding pass); eviction shrinks `params`, which
    /// can change answers, so callers there pass `None`.
    fn is_deps_m(
        &self,
        func: &Function,
        v: Value,
        deps: &mut FxHashSet<Value>,
        memo: Option<&mut FxHashMap<Value, (bool, Vec<Value>)>>,
    ) -> bool {
        let Some(m) = memo else {
            return self.is_deps(func, v, deps);
        };
        let v = func.dfg.resolve_aliases(v);
        if let Some((r, ds)) = m.get(&v) {
            deps.extend(ds.iter().copied());
            return *r;
        }
        let mut d2 = FxHashSet::default();
        let r = self.is_deps(func, v, &mut d2);
        deps.extend(d2.iter().copied());
        m.insert(v, (r, d2.into_iter().collect()));
        r
    }

    fn is_deps(&self, func: &Function, v: Value, deps: &mut FxHashSet<Value>) -> bool {
        // `select c, a, b` is non-null iff both arms are; every term on
        // the worklist must justify itself (a `!nonnull` load, a param
        // the fixpoint kept, a symbol/stack address, or an `inbounds`
        // offset of a non-null base).
        let mut work = vec![func.dfg.resolve_aliases(v)];
        let mut seen: FxHashSet<Value> = FxHashSet::default();
        'terms: while let Some(mut v) = work.pop() {
            if !seen.insert(v) {
                continue;
            }
            for _ in 0..8 {
                if self.loads.contains(&v)
                    || addr_of_symbol(func, v, self.nn_addr)
                    || non_zero_const(func, v)
                {
                    continue 'terms;
                }
                if self.params.contains(&v) {
                    deps.insert(v);
                    continue 'terms;
                }
                if let Some(i) = func.dfg.value_def(v).inst()
                    && let InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args,
                    } = func.dfg.insts[i]
                {
                    work.push(func.dfg.resolve_aliases(args[1]));
                    work.push(func.dfg.resolve_aliases(args[2]));
                    continue 'terms;
                }
                match self.derived_base(func, v) {
                    Some(b) => {
                        v = b;
                        continue;
                    }
                    _ => return false,
                }
            }
            // Derived chain too deep to prove.
            return false;
        }
        true
    }
}

fn dependents_entry(
    dependents: &mut FxHashMap<Value, Vec<usize>>,
    deps: &FxHashSet<Value>,
    ci: usize,
) {
    for &q in deps {
        let l = dependents.entry(q).or_default();
        if !l.contains(&ci) {
            l.push(ci);
        }
    }
}

/// `iconst` that stays non-zero after masking to its type width — the
/// `NonNull::dangling()` (=1) a `Vec::new`/`str::new` iterator merges
/// with the real data pointer.
fn non_zero_const(func: &Function, v: Value) -> bool {
    let Some(i) = func.dfg.value_def(v).inst() else {
        return false;
    };
    let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = func.dfg.insts[i]
    else {
        return false;
    };
    mask(imm.bits() as u64, bits(func, v).unwrap_or(64)) != 0
}

/// `nr` clones `o` — carry `derived`'s inbounds mark over, renaming the
/// recorded base through `map`. Tombstones propagate deliberately:
/// `NonNull` recovers the live base positionally from the clone's def.
pub fn prop_derived(
    dfg: &cranelift_codegen::ir::DataFlowGraph,
    derived: &mut FxHashMap<Value, Value>,
    o: Value,
    nr: Value,
    map: &FxHashMap<Value, Value>,
) {
    let Some(&b) = derived.get(&o) else {
        return;
    };
    let b = dfg
        .value_alias_dest_for_serialization(b)
        .filter(|&d| d != Value::reserved_value())
        .map(|d| map.get(&dfg.resolve_aliases(d)).copied().unwrap_or(d))
        .unwrap_or(b);
    derived.insert(nr, b);
}

fn is_iconst(func: &Function, v: Value) -> bool {
    func.dfg.resolved_value_def(v).inst().is_some_and(|i| {
        matches!(
            func.dfg.insts[i],
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                ..
            }
        )
    })
}

fn zero_iconst(func: &Function, v: Value) -> bool {
    func.dfg.resolved_value_def(v).inst().is_some_and(|i| {
        matches!(
            func.dfg.insts[i],
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm
            } if imm.bits() == 0
        )
    })
}

impl NonNull<'_> {
    /// Debug: is `v` an admitted non-null param?
    pub fn params_debug(&self, _func: &Function, v: Value) -> bool {
        self.params.contains(&v)
    }

    /// Debug: `derived` base for `v`, if present.
    pub fn derived_debug(&self, func: &Function, v: Value) -> Option<Value> {
        self.derived.get(&func.dfg.resolve_aliases(v)).copied()
    }

    /// Is `v` provably non-null on edge `ei` of terminator `t`? Beyond
    /// the context-free `is`, the edge's own condition carries facts:
    /// `brif (icmp eq s,0)`'s false arm means `s != 0`; when `s` is a
    /// niche select `select(k,0,x)` it also means `!k` and `x != 0`
    /// — the shape `Option::is_none` produces at loop tails.
    pub fn is_edge(&self, func: &Function, t: Inst, ei: usize, v: Value) -> bool {
        let mut memo = None;
        self.edge_aux_deps(
            func,
            t,
            ei,
            func.dfg.resolve_aliases(v),
            0,
            &mut FxHashSet::default(),
            &mut memo,
        )
    }

    fn edge_aux_deps(
        &self,
        func: &Function,
        t: Inst,
        ei: usize,
        v: Value,
        d: usize,
        deps: &mut FxHashSet<Value>,
        memo: &mut Option<FxHashMap<Value, (bool, Vec<Value>)>>,
    ) -> bool {
        if self.is_deps_m(func, v, deps, memo.as_mut()) {
            return true;
        }
        if d > 6 {
            return false;
        }
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[t] else {
            return false;
        };
        let Some(ci) = func.dfg.resolved_value_def(c).inst() else {
            return false;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            args: cmp_args,
            cond,
        } = func.dfg.insts[ci]
        else {
            return false;
        };
        if cond != IntCC::Equal && cond != IntCC::NotEqual {
            return false;
        }
        // The edge must assert `s != 0` for the compared operand `s`.
        let (s, nz) = if zero_iconst(func, cmp_args[1]) {
            (cmp_args[0], (cond == IntCC::Equal) == (ei != 0))
        } else if zero_iconst(func, cmp_args[0]) {
            (cmp_args[1], (cond == IntCC::Equal) == (ei != 0))
        } else {
            return false;
        };
        // On this edge `s` is nonzero iff `nz`.
        if !nz {
            return false;
        }
        let s = func.dfg.resolve_aliases(s);
        if v == s {
            return true;
        }
        // `s = select(k, 0, x)`: nonzero gives `!k` and `x != 0`.
        let Some(si) = func.dfg.value_def(s).inst() else {
            return false;
        };
        let InstructionData::Ternary {
            opcode: Opcode::Select,
            args: sa,
        } = func.dfg.insts[si]
        else {
            return false;
        };
        if !zero_iconst(func, sa[1]) {
            return false;
        }
        if v == func.dfg.resolve_aliases(sa[2]) {
            return true;
        }
        // `v = select(k, _, b)`: `!k` on this edge collapses v to b.
        if let Some(vi) = func.dfg.value_def(v).inst()
            && let InstructionData::Ternary {
                opcode: Opcode::Select,
                args: va,
            } = func.dfg.insts[vi]
            && func.dfg.resolve_aliases(va[0]) == func.dfg.resolve_aliases(sa[0])
        {
            return self.edge_aux_deps(
                func,
                t,
                ei,
                func.dfg.resolve_aliases(va[2]),
                d + 1,
                deps,
                memo,
            );
        }
        // `v` an inbounds offset of a base that is nn on this edge.
        if let Some(base) = self.derived_base(func, v) {
            return self.edge_aux_deps(
                func,
                t,
                ei,
                func.dfg.resolve_aliases(base),
                d + 1,
                deps,
                memo,
            );
        }
        false
    }

    /// `derived[v]` with a live base: entries go stale as passes alias
    /// or delete the recorded base — resolve it, and when the target is
    /// a tombstone recover positionally from the def: an inbounds
    /// `iadd`/`isub` offset's base is its single non-const operand.
    fn derived_base(&self, func: &Function, v: Value) -> Option<Value> {
        let &b = self.derived.get(&v)?;
        // Bases are recorded at lowering and can alias later params —
        // chase the chain to its canonical value; a tombstone (alias to
        // the reserved slot) means the recorded base was deleted, so
        // recover positionally below.
        let mut b = b;
        for _ in 0..16 {
            match func.dfg.value_alias_dest_for_serialization(b) {
                Some(d) if d != Value::reserved_value() => b = d,
                _ => break,
            }
        }
        if func.dfg.value_is_real(b) {
            return Some(b);
        }
        let i = func.dfg.value_def(v).inst()?;
        let InstructionData::Binary {
            opcode: Opcode::Iadd | Opcode::Isub,
            args,
        } = func.dfg.insts[i]
        else {
            return None;
        };
        let mut nc = args.iter().filter(|&&a| !is_iconst(func, a));
        let (Some(&b0), None) = (nc.next(), nc.next()) else {
            return None;
        };
        Some(func.dfg.resolve_aliases(b0))
    }
}

/// Addresses of symbols, thread-locals and stack slots are never null.
fn addr_of_symbol(func: &Function, v: Value, nn_addr: bool) -> bool {
    nn_addr
        && func.dfg.value_def(v).inst().is_some_and(|i| {
            matches!(
                func.dfg.insts[i].opcode(),
                Opcode::TlsValue | Opcode::SymbolValue | Opcode::FuncAddr | Opcode::StackAddr
            )
        })
}

/// Trace the `src → dst` edge at destination index `di` back through
/// single-predecessor `jump` chains to the `brif` whose arm selects it.
/// Returns `(guard_block, condition, taken_when_true)`.
fn edge_guard(
    func: &Function,
    cfg: &ControlFlowGraph,
    mut src: Block,
    mut di: usize,
) -> Option<(Block, Value, bool)> {
    for _ in 0..64 {
        let t = func.layout.last_inst(src)?;
        match func.dfg.insts[t] {
            InstructionData::Brif { arg: c, .. } => return Some((src, c, di == 0)),
            InstructionData::Jump { .. } => {
                // `src` falls through unconditionally; its single
                // predecessor's terminator holds the guard.
                let mut preds = cfg.pred_iter(src);
                let p = preds.next()?;
                if preds.next().is_some() {
                    return None;
                }
                let dests = func.dfg.insts[p.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                // If `src` is reachable on more than one arm the edge
                // carries no arm information.
                let mut hit = dests
                    .iter()
                    .enumerate()
                    .filter(|(_, bc)| bc.block(&func.dfg.value_lists) == src)
                    .map(|(i, _)| i);
                di = hit.next()?;
                if hit.next().is_some() {
                    return None;
                }
                src = p.block;
            }
            _ => return None,
        }
    }
    None
}

/// `v` is provably `!= k` (`k` masked to `v`'s width): constants,
/// zero-extend loads, known bits (`band`/`bor`/shifts), selects whose
/// arms both qualify, or a block param in `cand` (the greatest-fixpoint
/// set `ne_params` computes — iterator decode pipelines route values
/// through cycles of params like `Chars`' forwarder merges, which a
/// plain recursion can't prove).
fn ne_const(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    cand: &FxHashSet<Value>,
    v: Value,
    k: u64,
    depth: u32,
) -> bool {
    ne_const_deps(func, env, cand, v, k, depth, &mut FxHashSet::default())
}

fn ne_const_deps(
    func: &Function,
    env: &FxHashMap<Value, u64>,
    cand: &FxHashSet<Value>,
    v: Value,
    k: u64,
    depth: u32,
    deps: &mut FxHashSet<Value>,
) -> bool {
    if depth >= 8 {
        return false;
    }
    let v = func.dfg.resolve_aliases(v);
    let Some(w) = bits(func, v) else {
        return false;
    };
    let k = mask(k, w);
    if let Some(c) = known(func, env, v) {
        return c != k;
    }
    match func.dfg.value_def(v) {
        ValueDef::Param(..) => {
            let ok = cand.contains(&v);
            if ok {
                deps.insert(v);
            }
            ok
        }
        ValueDef::Result(i, _) => match func.dfg.insts[i].opcode() {
            // Zero-extend loads clear every bit above the load width.
            Opcode::Uload8 => k >> 8 != 0,
            Opcode::Uload16 => k >> 16 != 0,
            Opcode::Uload32 => k >> 32 != 0,
            Opcode::Select => {
                let InstructionData::Ternary { args: sa, .. } = func.dfg.insts[i] else {
                    return false;
                };
                ne_const_deps(func, env, cand, sa[1], k, depth + 1, deps)
                    && ne_const_deps(func, env, cand, sa[2], k, depth + 1, deps)
            }
            _ => {
                let kenv = FxHashMap::default();
                let (z, o) = kbits(func, env, &kenv, v, 0);
                ((o & !k) | (z & k)) != 0
            }
        },
        _ => false,
    }
}

/// Greatest fixpoint: block params whose every value is provably `!= k`
/// (`k` masked per param width). Every param starts a candidate; one is
/// dropped when a non-self incoming arg can't be shown `!= k` (self edges
/// preserve the value, adding no candidates — cycles of provable params
/// survive the fixpoint).
fn ne_params(
    func: &Function,
    cfg: &ControlFlowGraph,
    env: &FxHashMap<Value, u64>,
    k: u64,
) -> FxHashSet<Value> {
    // Edges out of unreachable blocks contribute no runtime values; the
    // pass runs before dead-block sweeps, so count only reachable preds.
    let mut reach: FxHashSet<Block> = FxHashSet::default();
    let mut work = Vec::new();
    if let Some(e) = func.layout.entry_block() {
        reach.insert(e);
        work.push(e);
    }
    while let Some(b) = work.pop() {
        for s in cfg.succ_iter(b) {
            if reach.insert(s) {
                work.push(s);
            }
        }
    }
    let mut args_of: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    let mut cand: FxHashSet<Value> = FxHashSet::default();
    for b in func.layout.blocks() {
        if !reach.contains(&b) {
            continue;
        }
        for (pidx, &p) in func.dfg.block_params(b).iter().enumerate() {
            if bits(func, p).is_none() {
                continue;
            }
            let mut args = Vec::new();
            let mut ok = true;
            for pred in cfg.pred_iter(b) {
                if !reach.contains(&pred.block) {
                    continue;
                }
                let dests = func.dfg.insts[pred.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                for bc in dests {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    match bc.args(&func.dfg.value_lists).nth(pidx) {
                        Some(BlockArg::Value(a)) => {
                            let a = func.dfg.resolve_aliases(a);
                            if a != p {
                                args.push(a);
                            }
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    break;
                }
            }
            // No non-self args (entry params, pure self-loops): nothing
            // proves `!= k`.
            if ok && !args.is_empty() {
                args_of.insert(p, args);
                cand.insert(p);
            }
        }
    }
    // Evaluate every candidate's args once. A param arg is proven only
    // by its own candidacy (`ValueDef::Param => cand.contains`), so the
    // deps a check recorded are exactly the params whose eviction
    // re-fails it — the fixpoint is worklist propagation, not rescans.
    let mut dependents: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    let mut work: Vec<Value> = Vec::new();
    for (&p, args) in &args_of {
        let mut fail = false;
        let mut pd: Vec<Value> = Vec::new();
        for &a in args {
            let mut deps = FxHashSet::default();
            if !ne_const_deps(func, env, &cand, a, k, 0, &mut deps) {
                fail = true;
                break;
            }
            pd.extend(deps);
        }
        if fail {
            work.push(p);
        } else {
            for q in pd {
                dependents.entry(q).or_default().push(p);
            }
        }
    }
    while let Some(q) = work.pop() {
        if !cand.remove(&q) {
            continue;
        }
        for &p in dependents.get(&q).into_iter().flatten() {
            work.push(p);
        }
    }
    cand
}

/// `icmp eq/ne x, 0` with `x` known non-null → constant. Also the
/// `Option<NonNull>`-niche tests `icmp eq/ne (select c, 0, X), 0` (a
/// select is null exactly when `c` picks the zero arm) and the block-param
/// equivalent (a param is null exactly on zero-arg edges; when all edges
/// funnel through one `brif`, the test reduces to that condition) — the
/// shapes `slice::Iter::next()`/`Chars` leave after Option lowering.
/// The same select shape with a non-zero sentinel (`icmp eq/ne
/// (select c, X, K), K` with `X` provably `!= K`, e.g. `Option<char>`'s
/// `-1`) reduces to `!c`.
fn fold_null_tests(func: &mut Function, nn: &NonNull) -> usize {
    let none = FxHashMap::default();
    enum Fold {
        Const(i64),
        Alias(Value),
        Neg(Value),
        NN(Value),
    }
    // `brif`/`select` conditions are only guaranteed nonzero-vs-zero;
    // an `icmp` result is canonically 0/1, so aliasing `c` directly is
    // only safe when `c` itself is a canonical boolean.
    let booly = |func: &Function, v: Value| {
        func.dfg
            .value_def(v)
            .inst()
            .is_some_and(|i| matches!(func.dfg.insts[i].opcode(), Opcode::Icmp | Opcode::Fcmp))
    };
    let mut hits = Vec::new();
    let mut cfg_lazy: Option<ControlFlowGraph> = None;
    let mut dt_lazy: Option<DominatorTree> = None;
    // `ne_params` sets per sentinel constant, built on first use.
    let mut ne_sets: FxHashMap<u64, FxHashSet<Value>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for inst in func.layout.block_insts(b) {
            let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[inst] else {
                continue;
            };
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual)
                || func.dfg.value_type(args[0]).is_vector()
            {
                continue;
            }
            let ne = cond == IntCC::NotEqual;
            let (x, k) = match (known(func, &none, args[0]), known(func, &none, args[1])) {
                (Some(k), None) => (args[1], k),
                (None, Some(k)) => (args[0], k),
                _ => continue,
            };
            let Some(w) = bits(func, x) else {
                continue;
            };
            let k = mask(k, w);
            if k != 0 {
                if !crate::pass_enabled("PLIRON_SENTINEL") {
                    continue;
                }
                // `select c, X, K == K` iff `!c` when `X != K`;
                // `select c, K, X` mirrors. The sentinel test of a
                // niche-encoded enum (`Option<char>`'s `-1`).
                let x = func.dfg.resolve_aliases(x);
                let Some(d) = func.dfg.value_def(x).inst() else {
                    continue;
                };
                let InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args: sa,
                } = func.dfg.insts[d]
                else {
                    continue;
                };
                let cfg = cfg_lazy.get_or_insert_with(|| ControlFlowGraph::with_function(func));
                let cand = ne_sets
                    .entry(k)
                    .or_insert_with(|| ne_params(func, cfg, &none, k));
                let eq_is_c = if known(func, &none, sa[2]) == Some(k)
                    && ne_const(func, &none, cand, sa[1], k, 0)
                {
                    false
                } else if known(func, &none, sa[1]) == Some(k)
                    && ne_const(func, &none, cand, sa[2], k, 0)
                {
                    true
                } else {
                    continue;
                };
                let c = sa[0];
                if func.dfg.value_type(c) != func.dfg.value_type(func.dfg.first_result(inst)) {
                    continue;
                }
                // `x == K` ⇔ `eq_is_c ? c : !c`; `ne` inverts.
                let f = match (ne, eq_is_c) {
                    (false, true) | (true, false) if booly(func, c) => Fold::Alias(c),
                    (false, true) | (true, false) => Fold::NN(c),
                    _ => Fold::Neg(c),
                };
                hits.push((inst, f));
                continue;
            }
            if nn.is(func, x) {
                hits.push((inst, Fold::Const((cond == IntCC::NotEqual) as i64)));
                continue;
            }
            let x = func.dfg.resolve_aliases(x);
            // A block param is the other form Rust's niche layout takes
            // after SROA: `p` merges the `Some` pointer with the `None`
            // zero. `p == 0` iff a zero edge was taken; when every edge
            // funnels through the same `brif c`, the test is that
            // condition itself.
            if crate::pass_enabled("PLIRON_PARAMNULL")
                && let ValueDef::Param(d, pidx) = func.dfg.value_def(x)
            {
                let cfg = cfg_lazy.get_or_insert_with(|| ControlFlowGraph::with_function(func));
                let mut edges: Vec<(bool, bool)> = Vec::new();
                let mut guard: Option<(Block, Value)> = None;
                let mut ok = true;
                for pred in cfg.pred_iter(d) {
                    let dests = func.dfg.insts[pred.inst]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                    for (di, bc) in dests.iter().enumerate() {
                        if bc.block(&func.dfg.value_lists) != d {
                            continue;
                        }
                        let Some(BlockArg::Value(arg)) = bc.args(&func.dfg.value_lists).nth(pidx)
                        else {
                            ok = false;
                            break;
                        };
                        let Some((gb, c, arm)) = edge_guard(func, cfg, pred.block, di) else {
                            ok = false;
                            break;
                        };
                        match guard {
                            None => guard = Some((gb, c)),
                            Some((pb, pc)) if pb == gb && pc == c => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                        let arg = func.dfg.resolve_aliases(arg);
                        match known(func, &none, arg) {
                            Some(0) => edges.push((arm, true)),
                            Some(_) => edges.push((arm, false)),
                            None if nn.is(func, arg) => edges.push((arm, false)),
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        break;
                    }
                }
                if !ok {
                    continue;
                }
                let (mut zarm, mut saw_x) = (None, false);
                for &(arm, is_z) in &edges {
                    if is_z {
                        match zarm {
                            None => zarm = Some(arm),
                            Some(z) if z == arm => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    } else {
                        saw_x = true;
                    }
                }
                if !ok {
                    continue;
                }
                let Some(az) = zarm else {
                    // Every edge passes a non-null value — only meaningful
                    // with at least one edge (an entry param has none and
                    // would fold vacuously).
                    if !saw_x {
                        continue;
                    }
                    hits.push((inst, Fold::Const(ne as i64)));
                    continue;
                };
                if !saw_x || edges.iter().any(|&(a, z)| !z && a == az) {
                    continue;
                }
                let Some((_, c)) = guard else {
                    continue;
                };
                if func.dfg.value_type(c) != func.dfg.value_type(func.dfg.first_result(inst)) {
                    continue;
                }
                // `c` must dominate the test: every path to the merge
                // block passes the guard, so it does — except a guard
                // inside the merge block itself (self-edge), where the
                // compare could precede `c`'s def.
                let dt = dt_lazy.get_or_insert_with(|| DominatorTree::with_function(func, cfg));
                let pp = match func.dfg.value_def(c) {
                    ValueDef::Result(ci, _) => ProgramPoint::Inst(ci),
                    ValueDef::Param(cb, _) => ProgramPoint::Block(cb),
                    _ => continue,
                };
                if !dt.dominates(pp, inst, &func.layout) {
                    continue;
                }
                // `p == 0` ⇔ arm `az` taken ⇔ (az ? c : !c); `ne` inverts.
                let f = match (ne, az) {
                    (false, true) | (true, false) if booly(func, c) => Fold::Alias(c),
                    (false, true) | (true, false) => Fold::NN(c),
                    _ => Fold::Neg(c),
                };
                hits.push((inst, f));
                continue;
            }
            let Some(d) = func.dfg.value_def(x).inst() else {
                continue;
            };
            let InstructionData::Ternary {
                opcode: Opcode::Select,
                args: sa,
            } = func.dfg.insts[d]
            else {
                continue;
            };
            // `select c, 0, X == 0` iff `c` (X ≠ 0); `select c, X, 0` mirrors.
            let t0 = known(func, &none, sa[1]) == Some(0) && nn.is(func, sa[2]);
            let t1 = known(func, &none, sa[2]) == Some(0) && nn.is(func, sa[1]);
            let c = sa[0];
            let f = match (t0, t1, ne) {
                (true, _, false) | (false, true, true) if booly(func, c) => Fold::Alias(c),
                (true, _, false) | (false, true, true) => Fold::NN(c),
                (true, _, true) | (false, true, false) => Fold::Neg(c),
                _ => continue,
            };
            hits.push((inst, f));
        }
    }
    let n = hits.len();
    for (inst, f) in hits {
        match f {
            Fold::Const(k) => {
                let ty = func.dfg.value_type(func.dfg.first_result(inst));
                func.replace(inst).iconst(ty, k);
            }
            Fold::Alias(c) => {
                let res = func.dfg.first_result(inst);
                func.dfg.clear_results(inst);
                func.layout.remove_inst(inst);
                func.dfg.change_to_alias(res, c);
            }
            Fold::Neg(c) => {
                func.replace(inst).icmp_imm_s(IntCC::Equal, c, 0);
            }
            Fold::NN(c) => {
                func.replace(inst).icmp_imm_s(IntCC::NotEqual, c, 0);
            }
        }
    }
    n
}

/// `brif (icmp cc (select c, K, x), K)` where `x` isn't provably `!= K`
/// (`fold_null_tests` can't collapse the icmp — no `NonNull` on the arm).
/// Splitting the branch on `c` still kills the select, because every
/// rewritten edge knows which side of `c` it is on:
///
/// ```text
/// b:   brif c, K_target{sel→k_op, sel2→sentinel arm, …}, mid
/// mid: brif (icmp cc x, k_op), T{sel→x, sel2→x-side arm, …},
///                            F{same}
/// ```
///
/// `K_target` is the edge the original brif took when `sel == K` (`T` for
/// `eq`, `F` for `ne`). On the `mid` edges `sel` is `x`, so the original
/// compare survives verbatim as `icmp cc x, K`; on the direct edge `sel`
/// is `K`, so no test is needed at all. Every `select c,·,·` appearing in
/// a rewritten edge arg folds to the arm that edge's `c`-side picks, which
/// is what removes the whole `cmov`-cluster `Chars::next()`/
/// `split_terminator` iterators emit per element.
///
/// `sel` uses inside an outcome block can fold too: `K_target` sees
/// `sel == K` on every path into it (`x == K` on the mid edge is the same
/// value), `X_target` sees `sel == x` — provided the block had no other
/// preds, so `b`/`mid` alone decide entry.
pub fn split_niche_tests(func: &mut Function) -> usize {
    struct Plan {
        b: Block,
        term: Inst,
        icmp_inst: Inst,
        sel: Value,
        sel_inst: Inst,
        cond: IntCC,
        c: Value,
        k_op: Value,
        xop: Value,
        sent: usize,
        k_target: Block,
        x_target: Block,
        in_k: Vec<Inst>,
        in_x: Vec<Inst>,
        kargs: Vec<BlockArg>,
        xargs_t: Vec<BlockArg>,
        xargs_f: Vec<BlockArg>,
    }
    let none = FxHashMap::default();
    let mut changed = 0usize;
    loop {
        // Alias-resolved operand → every inst (incl. edge args) using it.
        let mut uses: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    uses.entry(func.dfg.resolve_aliases(v)).or_default().push(i);
                }
            }
        }
        let cfg = ControlFlowGraph::with_function(func);
        // Collect every viable split against the pre-apply state, then
        // apply them in a batch — the old loop rebuilt `uses` and the
        // cfg per accepted split. A plan goes stale if an earlier apply
        // rewrote one of its insts or gave a target block a new `mid`
        // pred its `sole_pred` check can't see, so those are deferred to
        // the next `outer` round.
        let mut plans: Vec<Plan> = Vec::new();
        for b in func.layout.blocks() {
            let Some(term) = func.layout.last_inst(b) else {
                continue;
            };
            let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[term] else {
                continue;
            };
            let Some(icmp_inst) = func.dfg.resolved_value_def(arg).inst() else {
                continue;
            };
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond,
                args: iargs,
            } = func.dfg.insts[icmp_inst]
            else {
                continue;
            };
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual)
                || func.dfg.value_type(iargs[0]).is_vector()
            {
                continue;
            }
            // The icmp must be a single-use test feeding this brif; a
            // shared compare would keep `sel` alive for its other use.
            if !matches!(
                uses.get(&func.dfg.resolve_aliases(arg))
                    .map(|v| v.as_slice()),
                Some([t]) if *t == term
            ) {
                continue;
            }
            // Normalize `x` to the select and `k` to the const arm.
            let (x, k) = match (known(func, &none, iargs[0]), known(func, &none, iargs[1])) {
                (Some(k), None) => (iargs[1], k),
                (None, Some(k)) => (iargs[0], k),
                _ => continue,
            };
            let sel = func.dfg.resolve_aliases(x);
            let Some(sel_inst) = func.dfg.value_def(sel).inst() else {
                continue;
            };
            let InstructionData::Ternary {
                opcode: Opcode::Select,
                args: sa,
            } = func.dfg.insts[sel_inst]
            else {
                continue;
            };
            let Some(w) = bits(func, sel) else {
                continue;
            };
            let k = mask(k, w);
            // Which select arm is the sentinel constant? `sel == K` on
            // that side of `c`; `x` is the other arm.
            let sent = if known(func, &none, sa[1]) == Some(k) {
                1
            } else if known(func, &none, sa[2]) == Some(k) {
                2
            } else {
                continue;
            };
            let (c, k_op, xop) = (sa[0], sa[sent], sa[3 - sent]);
            // Nested niche selects (`select (icmp sel,0), K, sel` — the
            // `Option<Option>` chains in `split_terminator`) would just
            // re-create this test one level down in `mid`: the inner
            // select stays, we add a block, and the next iteration splits
            // again. Require `x` to be a plain value.
            if let Some(xi) = func.dfg.resolved_value_def(xop).inst()
                && matches!(
                    func.dfg.insts[xi],
                    InstructionData::Ternary {
                        opcode: Opcode::Select,
                        ..
                    }
                )
            {
                continue;
            }
            // `K_target`: original edge taken when `sel == K`.
            let (k_call, x_call) = if cond == IntCC::Equal {
                (&blocks[0], &blocks[1])
            } else {
                (&blocks[1], &blocks[0])
            };
            let (k_target, x_target) = (
                k_call.block(&func.dfg.value_lists),
                x_call.block(&func.dfg.value_lists),
            );
            if k_target == x_target || k_target == b || x_target == b {
                continue;
            }
            // `sel` may only be used by the icmp, by this brif's edge
            // args, or inside an outcome block whose only pred is `b`
            // (so the rewrite leaves `b`/`mid` as its sole deciders).
            let mut in_k = Vec::new();
            let mut in_x = Vec::new();
            let mut ok = true;
            for &u in uses.get(&sel).map(|v| v.as_slice()).unwrap_or(&[]) {
                if u == icmp_inst || u == term {
                    continue;
                }
                let Some(ub) = func.layout.inst_block(u) else {
                    ok = false;
                    break;
                };
                if ub == k_target {
                    in_k.push(u);
                } else if ub == x_target {
                    in_x.push(u);
                } else {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            let sole_pred = |d: Block| {
                let mut n = 0;
                for p in cfg.pred_iter(d) {
                    if p.block != b {
                        return false;
                    }
                    n += 1;
                }
                n == 1
            };
            if (!in_k.is_empty() && !sole_pred(k_target))
                || (!in_x.is_empty() && !sole_pred(x_target))
            {
                continue;
            }
            // Measured: splitting pays when `sel` is purely test+dispatch
            // (chars_*/str iterators — the select only existed to encode
            // `None`, so every use folds and the cmov cluster dies). When
            // `sel` feeds real work inside an outcome block (tokenize's
            // `uload8`), the extra block boundary on the hot path costs
            // more than the removed cmov saves (~35% regression). Opt back
            // in with `PLIRON_SELTEST_INUSE=1`.
            if (!in_k.is_empty() || !in_x.is_empty())
                && !crate::config::config().is("PLIRON_SELTEST_INUSE", "1")
            {
                continue;
            }
            // `k_op`/`xop` must dominate their new use sites: both are
            // operands of the select in `b`'s test chain — defined at or
            // before the select, so they reach `mid` and both targets.
            // Build the rewritten edges.
            let kargs: Vec<BlockArg> = k_call
                .args(&func.dfg.value_lists)
                .map(|a| fold_sel_arg(func, a, c, sent))
                .collect();
            let xargs_t: Vec<BlockArg> = blocks[0]
                .args(&func.dfg.value_lists)
                .map(|a| fold_sel_arg(func, a, c, 3 - sent))
                .collect();
            let xargs_f: Vec<BlockArg> = blocks[1]
                .args(&func.dfg.value_lists)
                .map(|a| fold_sel_arg(func, a, c, 3 - sent))
                .collect();
            plans.push(Plan {
                b,
                term,
                icmp_inst,
                sel,
                sel_inst,
                cond,
                c,
                k_op,
                xop,
                sent,
                k_target,
                x_target,
                in_k,
                in_x,
                kargs,
                xargs_t,
                xargs_f,
            });
        }
        // Defer any plan whose key insts an earlier apply rewrote or
        // removed, or whose targets gained a `mid` pred — both make the
        // plan's collected facts stale. `Uses::new` is fresh each plan
        // anyway for the dead-select sweep; here it needs one build for
        // all of them.
        let mut mutated: FxHashSet<Inst> = FxHashSet::default();
        let mut claimed: FxHashSet<Block> = FxHashSet::default();
        let mut dead_sweep: Vec<Inst> = Vec::new();
        let prev = changed;
        for pl in plans {
            if claimed.contains(&pl.k_target)
                || claimed.contains(&pl.x_target)
                || [pl.term, pl.icmp_inst, pl.sel_inst]
                    .into_iter()
                    .chain(pl.in_k.iter().copied())
                    .chain(pl.in_x.iter().copied())
                    .any(|i| mutated.contains(&i))
            {
                continue;
            }
            claimed.extend([pl.k_target, pl.x_target]);
            mutated.extend(pl.in_k.iter().copied().chain(pl.in_x.iter().copied()));
            mutated.extend([pl.term, pl.icmp_inst, pl.sel_inst]);
            let Plan {
                b,
                term,
                icmp_inst,
                sel,
                sel_inst,
                cond,
                c,
                k_op,
                xop,
                sent,
                k_target,
                x_target,
                in_k,
                in_x,
                kargs,
                xargs_t,
                xargs_f,
            } = pl;
            // Rewrite in-block uses before touching edges: `k_target`
            // sees `sel == k_op`, `x_target` sees `sel == xop`.
            for (us, repl) in [(&in_k, k_op), (&in_x, xop)] {
                for &u in us {
                    let repl_map: FxHashMap<Value, Value> = func
                        .dfg
                        .inst_values(u)
                        .filter(|&v| func.dfg.resolve_aliases(v) == sel)
                        .map(|v| (v, repl))
                        .collect();
                    func.dfg
                        .map_inst_values(u, |v| repl_map.get(&v).copied().unwrap_or(v));
                }
            }
            if pliron_cfg::tracing_on() { tracing::debug!(
                target: "pliron::jumpthread",
                "seltest {b}: sel={sel} c={c} in_k={} in_x={} kargs={} xt={} xf={}",
                in_k.len(),
                in_x.len(),
                kargs.len(),
                xargs_t.len(),
                xargs_f.len()
            ) };
            // Splice `mid` in and re-point `b`'s brif at it. `blocks[0]`
            // is the cond-true arm: `k_target` when `cond == Equal`.
            let (t_blk, f_blk) = if cond == IntCC::Equal {
                (k_target, x_target)
            } else {
                (x_target, k_target)
            };
            let mid = func.dfg.make_block();
            func.layout.insert_block_after(mid, b);
            {
                let mut pos = FuncCursor::new(func).at_bottom(mid);
                let t = pos.ins().icmp(cond, xop, k_op);
                pos.ins().brif(t, t_blk, &xargs_t, f_blk, &xargs_f);
            }
            func.layout.remove_inst(term);
            let mut pos = FuncCursor::new(func).at_bottom(b);
            if sent == 1 {
                pos.ins().brif(c, k_target, &kargs, mid, &[]);
            } else {
                pos.ins().brif(c, mid, &[], k_target, &kargs);
            }
            // `sel`/icmp are dead now (all uses rewritten); the other
            // `select c`s folded out of the edge args may be too — queue
            // them for the batched sweep below.
            func.dfg.clear_results(icmp_inst);
            func.layout.remove_inst(icmp_inst);
            dead_sweep.push(sel_inst);
            for i in func.layout.block_insts(b).collect::<Vec<_>>() {
                if i == sel_inst {
                    continue;
                }
                if let InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } = func.dfg.insts[i]
                {
                    if func.dfg.resolve_aliases(args[0]) == func.dfg.resolve_aliases(c) {
                        dead_sweep.push(i);
                    }
                }
            }
            changed += 1;
        }
        if changed == prev {
            break;
        }
        // One use index for the whole sweep: removals only delete
        // uses, so a stale entry just skips a removal the next
        // outer round catches — a per-inst whole-function scan
        // was O(dead × fn).
        let uses = crate::uses::Uses::new(func);
        for di in dead_sweep {
            if func.layout.inst_block(di).is_none() {
                continue;
            }
            let r = func.dfg.resolve_aliases(func.dfg.first_result(di));
            if uses.is_used(r) {
                continue;
            }
            func.dfg.clear_results(di);
            func.layout.remove_inst(di);
        }
    }
    changed
}

/// On a rewritten niche-split edge `c`'s side is known: any
/// `select c, a, b` in the args folds to arm `side` (1 or 2).
fn fold_sel_arg(func: &Function, a: BlockArg, c: Value, side: usize) -> BlockArg {
    let BlockArg::Value(a) = a else {
        return a;
    };
    let r = func.dfg.resolve_aliases(a);
    let Some(i) = func.dfg.value_def(r).inst() else {
        return BlockArg::Value(a);
    };
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args: sa,
    } = func.dfg.insts[i]
    else {
        return BlockArg::Value(a);
    };
    if func.dfg.resolve_aliases(sa[0]) == func.dfg.resolve_aliases(c) {
        BlockArg::Value(sa[side])
    } else {
        BlockArg::Value(a)
    }
}

enum Arg {
    V(Value),
    K(u64, Type),
    /// Clone of a merge-block body inst, args already rewritten to `Arg`s
    /// resolved in the pred — materialized before `pinst` at apply time.
    R(Inst, Vec<Arg>),
}

/// Resolve `v` (a merge-block value) to something usable on the threaded
/// edge: an incoming edge arg, a known constant, a cloneable pure body
/// inst (bounded depth), or the value itself if its def dominates `pinst`.
fn map_val(
    func: &Function,
    argv: &FxHashMap<Value, Value>,
    body_vals: &FxHashSet<Value>,
    env: &FxHashMap<Value, u64>,
    dom: &DominatorTree,
    pinst: Inst,
    v: Value,
    out: &mut Vec<Arg>,
    depth: usize,
) -> bool {
    if let Some(&pv) = argv.get(&v) {
        out.push(Arg::V(pv));
        return true;
    }
    if !body_vals.contains(&v) {
        // An external value is a legal edge arg only if its def
        // dominates the threaded branch — dominating `b` is not enough
        // (`pinst` may have arms that never reach `b`'s dominator).
        let pp = match func.dfg.value_def(v) {
            ValueDef::Result(di, _) => ProgramPoint::Inst(di),
            ValueDef::Param(db, _) => ProgramPoint::Block(db),
            _ => return false,
        };
        if !dom.dominates(pp, pinst, &func.layout) {
            return false;
        }
        out.push(Arg::V(v));
        return true;
    }
    // `env` tracks 64 bits; it can't describe a wider param (and `iconst`
    // tops out at i64).
    if let Some(&k) = env.get(&v) {
        if func.dfg.value_type(v).bits() <= 64 {
            out.push(Arg::K(k, func.dfg.value_type(v)));
            return true;
        }
        return false;
    }
    if depth == 0 {
        return false;
    }
    let Some(di) = func.dfg.value_def(v).inst() else {
        return false;
    };
    if !pure_op(func, di) || func.dfg.inst_results(di).len() != 1 {
        return false;
    }
    let args: Vec<Value> = func.dfg.inst_args(di).to_vec();
    let mut subs = Vec::with_capacity(args.len());
    for a in args {
        let a = func.dfg.resolve_aliases(a);
        let mut tmp = Vec::with_capacity(1);
        if !map_val(
            func,
            argv,
            body_vals,
            env,
            dom,
            pinst,
            a,
            &mut tmp,
            depth - 1,
        ) {
            return false;
        }
        subs.push(tmp.pop().unwrap());
    }
    out.push(Arg::R(di, subs));
    true
}

/// Emit an `Arg` before `pinst`, cloning remat insts in order.
fn emit_arg(func: &mut Function, pinst: Inst, a: &Arg) -> Value {
    match a {
        Arg::V(v) => *v,
        Arg::K(k, ty) => FuncCursor::new(func)
            .at_inst(pinst)
            .ins()
            .iconst(*ty, *k as i64),
        Arg::R(di, subs) => {
            let vals: Vec<Value> = subs.iter().map(|s| emit_arg(func, pinst, s)).collect();
            let ni = func.dfg.clone_inst(*di);
            func.dfg.overwrite_inst_values(ni, vals.into_iter());
            func.layout.insert_inst(ni, pinst);
            func.dfg.first_result(ni)
        }
    }
}

/// A retargeted edge `pinst`'s destination `di` → `s`. With `repair`, the values
/// of the bypassed merge block that are used from `s` on become new params of
/// `s` (fed by the merge block's own branch `term`/`ti` and by the new edge).
struct Edit {
    pinst: Inst,
    di: usize,
    s: Block,
    args: Vec<Arg>,
    /// (merge term, arm, needed exports, region, other edges into `s`
    /// that must also feed the new params — (pred inst, dest idx)).
    repair: Option<(Inst, usize, Vec<Value>, Vec<Block>, Vec<(Inst, usize)>)>,
    /// Resolved args for each `repair` extra edge, filled in by the
    /// post-edit dominance validation (parallel to the `extra` list).
    eargs: Vec<Vec<Arg>>,
}

/// Blocks reachable from the entry without passing `defb`, in the graph
/// where each planned edit's `pinst -> s` edge already exists (the old
/// `pinst -> b` edges are kept too — an over-approximation that can only
/// reject, never accept wrongly).
fn reach_avoid(
    func: &Function,
    cfg: &ControlFlowGraph,
    new_succs: &FxHashMap<Block, Vec<Block>>,
    defb: Block,
) -> FxHashSet<Block> {
    let mut seen = FxHashSet::default();
    let Some(entry) = func.layout.entry_block() else {
        return seen;
    };
    if entry == defb {
        return seen;
    }
    seen.insert(entry);
    let mut work = vec![entry];
    while let Some(x) = work.pop() {
        let extra = new_succs.get(&x).into_iter().flatten().copied();
        for nx in cfg.succ_iter(x).chain(extra) {
            if nx != defb && seen.insert(nx) {
                work.push(nx);
            }
        }
    }
    seen
}

/// Constants a block's body computes without any incoming-edge context
/// (`iconst`s and pure folds of them). Those are the only merge values
/// safe to rematerialize on a repair edge when the original def stops
/// dominating it — a fresh `iconst` dominates every use by construction.
fn const_env(func: &Function, b: Block) -> FxHashMap<Value, u64> {
    let mut env = FxHashMap::default();
    let nz = |_: Value| false;
    let rg = |_: Value| None;
    for i in func.layout.block_insts(b) {
        if !pure_op(func, i) {
            continue;
        }
        if let Some(c) = eval(func, &env, &nz, &rg, i) {
            env.insert(func.dfg.first_result(i), c);
        }
    }
    env
}

/// Blocks reachable from `s` without entering `b`, or None past MAX_WALK.
fn region(cfg: &ControlFlowGraph, b: Block, s: Block) -> Option<Vec<Block>> {
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut order = Vec::new();
    let mut work = vec![s];
    while let Some(x) = work.pop() {
        if x == b || !seen.insert(x) {
            continue;
        }
        order.push(x);
        if order.len() > MAX_WALK {
            return None;
        }
        work.extend(cfg.succ_iter(x));
    }
    Some(order)
}

/// Thread one round; returns the number of edges retargeted.
pub(crate) fn round<'a>(
    func: &mut Function,
    loads: &'a FxHashSet<Value>,
    derived: &'a FxHashMap<Value, Value>,
    nn: &mut Option<NonNull<'a>>,
    domcond: bool,
    cfg: &ControlFlowGraph,
    domtree: &DominatorTree,
) -> usize {
    // Both whole-function indexes are consulted only once a candidate
    // edge actually threads — lazy so a no-work round is a linear scan.
    let mut users_l: Option<FxHashMap<Block, FxHashSet<Block>>> = None;
    let mut facts_l: Option<FxHashMap<Block, Cmp>> = None;
    let entry = func.layout.entry_block();
    let kb_on = crate::pass_enabled("PLIRON_KBITS");
    let range2 = crate::pass_enabled("PLIRON_RANGE2");
    let bodyk = crate::pass_enabled("PLIRON_BODYK");
    let dbg = pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::jumpthread", tracing::Level::DEBUG);
    let def_block = |v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    let mut edits: Vec<Edit> = Vec::new();
    let mut touched: FxHashSet<Inst> = FxHashSet::default();
    // Blocks taking part in any edit, and blocks no later edit may touch.
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut locked: FxHashSet<Block> = FxHashSet::default();
    // `region(b, s)` and `facts_at(pblock)` are stable for the whole edit-
    // collection phase (the cfg is immutable until edits apply below), but
    // were being recomputed per predecessor edge — a merge block has only
    // two `s` candidates yet one walk per pred. Memoize per round.
    let mut regions: FxHashMap<(Block, Block), Option<Vec<Block>>> = FxHashMap::default();
    let mut fact_memo: FxHashMap<Block, Vec<Cmp>> = FxHashMap::default();
    // Per-edge scratch, cleared and reused across (pred, dest) pairs —
    // these used to be five fresh HashMap allocations per edge.
    let mut args: Vec<BlockArg> = Vec::new();
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    let mut argv: FxHashMap<Value, Value> = FxHashMap::default();
    let mut pn: FxHashSet<Value> = FxHashSet::default();
    let mut kenv: KBits = FxHashMap::default();
    let mut prange: FxHashMap<Value, (u64, u64)> = FxHashMap::default();
    for b in func.layout.blocks() {
        if Some(b) == entry || locked.contains(&b) {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        let Some((&term, body)) = insts.split_last() else {
            continue;
        };
        // `iconst`s cost nothing to evaluate through the env map; budget
        // the real ops only (long niche-merge chains — `Option<char>`
        // flag test + sentinel compare — otherwise exceed MAX_BODY).
        let real = if bodyk {
            body.iter()
                .filter(|&&i| {
                    !matches!(
                        func.dfg.insts[i],
                        InstructionData::UnaryImm {
                            opcode: Opcode::Iconst,
                            ..
                        }
                    )
                })
                .count()
        } else {
            body.len()
        };
        if real > MAX_BODY || !body.iter().all(|&i| pure_op(func, i)) {
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
        let mut body_vals_l: Option<FxHashSet<Value>> = None;
        for pred in cfg.pred_iter(b) {
            let (pinst, pblock) = (pred.inst, pred.block);
            if pblock == b || touched.contains(&pinst) || locked.contains(&pblock) {
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
                args.clear();
                args.extend(bc.args(&func.dfg.value_lists));
                if args.len() != params.len() {
                    continue;
                }
                // A real candidate edge — the first one per round builds
                // the NonNull fixpoint; blocks failing the gates above
                // never reach it.
                let nnr = nn.get_or_insert_with(|| NonNull::new(func, loads, derived));
                env.clear();
                argv.clear();
                pn.clear();
                let mut ok = true;
                for (&p, a) in params.iter().zip(&args) {
                    let BlockArg::Value(v) = *a else {
                        ok = false;
                        break;
                    };
                    let v = func.dfg.resolve_aliases(v);
                    argv.insert(p, v);
                    if let Some(c) = known(func, &env, v) {
                        env.insert(p, c);
                    } else if nnr.is(func, v) {
                        pn.insert(p);
                    }
                }
                if !ok {
                    continue;
                }
                // Known bits of non-constant incoming args (e.g. a packed `Option` tag).
                kenv.clear();
                if kb_on {
                    let none = FxHashMap::default();
                    for (&p, &v) in &argv {
                        if !env.contains_key(&p) {
                            let k = kbits(func, &env, &none, v, 0);
                            if k != (0, 0) {
                                kenv.insert(p, k);
                            }
                        }
                    }
                }
                let nz = |v: Value| {
                    let v = func.dfg.resolve_aliases(v);
                    nnr.is(func, v) || pn.contains(&v)
                };
                // Ranges of the incoming args implied by branches dominating the pred.
                prange.clear();
                let fs = fact_memo.entry(pblock).or_insert_with(|| {
                    facts_at(
                        &domtree,
                        facts_l.get_or_insert_with(|| {
                            if domcond {
                                edge_facts(func, cfg)
                            } else {
                                FxHashMap::default()
                            }
                        }),
                        pblock,
                    )
                });
                if !fs.is_empty() {
                    for (&p, &v) in &argv {
                        if !env.contains_key(&p)
                            && let Some(r) = urange(func, &fs, v, 0, range2)
                        {
                            prange.insert(p, r);
                        }
                    }
                }
                let rg = |v: Value| prange.get(&func.dfg.resolve_aliases(v)).copied();
                for &i in body {
                    if let Some(c) =
                        eval(func, &env, &nz, &rg, i).or_else(|| kfold(func, &env, &kenv, i))
                    {
                        env.insert(func.dfg.first_result(i), c);
                    }
                }
                let taken = match known(func, &env, cond) {
                    Some(c) => c != 0,
                    None if nz(cond) => true,
                    None if !kenv.is_empty() && kbits(func, &env, &kenv, cond, 0).1 != 0 => true,
                    None => continue,
                };
                if dbg {
                    if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::jumpthread",
                        "jt take {pblock}->{b}->{s_tag} cond={cond} env={env:?} pn={pn:?} kenv={kenv:?} prange={prange:?} taken={taken}",
                        s_tag = if taken { "T" } else { "F" }
                    ) };
                    if let Some(di) = func.dfg.value_def(cond).inst() {
                        for v in func.dfg.inst_args(di) {
                            let r = func.dfg.resolve_aliases(*v);
                            if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::jumpthread",
                                "  jt arg {r}: loads={} params={} derived={:?}",
                                nnr.loads.contains(&r),
                                nnr.params.contains(&r),
                                nnr.derived.get(&r),
                            ) };
                        }
                    }
                }
                let ti = if taken { 0 } else { 1 };
                let t = targets[ti];
                let s = t.block(&func.dfg.value_lists);
                if s == b || locked.contains(&s) {
                    if dbg {
                        if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::jumpthread", "jt skip {b}->{s}: self/locked") };
                    }
                    continue;
                }
                let Some(reg) = regions
                    .entry((b, s))
                    .or_insert_with(|| region(&cfg, b, s))
                    .clone()
                else {
                    if dbg {
                        if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::jumpthread", "jt skip {b}->{s}: region>MAX_WALK") };
                    }
                    continue;
                };
                // B's values used from `s` on (resolved), in first-use order.
                let mut needed: Vec<Value> = Vec::new();
                if let Some(bu) = users_l.get_or_insert_with(|| users(func)).get(&b)
                    && reg.iter().any(|x| bu.contains(x))
                {
                    for &x in &reg {
                        if !bu.contains(&x) {
                            continue;
                        }
                        for inst in func.layout.block_insts(x) {
                            for v in func.dfg.inst_values(inst) {
                                let r = func.dfg.resolve_aliases(v);
                                if def_block(r) == Some(b) && !needed.contains(&r) {
                                    needed.push(r);
                                }
                            }
                        }
                    }
                }
                let repair = !needed.is_empty();
                // `s`'s other incoming edges (beyond the merge arm) must
                // also feed the new params: append the needed b-values
                // verbatim — legal when each def dominates that pred's
                // terminator (true for loop-carried exports like
                // tokenize's v1899, and always for the merge term's own
                // second arm when both arms land on `s`).
                let mut extra: Vec<(Inst, usize)> = Vec::new();
                if repair {
                    let mut okr = true;
                    for pr in cfg.pred_iter(s) {
                        let q = pr.inst;
                        let qdests = func.dfg.insts[q]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                        for (dj, qbc) in qdests.iter().enumerate() {
                            if qbc.block(&func.dfg.value_lists) != s || (q == term && dj == ti) {
                                continue;
                            }
                            for &w in &needed {
                                let pp = match func.dfg.value_def(w) {
                                    ValueDef::Result(di, _) => ProgramPoint::Inst(di),
                                    ValueDef::Param(db, _) => ProgramPoint::Block(db),
                                    _ => {
                                        okr = false;
                                        break;
                                    }
                                };
                                if !domtree.dominates(pp, q, &func.layout) {
                                    okr = false;
                                    break;
                                }
                            }
                            if !okr {
                                break;
                            }
                            extra.push((q, dj));
                        }
                        if !okr {
                            break;
                        }
                    }
                    if !okr {
                        if dbg {
                            if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::jumpthread", "jt skip {b}->{s} from {pblock}: extra edge can't supply exports") };
                        }
                        continue;
                    }
                }
                if repair
                    && (seen.contains(&b)
                        || seen.contains(&s)
                        || seen.contains(&pblock)
                        || reg.iter().any(|x| seen.contains(x))
                        || !reg
                            .iter()
                            .filter(|x| {
                                users_l
                                    .get_or_insert_with(|| users(func))
                                    .get(&b)
                                    .is_some_and(|bu| bu.contains(x))
                            })
                            .all(|&x| domtree.block_dominates(s, x)))
                {
                    if dbg {
                        if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::jumpthread",
                            "jt skip {b}->{s} from {pblock}: repair seen={}/{}/{} regseen={} dom={}",
                            seen.contains(&b),
                            seen.contains(&s),
                            seen.contains(&pblock),
                            reg.iter().any(|x| seen.contains(x)),
                            reg.iter()
                                .filter(|x| {
                                users_l
                                    .get_or_insert_with(|| users(func))
                                    .get(&b)
                                    .is_some_and(|bu| bu.contains(x))
                            })
                                .all(|&x| domtree.block_dominates(s, x))
                        ) };
                    }
                    continue;
                }
                let mut new = Vec::new();
                let body_vals = body_vals_l.get_or_insert_with(|| {
                    body.iter().map(|&i| func.dfg.first_result(i)).collect()
                });
                for a in t.args(&func.dfg.value_lists) {
                    let BlockArg::Value(v) = a else {
                        ok = false;
                        break;
                    };
                    ok &= map_val(
                        func,
                        &argv,
                        &body_vals,
                        &env,
                        &domtree,
                        pinst,
                        func.dfg.resolve_aliases(v),
                        &mut new,
                        4,
                    );
                }
                for &w in &needed {
                    ok &= map_val(
                        func, &argv, &body_vals, &env, &domtree, pinst, w, &mut new, 4,
                    );
                }
                if !ok {
                    continue;
                }
                touched.insert(pinst);
                seen.extend([b, s, pblock]);
                let repair = repair.then(|| {
                    touched.insert(term);
                    locked.extend([b, s, pblock]);
                    locked.extend(reg.iter().copied());
                    (term, ti, needed, reg, extra)
                });
                edits.push(Edit {
                    pinst,
                    di,
                    s,
                    args: new,
                    repair,
                    eargs: Vec::new(),
                });
                break;
            }
        }
    }
    // The batch's planned `pinst -> s` edges can open fresh paths that
    // bypass a def some appended edge arg relies on (an export fed
    // verbatim to `s`'s other preds dominated them only because every
    // route used to pass through `b` — think `s` looping back to its own
    // pred). Re-check every value placed on an edge against the
    // post-edit graph, rematerializing constants where possible and
    // dropping the edit otherwise.
    let mut new_succs: FxHashMap<Block, Vec<Block>> = FxHashMap::default();
    for e in &edits {
        if let Some(pb) = func.layout.inst_block(e.pinst) {
            new_succs.entry(pb).or_default().push(e.s);
        }
    }
    let mut memo: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
    let mut post_safe = |func: &Function, v: Value, u: Inst| -> bool {
        let pp = match func.dfg.value_def(v) {
            ValueDef::Result(di, _) => ProgramPoint::Inst(di),
            ValueDef::Param(db, _) => ProgramPoint::Block(db),
            _ => return false,
        };
        if !domtree.dominates(pp, u, &func.layout) {
            return false;
        }
        let Some(db) = def_block(v) else {
            return false;
        };
        let Some(ub) = func.layout.inst_block(u) else {
            return false;
        };
        if db == ub {
            // Dominance within the block was just checked: the def
            // sits before the terminator regardless of how the block
            // is reached.
            return true;
        }
        let r = memo
            .entry(db)
            .or_insert_with(|| reach_avoid(func, &cfg, &new_succs, db));
        !r.contains(&ub)
    };
    let mut benvs: FxHashMap<Block, FxHashMap<Value, u64>> = FxHashMap::default();
    let mut kept: Vec<Edit> = Vec::with_capacity(edits.len());
    'edits: for mut e in edits {
        // `V` leaves carried onto the retargeted edge itself.
        let mut bad = {
            let mut stack: Vec<&Arg> = e.args.iter().collect();
            let mut bad = false;
            while let Some(a) = stack.pop() {
                match a {
                    Arg::K(..) => {}
                    Arg::R(_, subs) => stack.extend(subs.iter()),
                    Arg::V(v) => {
                        let v = func.dfg.resolve_aliases(*v);
                        if !post_safe(func, v, e.pinst) {
                            bad = true;
                            break;
                        }
                    }
                }
            }
            bad
        };
        if !bad && let Some((term, _, needed, _, extra)) = &e.repair {
            let b = func.layout.inst_block(*term).unwrap();
            let benv = benvs.entry(b).or_insert_with(|| const_env(func, b));
            for &(q, _) in extra {
                let mut es = Vec::with_capacity(needed.len());
                for &w in needed {
                    let k = benv
                        .get(&w)
                        .filter(|_| func.dfg.value_type(w).bits() <= 64)
                        .copied();
                    if let Some(k) = k {
                        es.push(Arg::K(k, func.dfg.value_type(w)));
                    } else if post_safe(func, w, q) {
                        es.push(Arg::V(w));
                    } else {
                        bad = true;
                        break;
                    }
                }
                if bad {
                    break;
                }
                e.eargs.push(es);
            }
        }
        if bad {
            continue 'edits;
        }
        kept.push(e);
    }
    let n = kept.len();
    for e in kept {
        if let Some((term, ti, needed, reg, extra)) = e.repair {
            let mut to: FxHashMap<Value, Value> = FxHashMap::default();
            for &w in &needed {
                let ty = func.dfg.value_type(w);
                to.insert(w, func.dfg.append_block_param(e.s, ty));
            }
            for x in reg {
                let insts: Vec<Inst> = func.layout.block_insts(x).collect();
                for inst in insts {
                    let repl: FxHashMap<Value, Value> = func
                        .dfg
                        .inst_values(inst)
                        .filter_map(|v| to.get(&func.dfg.resolve_aliases(v)).map(|&n| (v, n)))
                        .collect();
                    if !repl.is_empty() {
                        func.dfg
                            .map_inst_values(inst, |v| repl.get(&v).copied().unwrap_or(v));
                    }
                }
            }
            // Every other edge into `s` feeds the new params — verbatim
            // b-values when they still dominate that edge post-edit,
            // freshly rematerialized constants otherwise (both resolved
            // during validation into `e.eargs`).
            for ((q, dj), es) in extra.iter().zip(&e.eargs) {
                let dest_ok = {
                    let dfg = &func.dfg;
                    dfg.insts[*q].branch_destination(&dfg.jump_tables, &dfg.exception_tables)[*dj]
                        .block(&dfg.value_lists)
                        == e.s
                };
                if !dest_ok {
                    continue;
                }
                let vals: Vec<Value> = es.iter().map(|a| emit_arg(func, *q, a)).collect();
                let dfg = &mut func.dfg;
                let bc = &mut dfg.insts[*q]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[*dj];
                for v in vals {
                    bc.append_argument(v, &mut dfg.value_lists);
                }
            }
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[term]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[ti];
            for &w in &needed {
                bc.append_argument(w, &mut dfg.value_lists);
            }
        }
        let mut args = Vec::with_capacity(e.args.len());
        for a in &e.args {
            args.push(BlockArg::Value(emit_arg(func, e.pinst, a)));
        }
        let bc = BlockCall::new(e.s, args, &mut func.dfg.value_lists);
        let dfg = &mut func.dfg;
        dfg.insts[e.pinst]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[e.di] = bc;
    }
    n
}

/// Splice `jump`-only successors that have no other predecessor into their
/// predecessor, so a merge block and its test end up in one block.
pub fn merge_chains(func: &mut Function) {
    let prof = pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::jumpthread", tracing::Level::DEBUG);
    let t0 = std::time::Instant::now();
    let cfg = ControlFlowGraph::with_function(func);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut gone: FxHashSet<Block> = FxHashSet::default();
    let (mut merges, mut moves) = (0usize, 0usize);
    let (mut t_cfg, mut t_alias, mut t_move) = (0u128, 0u128, 0u128);
    for x in blocks {
        if gone.contains(&x) {
            continue;
        }
        while let Some(term) = func.layout.last_inst(x) {
            let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
                break;
            };
            let y = destination.block(&func.dfg.value_lists);
            let tp = std::time::Instant::now();
            // `take(2)`, not `count()`: a shared merge block can have one
            // predecessor list entry per dispatch arm, and counting them
            // all per arm is O(arms x preds) on giant `br_table` fns.
            if y == x || Some(y) == entry || cfg.pred_iter(y).take(2).count() != 1 {
                break;
            }
            t_cfg += tp.elapsed().as_nanos();
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
            // An arg that is one of y's own params (a dead y->x->y cycle or
            // a param permutation) would alias a param to itself or a
            // sibling; resolve first since existing aliases can mask it.
            let pset: FxHashSet<Value> = func.dfg.block_params(y).iter().copied().collect();
            if args
                .iter()
                .any(|&a| pset.contains(&func.dfg.resolve_aliases(a)))
            {
                break;
            }
            let tp = std::time::Instant::now();
            let params = func.dfg.detach_block_params(y);
            let params: Vec<Value> = params.as_slice(&func.dfg.value_lists).to_vec();
            for (p, a) in params.into_iter().zip(args) {
                func.dfg.change_to_alias(p, a);
            }
            t_alias += tp.elapsed().as_nanos();
            func.layout.remove_inst(term);
            let tp = std::time::Instant::now();
            let insts: Vec<Inst> = func.layout.block_insts(y).collect();
            for i in insts {
                func.layout.remove_inst(i);
                func.layout.append_inst(i, x);
                moves += 1;
            }
            func.layout.remove_block(y);
            t_move += tp.elapsed().as_nanos();
            merges += 1;
            gone.insert(y);
        }
    }
    if prof {
        if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread",
            "  merge_chains total {:?} merges={merges} moves={moves} cfg={}ms alias={}ms move={}ms",
            t0.elapsed(),
            t_cfg / 1_000_000,
            t_alias / 1_000_000,
            t_move / 1_000_000
        ) };
    }
}

/// A dispatch block `y`'s successors may use `y`'s params and results
/// directly — which is exactly what strands them when an edge tries to
/// thread past `y`. Rebind each sole-pred successor `s`: append a param
/// for every `y` value `s`'s dominated cone consumes, feed it on `y`'s
/// edge, and rename the uses. Once the exports travel as edge args an
/// edgespec threading can land on `s` — an enum-discriminant dispatch
/// whose use-cone is small (a couple of epilogue blocks) then folds.
pub fn rebind_exports(func: &mut Function) -> usize {
    let prof2 = pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::jumpthread", tracing::Level::DEBUG);
    let t0 = std::time::Instant::now();
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    // Resolved use index — one O(fn) scan shared by every (y, s) cone
    // walk instead of re-walking `inst_values` per use.
    let uses = crate::uses::Uses::new(func);
    if prof2 {
        if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread",
            "    rebind setup {:.3}ms",
            t0.elapsed().as_secs_f64() * 1e3
        ) };
    }
    let t0 = std::time::Instant::now();
    let mut edits = Vec::new();
    for y in func.layout.blocks().collect::<Vec<_>>() {
        // Params then inst results — deterministic order.
        let mut exports: Vec<Value> = func.dfg.block_params(y).to_vec();
        for i in func.layout.block_insts(y) {
            exports.extend(func.dfg.inst_results(i).iter().copied());
        }
        let Some(term) = func.layout.last_inst(y) else {
            continue;
        };
        let dests = func.dfg.insts[term]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .to_vec();
        // Qualifying successors: `s`'s only pred edges must come from `y`'s
        // own terminator — which also means idom(s) == y, so `s` dominates a
        // site iff `s` is the node directly below `y` on the site's
        // dominator chain. Attribute each export use-site to that arm with
        // one idom walk; the old per-(succ, export, site)
        // `block_dominates` product was quadratic on giant dispatches
        // (arms × exports × sites × dom depth).
        let mut quals: Vec<Block> = Vec::new();
        let mut done: FxHashSet<Block> = FxHashSet::default();
        for bc in &dests {
            let s = bc.block(&func.dfg.value_lists);
            // Every edge into `s` must leave `y`'s terminator — `pred_iter`
            // counts instructions, so a brif with both arms on `s` still
            // qualifies (all its calls get args appended below).
            if s == y || !done.insert(s) || !cfg.pred_iter(s).all(|e| e.inst == term) {
                continue;
            }
            quals.push(s);
        }
        if quals.is_empty() {
            continue;
        }
        let qual: FxHashSet<Block> = quals.iter().copied().collect();
        // s -> w -> insts inside s's cone that use w.
        let mut cone: FxHashMap<Block, FxHashMap<Value, Vec<Inst>>> = FxHashMap::default();
        for &w in &exports {
            let ss = uses.sites(w);
            if ss.is_empty() {
                continue;
            }
            for &(b, i) in ss {
                if b == y {
                    continue;
                }
                let mut cur = b;
                let arm = loop {
                    match dom.idom(cur) {
                        Some(d) if d == y => break Some(cur),
                        Some(d) => cur = d,
                        None => break None,
                    }
                };
                let Some(s) = arm else {
                    continue;
                };
                if !qual.contains(&s) {
                    continue;
                }
                cone.entry(s).or_default().entry(w).or_default().push(i);
            }
        }
        for s in quals {
            let Some(ws) = cone.get(&s) else {
                continue;
            };
            // `y` values consumed from `s`'s dominated cone — `s` owns the
            // only path there, so a param can carry each.
            let needed: Vec<Value> = exports
                .iter()
                .copied()
                .filter(|w| ws.contains_key(w))
                .collect();
            if needed.len() > 8 {
                continue;
            }
            let insts: Vec<Inst> = needed.iter().flat_map(|w| ws[w].iter().copied()).collect();
            edits.push((y, term, s, needed, insts));
        }
    }
    if prof2 {
        if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread",
            "    rebind scan {:.3}ms (edits={})",
            t0.elapsed().as_secs_f64() * 1e3,
            edits.len()
        ) };
    }
    let t0 = std::time::Instant::now();
    let n = edits.len();
    // Edge args grouped by term — a giant dispatch has an edit per arm,
    // and a full destination rescan per edit is O(arms²).
    let mut appends: FxHashMap<Inst, Vec<(Block, Vec<Value>)>> = FxHashMap::default();
    for (_y, term, s, needed, insts) in edits {
        let mut repl: FxHashMap<Value, Value> = FxHashMap::default();
        for &w in &needed {
            let ty = func.dfg.value_type(w);
            let p = func.dfg.append_block_param(s, ty);
            repl.insert(w, p);
        }
        // Rewrite only the insts that reference a `needed` value inside
        // the cone — the site index says exactly where they are (a site
        // rewired by an earlier edit just produces an empty remap).
        let insts: FxHashSet<Inst> = insts.into_iter().collect();
        for i in insts {
            let to: FxHashMap<Value, Value> = func
                .dfg
                .inst_values(i)
                .filter_map(|v| repl.get(&func.dfg.resolve_aliases(v)).map(|&n| (v, n)))
                .collect();
            if !to.is_empty() {
                func.dfg
                    .map_inst_values(i, |v| to.get(&v).copied().unwrap_or(v));
            }
        }
        appends.entry(term).or_default().push((s, needed));
    }
    for (term, ss) in appends {
        // Per-call arg order is `ss`'s edit order for this block — each
        // successor appears once, so a lookup keeps it O(1) per call.
        let map: FxHashMap<Block, Vec<Value>> = ss.into_iter().collect();
        let dfg = &mut func.dfg;
        for bc in
            dfg.insts[term].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if let Some(needed) = map.get(&bc.block(&dfg.value_lists)) {
                for &w in needed {
                    bc.append_argument(w, &mut dfg.value_lists);
                }
            }
        }
    }
    if prof2 {
        if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "    rebind apply {:.3}ms", t0.elapsed().as_secs_f64() * 1e3) };
    }
    n
}

/// Clone a small pure dispatch block into a `jump` predecessor — the pred's
/// edge then carries the dispatch's test and branch args itself (the
/// `select→icmp` discriminant in the pred folds against the clone to a
/// raw `brif`). Legal only while every def of `y` is used inside `y` —
/// `rebind_exports` creates exactly that shape by moving shared uses
/// onto successor params.
pub fn dispatch_dup(func: &mut Function, dom: &DominatorTree) -> usize {
    let entry = func.layout.entry_block();
    // Values with any use outside their def block: a `y` whose defs all
    // stay internal can be cloned without stranding downstream uses.
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    for x in func.layout.blocks() {
        for i in func.layout.block_insts(x) {
            for v in func.dfg.inst_values(i) {
                let v = func.dfg.resolve_aliases(v);
                let db = match func.dfg.value_def(v) {
                    ValueDef::Param(b, _) => Some(b),
                    ValueDef::Result(di, _) => func.layout.inst_block(di),
                    _ => None,
                };
                if db.is_some_and(|b| b != x) {
                    escaping.insert(v);
                }
            }
        }
    }
    let mut edits = Vec::new();
    for p in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(p) else {
            continue;
        };
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let y = destination.block(&func.dfg.value_lists);
        if y == p || Some(y) == entry {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(y).collect();
        let Some((&yterm, body)) = insts.split_last() else {
            continue;
        };
        if body.len() > 12 || !body.iter().all(|&i| pure_op(func, i)) {
            continue;
        }
        if !matches!(
            func.dfg.insts[yterm],
            InstructionData::Brif { .. } | InstructionData::Jump { .. }
        ) {
            continue;
        }
        let params = func.dfg.block_params(y).to_vec();
        let args: Vec<Value> = destination
            .args(&func.dfg.value_lists)
            .filter_map(|a| match a {
                BlockArg::Value(v) => Some(v),
                _ => None,
            })
            .collect();
        if args.len() != params.len() {
            continue;
        }
        // A `y` param re-bound to `y` itself can't be cloned.
        let pset: FxHashSet<Value> = params.iter().copied().collect();
        if args
            .iter()
            .any(|&a| pset.contains(&func.dfg.resolve_aliases(a)))
        {
            continue;
        }
        let map: FxHashMap<Value, Value> = params.iter().copied().zip(args).collect();
        let mut ok = true;
        // `y` insts and branch args must resolve to clones or to values
        // dominating `p`'s tail.
        for i in insts.iter().take(body.len()).chain([&yterm]) {
            for v in func.dfg.inst_values(*i) {
                let v = func.dfg.resolve_aliases(v);
                if map.contains_key(&v) {
                    continue;
                }
                let okd = match func.dfg.value_def(v) {
                    ValueDef::Param(b, _) => {
                        b == y || (func.layout.is_block_inserted(b) && dom.block_dominates(b, p))
                    }
                    ValueDef::Result(di, _) => {
                        func.layout.inst_block(di) == Some(y)
                            || dom.dominates(di, term, &func.layout)
                    }
                    _ => false,
                };
                if !okd {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        // Nothing `y` defines may be observed outside `y`.
        if ok {
            for i in insts.iter().take(body.len()) {
                for r in func.dfg.inst_results(*i).iter() {
                    if escaping.contains(&func.dfg.resolve_aliases(*r)) {
                        ok = false;
                    }
                }
            }
            if params.iter().any(|pv| escaping.contains(pv)) {
                ok = false;
            }
        }
        if ok {
            edits.push((p, term, y, map));
        }
    }
    let mut n = 0;
    for (_p, term, y, mut map) in edits {
        let insts: Vec<Inst> = func.layout.block_insts(y).collect();
        let Some((&yterm, body)) = insts.split_last() else {
            continue;
        };
        for &i in body {
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
            func.layout.insert_inst(ni, term);
            let nrs = func.dfg.inst_results(ni).to_vec();
            for (r, nr) in func.dfg.inst_results(i).to_vec().into_iter().zip(nrs) {
                map.insert(r, nr);
            }
        }
        let sub =
            |bc: BlockCall, map: &FxHashMap<Value, Value>, func: &Function| -> Vec<BlockArg> {
                bc.args(&func.dfg.value_lists)
                    .map(|a| match a {
                        BlockArg::Value(v) => {
                            let v = func.dfg.resolve_aliases(v);
                            BlockArg::Value(*map.get(&v).unwrap_or(&v))
                        }
                        a => a,
                    })
                    .collect()
            };
        let mut cur = FuncCursor::new(func).at_inst(term);
        let made = match cur.func.dfg.insts[yterm] {
            InstructionData::Jump { destination, .. } => {
                let d = destination.block(&cur.func.dfg.value_lists);
                let args = sub(destination, &map, cur.func);
                Some(cur.ins().jump(d, &args))
            }
            InstructionData::Brif { arg, blocks, .. } => {
                let c = cur.func.dfg.resolve_aliases(arg);
                let c = *map.get(&c).unwrap_or(&c);
                let (tb, eb) = (
                    blocks[0].block(&cur.func.dfg.value_lists),
                    blocks[1].block(&cur.func.dfg.value_lists),
                );
                let ta = sub(blocks[0], &map, cur.func);
                let ea = sub(blocks[1], &map, cur.func);
                Some(cur.ins().brif(c, tb, &ta, eb, &ea))
            }
            _ => None,
        };
        if made.is_none() {
            continue;
        }
        func.layout.remove_inst(term);
        n += 1;
    }
    n
}

/// Retarget edges into blocks that only `jump` onward straight to the final
/// target, so a constant arg reaches the block that tests it. Each forwarded
/// arg must be a param of the bypassed block or defined where it dominates
/// the predecessor's branch. Returns the edges retargeted.
fn bypass_forwarders(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let entry = func.layout.entry_block();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    // Block params used outside their own block: bypassing that block would
    // leave those uses undominated.
    let mut escaping: FxHashSet<Value> = FxHashSet::default();
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for &x in &blocks {
        for i in func.layout.block_insts(x) {
            let mut note = |v: Value| {
                let v = func.dfg.resolve_aliases(v);
                *uses.entry(v).or_default() += 1;
                if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                    && d != x
                {
                    escaping.insert(v);
                }
            };
            func.dfg.inst_args(i).iter().for_each(|&v| note(v));
            for bc in func.dfg.insts[i]
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
    let mut n = 0;
    for b in blocks {
        if Some(b) == entry
            || func
                .dfg
                .block_params(b)
                .iter()
                .any(|p| escaping.contains(p))
        {
            continue;
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        // Any other insts must be pure and used only by each other.
        let body: Vec<Inst> = func.layout.block_insts(b).filter(|&i| i != term).collect();
        let mut local: FxHashMap<Value, u32> = FxHashMap::default();
        for &i in &body {
            for &a in func.dfg.inst_args(i) {
                *local.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
        }
        if !body.iter().all(|&i| {
            pure_op(func, i) && {
                let r = func.dfg.first_result(i);
                uses.get(&r).copied().unwrap_or(0) == local.get(&r).copied().unwrap_or(0)
            }
        }) {
            continue;
        }
        let InstructionData::Jump { destination, .. } = func.dfg.insts[term] else {
            continue;
        };
        let t = destination.block(&func.dfg.value_lists);
        if t == b {
            continue;
        }
        let mut targs = Vec::new();
        for a in destination.args(&func.dfg.value_lists) {
            match a {
                BlockArg::Value(v) => targs.push(func.dfg.resolve_aliases(v)),
                _ => targs.clear(),
            }
        }
        if targs.len() != func.dfg.num_block_params(t) {
            continue;
        }
        let params = func.dfg.block_params(b).to_vec();
        let preds: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        for (pb, pinst) in preds {
            if pb == b
                || !matches!(
                    func.dfg.insts[pinst].opcode(),
                    Opcode::Jump | Opcode::Brif | Opcode::BrTable
                )
            {
                continue;
            }
            // Index dests in place — `to_vec()` materializes every arm
            // of a br_table (16k entries per pred on giant dispatch fns)
            // though only the ones targeting `b` matter.
            let ndest = func.dfg.insts[pinst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .len();
            for di in 0..ndest {
                let bc = &func.dfg.insts[pinst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[di];
                if bc.block(&func.dfg.value_lists) != b {
                    continue;
                }
                let pargs: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if pargs.len() != params.len() {
                    continue;
                }
                let mut new = Vec::with_capacity(targs.len());
                for &v in &targs {
                    if let Some(k) = params.iter().position(|&p| p == v) {
                        new.push(pargs[k]);
                        continue;
                    }
                    let ok = match func.dfg.value_def(v) {
                        ValueDef::Result(i, _) => {
                            i != pinst && domtree.dominates(i, pinst, &func.layout)
                        }
                        ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
                        _ => false,
                    };
                    if !ok {
                        break;
                    }
                    new.push(BlockArg::Value(v));
                }
                if new.len() != targs.len() {
                    continue;
                }
                let nbc = BlockCall::new(t, new, &mut func.dfg.value_lists);
                let dfg = &mut func.dfg;
                dfg.insts[pinst]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di] =
                    nbc;
                n += 1;
            }
        }
    }
    n
}

/// The icmp a branch condition tests, through `band 1` and extends of its 0/1 result.
pub(crate) fn cond_icmp(func: &Function, v: Value) -> Option<Inst> {
    let env = FxHashMap::default();
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..6 {
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                ..
            } => return Some(i),
            InstructionData::Unary {
                opcode: Opcode::Uextend | Opcode::Ireduce,
                arg,
            } => v = func.dfg.resolve_aliases(arg),
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => {
                let (a, b) = (
                    func.dfg.resolve_aliases(args[0]),
                    func.dfg.resolve_aliases(args[1]),
                );
                if known(func, &env, b) == Some(1) {
                    v = a;
                } else if known(func, &env, a) == Some(1) {
                    v = b;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

pub(crate) type Cmp = (IntCC, Value, Value);

/// A scalar icmp as `(cc, x, y)`; `(x - y) ==/!= 0` becomes `x ==/!= y`.
pub(crate) fn norm_icmp(func: &Function, i: Inst) -> Option<Cmp> {
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let (mut a, mut b) = (
        func.dfg.resolve_aliases(args[0]),
        func.dfg.resolve_aliases(args[1]),
    );
    if func.dfg.value_type(a).is_vector() {
        return None;
    }
    if matches!(cond, IntCC::Equal | IntCC::NotEqual) {
        let env = FxHashMap::default();
        if known(func, &env, a) == Some(0) {
            std::mem::swap(&mut a, &mut b);
        }
        if known(func, &env, b) == Some(0)
            && let Some(d) = func.dfg.value_def(a).inst()
            && let InstructionData::Binary {
                opcode: Opcode::Isub,
                args: s,
            } = func.dfg.insts[d]
        {
            return Some((
                cond,
                func.dfg.resolve_aliases(s[0]),
                func.dfg.resolve_aliases(s[1]),
            ));
        }
    }
    Some((cond, a, b))
}

/// What a true fact `f` says about query `q`, if anything.
fn implied(f: Cmp, q: Cmp) -> Option<bool> {
    use IntCC::*;
    let (fc, fx, fy) = f;
    let qc = if (q.1, q.2) == (fx, fy) {
        q.0
    } else if (q.1, q.2) == (fy, fx) {
        q.0.swap_args()
    } else {
        return None;
    };
    if qc == fc {
        return Some(true);
    }
    if qc == fc.complement() {
        return Some(false);
    }
    match fc {
        Equal => Some(matches!(
            qc,
            UnsignedGreaterThanOrEqual
                | UnsignedLessThanOrEqual
                | SignedGreaterThanOrEqual
                | SignedLessThanOrEqual
        )),
        UnsignedLessThan | UnsignedGreaterThan | SignedLessThan | SignedGreaterThan => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Decide `a cc b` when one side is the constant `k` and the other lies in
/// the unsigned range `[lo, hi]` (`w`-bit compare).
fn range_cmp(
    cc: IntCC,
    a: Option<u64>,
    b: Option<u64>,
    ra: Option<(u64, u64)>,
    rb: Option<(u64, u64)>,
    w: u32,
) -> Option<bool> {
    use IntCC::*;
    let (cc, (lo, hi), k) = match (a, b) {
        (None, Some(k)) => (cc, ra?, k),
        (Some(k), None) => (cc.swap_args(), rb?, k),
        _ => return None,
    };
    let half = 1u64 << (w - 1);
    let cc = match cc {
        _ if hi >= half || k >= half => cc,
        SignedLessThan => UnsignedLessThan,
        SignedLessThanOrEqual => UnsignedLessThanOrEqual,
        SignedGreaterThan => UnsignedGreaterThan,
        SignedGreaterThanOrEqual => UnsignedGreaterThanOrEqual,
        c => c,
    };
    let dec = |t: bool, f: bool| {
        if t {
            Some(true)
        } else if f {
            Some(false)
        } else {
            None
        }
    };
    match cc {
        UnsignedLessThan => dec(hi < k, lo >= k),
        UnsignedLessThanOrEqual => dec(hi <= k, lo > k),
        UnsignedGreaterThan => dec(lo > k, hi <= k),
        UnsignedGreaterThanOrEqual => dec(lo >= k, hi < k),
        Equal => dec(lo == k && hi == k, k < lo || k > hi),
        NotEqual => dec(k < lo || k > hi, lo == k && hi == k),
        _ => None,
    }
}

/// Unsigned range of `v` from its width, `uextend`/`band` with a constant,
/// and the facts `fs` known where it is used.
fn full_range(func: &Function, v: Value) -> Option<(u64, u64)> {
    if func.dfg.value_type(v).is_vector() {
        return None;
    }
    let w = bits(func, v)?;
    Some((0, mask(u64::MAX, w)))
}

fn urange_or_full(
    func: &Function,
    fs: &[Cmp],
    v: Value,
    depth: u32,
    range2: bool,
) -> Option<(u64, u64)> {
    urange(func, fs, v, depth, range2).or_else(|| range2.then(|| full_range(func, v)).flatten())
}

fn urange(func: &Function, fs: &[Cmp], v: Value, depth: u32, range2: bool) -> Option<(u64, u64)> {
    let v = func.dfg.resolve_aliases(v);
    let w = bits(func, v)?;
    if func.dfg.value_type(v).is_vector() {
        return None;
    }
    let full = mask(u64::MAX, w);
    let (mut lo, mut hi) = (0u64, full);
    let env = FxHashMap::default();
    for &(cc, a, b) in fs {
        let (cc, k) = if a == v {
            (cc, known(func, &env, b))
        } else if b == v {
            (cc.swap_args(), known(func, &env, a))
        } else {
            continue;
        };
        let Some(k) = k else { continue };
        match cc {
            IntCC::UnsignedLessThan if k > 0 => hi = hi.min(k - 1),
            IntCC::UnsignedLessThanOrEqual => hi = hi.min(k),
            IntCC::UnsignedGreaterThan if k < full => lo = lo.max(k + 1),
            IntCC::UnsignedGreaterThanOrEqual => lo = lo.max(k),
            IntCC::Equal => {
                lo = lo.max(k);
                hi = hi.min(k);
            }
            _ => {}
        }
    }
    if depth < if range2 { 8 } else { 4 }
        && let Some(i) = func.dfg.value_def(v).inst()
    {
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } => {
                if let Some((l, h)) = urange_or_full(func, fs, arg, depth + 1, range2) {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Binary {
                opcode: opcode @ (Opcode::Iadd | Opcode::Isub),
                args,
            } => {
                if range2
                    && opcode == Opcode::Isub
                    && let Some(k) = known(func, &env, args[0])
                    && let Some((l, h)) = urange_or_full(func, fs, args[1], depth + 1, range2)
                    && h <= k
                {
                    lo = lo.max(k - h);
                    hi = hi.min(k - l);
                }
                if let Some(k) = known(func, &env, args[1])
                    && let Some((l, h)) = urange_or_full(func, fs, args[0], depth + 1, range2)
                {
                    let (l2, h2) = if opcode == Opcode::Isub {
                        (l.wrapping_sub(k), h.wrapping_sub(k))
                    } else {
                        (l.wrapping_add(k), h.wrapping_add(k))
                    };
                    // Only when the whole range moves without wrapping.
                    let nowrap = if opcode == Opcode::Isub {
                        l >= k
                    } else {
                        h <= full - k
                    };
                    if nowrap {
                        lo = lo.max(l2);
                        hi = hi.min(h2);
                    }
                }
            }
            InstructionData::Unary {
                opcode: opcode @ (Opcode::Clz | Opcode::Ctz | Opcode::Popcnt),
                arg,
            } if range2 => {
                if let Some((l, h)) = urange_or_full(func, fs, arg, depth + 1, range2) {
                    let w = bits(func, arg)?;
                    let r = match opcode {
                        Opcode::Clz => {
                            let clzw = |x: u64| {
                                if x == 0 {
                                    w
                                } else {
                                    x.leading_zeros() - (64 - w)
                                }
                            };
                            (u64::from(clzw(h)), u64::from(clzw(l)))
                        }
                        Opcode::Ctz => (0, u64::from(if l > 0 { w - 1 } else { w })),
                        Opcode::Popcnt => (u64::from(l > 0), u64::from(w)),
                        _ => unreachable!(),
                    };
                    lo = lo.max(r.0);
                    hi = hi.min(r.1);
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Ushr,
                args,
            } if range2 => {
                if let Some(s) = known(func, &env, args[1])
                    && s < u64::from(w)
                    && let Some((l, h)) = urange_or_full(func, fs, args[0], depth + 1, range2)
                {
                    lo = lo.max(l >> s);
                    hi = hi.min(h >> s);
                }
            }
            InstructionData::BinaryImm8 {
                opcode: Opcode::Ushr,
                arg,
                imm,
            } if range2 => {
                let s = u64::from(imm);
                if s < u64::from(w)
                    && let Some((l, h)) = urange_or_full(func, fs, arg, depth + 1, range2)
                {
                    lo = lo.max(l >> s);
                    hi = hi.min(h >> s);
                }
            }
            InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg,
            } => {
                if let Some((l, h)) = urange(func, fs, arg, depth + 1, range2)
                    && h <= full
                {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => {
                if let Some(m) = known(func, &env, args[1]).or(known(func, &env, args[0])) {
                    hi = hi.min(m);
                }
            }
            _ => {}
        }
    }
    (lo <= hi && (lo, hi) != (0, full)).then_some((lo, hi))
}

/// The condition known on entry to each block whose only predecessor ends
/// in `brif c` (`c` or its complement).
pub(crate) fn edge_facts(func: &Function, cfg: &ControlFlowGraph) -> FxHashMap<Block, Cmp> {
    let mut fact: FxHashMap<Block, Cmp> = FxHashMap::default();
    for b in func.layout.blocks() {
        let mut preds = cfg.pred_iter(b);
        let (Some(p), None) = (preds.next(), preds.next()) else {
            continue;
        };
        if p.block == b {
            continue;
        }
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] else {
            continue;
        };
        let t = blocks[0].block(&func.dfg.value_lists);
        if t == blocks[1].block(&func.dfg.value_lists) {
            continue;
        }
        let Some((c, x, y)) = cond_icmp(func, arg).and_then(|i| norm_icmp(func, i)) else {
            continue;
        };
        fact.insert(b, (if b == t { c } else { c.complement() }, x, y));
    }
    fact
}

/// Index constants known on entry to blocks whose only predecessor is a
/// `br_table` listing them once (not as default): `idx == k`, and also
/// `x == k` when `idx = ireduce x` and `x` is a `uextend` no wider than `idx`.
fn table_facts(func: &Function, cfg: &ControlFlowGraph) -> FxHashMap<Block, Vec<(Value, u64)>> {
    let mut m: FxHashMap<Block, Vec<(Value, u64)>> = FxHashMap::default();
    let pool = &func.dfg.value_lists;
    // Arms grouped by their dispatch inst — per-arm jump-table scans are
    // O(arms²), so each table is walked once and per-target hits counted.
    let mut by_inst: FxHashMap<Inst, Vec<(Block, cranelift_codegen::ir::JumpTable)>> =
        FxHashMap::default();
    for b in func.layout.blocks() {
        let mut preds = cfg.pred_iter(b);
        let (Some(p), None) = (preds.next(), preds.next()) else {
            continue;
        };
        if p.block == b {
            continue;
        }
        let InstructionData::BranchTable { table, .. } = func.dfg.insts[p.inst] else {
            continue;
        };
        let jt = &func.dfg.jump_tables[table];
        if jt.default_block().block(pool) == b {
            continue;
        }
        by_inst.entry(p.inst).or_default().push((b, table));
    }
    for (pinst, arms) in by_inst {
        let InstructionData::BranchTable { arg, .. } = func.dfg.insts[pinst] else {
            continue;
        };
        let arg = func.dfg.resolve_aliases(arg);
        // One pass over the shared table: target -> (count, first index).
        let mut cnt: FxHashMap<Block, (u32, u64)> = FxHashMap::default();
        for (k, c) in func.dfg.jump_tables[arms[0].1]
            .as_slice()
            .iter()
            .enumerate()
        {
            let e = cnt.entry(c.block(pool)).or_default();
            e.0 += 1;
            e.1 = k as u64;
        }
        for (b, _) in arms {
            let Some(&(1, k)) = cnt.get(&b) else {
                continue;
            };
            let mut v = vec![(arg, k)];
            if let Some(i) = func.dfg.value_def(arg).inst()
                && let InstructionData::Unary {
                    opcode: Opcode::Ireduce,
                    arg: x,
                } = func.dfg.insts[i]
            {
                let x = func.dfg.resolve_aliases(x);
                if let Some(j) = func.dfg.value_def(x).inst()
                    && let InstructionData::Unary {
                        opcode: Opcode::Uextend,
                        arg: n,
                    } = func.dfg.insts[j]
                    && bits(func, n)
                        .zip(bits(func, arg))
                        .is_some_and(|(n, a)| n <= a)
                {
                    v.push((x, k));
                }
            }
            m.insert(b, v);
        }
    }
    m
}

/// Facts holding throughout `b`: those of `b` and its dominators.
pub(crate) fn facts_at(
    domtree: &DominatorTree,
    fact: &FxHashMap<Block, Cmp>,
    b: Block,
) -> Vec<Cmp> {
    let mut fs = Vec::new();
    if fact.is_empty() {
        return fs;
    }
    let mut cur = Some(b);
    for _ in 0..64 {
        let Some(c) = cur else { break };
        if let Some(&f) = fact.get(&c) {
            fs.push(f);
        }
        cur = domtree.idom(c);
    }
    fs
}

/// Fold icmps decided by a dominating branch: a block whose only predecessor
/// ends in `brif c` knows `c` (or its complement), as do the blocks it
/// dominates; equal operands or a constant against a known range decide it.
/// Returns the icmps folded.
pub fn fold_dominated_conds(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    fold_dominated_conds_at(func, &cfg, &domtree)
}

fn fold_dominated_conds_at(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    domtree: &DominatorTree,
) -> usize {
    let folds = dominated_conds(func, cfg, domtree);
    for &(i, k) in &folds {
        let ty = func.dfg.value_type(func.dfg.first_result(i));
        func.replace(i).iconst(ty, i64::from(k));
    }
    folds.len()
}

/// The values of `fold_dominated_conds` and `fold_const_branches` without rewriting `func`.
pub fn known_values(func: &Function) -> FxHashMap<Value, u64> {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let mut env: FxHashMap<Value, u64> = dominated_conds(func, &cfg, &domtree)
        .into_iter()
        .map(|(i, k)| (func.dfg.first_result(i), u64::from(k)))
        .collect();
    for _ in 0..2 {
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if func.dfg.inst_results(i).len() == 1
                    && !env.contains_key(&func.dfg.first_result(i))
                    && let Some(c) = eval(func, &env, &|_| false, &|_| None, i)
                {
                    env.insert(func.dfg.first_result(i), c);
                }
            }
        }
    }
    env
}

pub fn known_value(func: &Function, env: &FxHashMap<Value, u64>, v: Value) -> Option<u64> {
    known(func, env, v)
}

fn dominated_conds(
    func: &Function,
    cfg: &ControlFlowGraph,
    domtree: &DominatorTree,
) -> Vec<(Inst, bool)> {
    let fact = edge_facts(func, &cfg);
    let tfact = table_facts(func, &cfg);
    let range2 = crate::pass_enabled("PLIRON_RANGE2");
    if !range2 && fact.is_empty() && tfact.is_empty() {
        return Vec::new();
    }
    // One domtree DFS carries the inherited facts into each block
    // instead of two ≤64-hop idom walks per block: `fs` is the chain
    // nearest-first (stack top = the block itself), and `env` keeps the
    // original furthest-ancestor-wins precedence — entries are only
    // filled when an ancestor didn't already set them, and an undo log
    // restores them on exit.
    let mut children: FxHashMap<Block, Vec<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        if let Some(p) = domtree.idom(b) {
            children.entry(p).or_default().push(b);
        }
    }
    let mut fs: Vec<Cmp> = Vec::new();
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    let mut undo: Vec<Value> = Vec::new();
    let mut folds = Vec::new();
    // (block, fs_mark, undo_mark, exiting) — enter pushes the block's
    // own facts after recording marks, exit truncates back to them.
    let mut stack: Vec<(Block, usize, usize, bool)> = func
        .layout
        .blocks()
        .filter(|&b| domtree.idom(b).is_none())
        .map(|b| (b, 0, 0, false))
        .collect();
    while let Some((b, fmark, umark, exit)) = stack.pop() {
        if exit {
            for &v in undo[umark..].iter() {
                env.remove(&v);
            }
            undo.truncate(umark);
            fs.truncate(fmark);
            continue;
        }
        let (fmark, umark) = (fs.len(), undo.len());
        if let Some(&f) = fact.get(&b) {
            fs.push(f);
        }
        for &(v, k) in tfact.get(&b).into_iter().flatten() {
            if !env.contains_key(&v) {
                env.insert(v, k);
                undo.push(v);
            }
        }
        stack.push((b, fmark, umark, true));
        for &c in children.get(&b).into_iter().flatten() {
            stack.push((c, 0, 0, false));
        }
        if !range2 && fs.is_empty() && env.is_empty() {
            continue;
        }
        // Nearest-ancestor-first, as `facts_at` produced.
        let fsv: Vec<Cmp> = fs.iter().rev().copied().collect();
        let fs = fsv.as_slice();
        // `x == y` facts let a query on `y` be decided through `x`.
        let eqs: Vec<(Value, Value)> = fs
            .iter()
            .filter(|f| f.0 == IntCC::Equal)
            .map(|f| (f.1, f.2))
            .collect();
        let alts = |v: Value| {
            let mut a = vec![v];
            for &(x, y) in &eqs {
                if x == v {
                    a.push(y);
                } else if y == v {
                    a.push(x);
                }
            }
            a
        };
        let decide = |q: Cmp| -> Option<bool> {
            if let Some(r) = fs.iter().find_map(|&f| implied(f, q)) {
                return Some(r);
            }
            let w = bits(func, q.1)?;
            let (ka, kb) = (known(func, &env, q.1), known(func, &env, q.2));
            if let (Some(a), Some(b)) = (ka, kb) {
                return Some(icmp(q.0, a, b, w));
            }
            range_cmp(
                q.0,
                ka,
                kb,
                urange(func, fs, q.1, 0, range2),
                urange(func, fs, q.2, 0, range2),
                w,
            )
        };
        for i in func.layout.block_insts(b) {
            let Some(q) = norm_icmp(func, i) else {
                continue;
            };
            let r = alts(q.1)
                .into_iter()
                .flat_map(|a| alts(q.2).into_iter().map(move |b| (q.0, a, b)))
                .find_map(decide);
            if let Some(k) = r {
                folds.push((i, k));
            }
        }
    }
    folds
}

/// Block params whose every incoming `jump`/`brif` arg is one value `v` (or
/// the param itself) become aliases of `v`. Threading's SSA repair creates
/// these, and Cranelift only removes them after our condition folding ran, so
/// the same value seen through two params would not match a dominating test.
pub fn remove_trivial_params(func: &mut Function, dt: &DominatorTree) -> Vec<(Value, Value)> {
    #[derive(Clone, Copy, PartialEq)]
    enum In {
        Unset,
        One(Value),
        Many,
    }
    let entry = func.layout.entry_block();
    let mut inc: FxHashMap<Block, Vec<In>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let fixed = matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif);
        for bc in
            func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            let s = bc.block(&func.dfg.value_lists);
            let params = func.dfg.block_params(s);
            let st = inc
                .entry(s)
                .or_insert_with(|| vec![In::Unset; params.len()]);
            if !fixed {
                st.iter_mut().for_each(|x| *x = In::Many);
                continue;
            }
            for (i, a) in bc.args(&func.dfg.value_lists).enumerate().take(st.len()) {
                let new = match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        if v == params[i] {
                            continue;
                        }
                        In::One(v)
                    }
                    _ => In::Many,
                };
                st[i] = match (st[i], new) {
                    (In::Unset, x) => x,
                    (In::One(a), In::One(b)) if a == b => In::One(a),
                    _ => In::Many,
                };
            }
        }
    }
    let def_block = |func: &Function, v: Value| match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    };
    // `v` must dominate `b`: an edge passing the param itself (e.g. a loop
    // latch) does not constrain `v`'s position, so without this check the
    // alias can put `v` in scopes its def doesn't dominate.
    let mut rm: Vec<(Block, usize, Value, Value)> = Vec::new();
    for (&b, st) in &inc {
        if Some(b) == entry {
            continue;
        }
        let ps = func.dfg.block_params(b);
        for (i, s) in st.iter().enumerate() {
            if let In::One(v) = *s
                && let Some(d) = def_block(func, v)
                && d != b
                && dt.dominates(d, b, &func.layout)
            {
                rm.push((b, i, ps[i], v));
            }
        }
    }
    // No alias chains between params removed together (avoids alias loops).
    let gone: FxHashSet<Value> = rm.iter().map(|r| r.2).collect();
    rm.retain(|r| !gone.contains(&r.3));
    if rm.is_empty() {
        return Vec::new();
    }
    let mut idx: FxHashMap<Block, Vec<usize>> = FxHashMap::default();
    for r in &rm {
        idx.entry(r.0).or_default().push(r.1);
    }
    for v in idx.values_mut() {
        v.sort_unstable_by(|a, b| b.cmp(a));
    }
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        if !matches!(func.dfg.insts[t].opcode(), Opcode::Jump | Opcode::Brif) {
            continue;
        }
        let dfg = &mut func.dfg;
        for bc in
            dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if let Some(is) = idx.get(&bc.block(&dfg.value_lists)) {
                for &i in is {
                    bc.remove(i, &mut dfg.value_lists);
                }
            }
        }
    }
    let aliases: Vec<(Value, Value)> = rm.iter().map(|&(_, _, p, v)| (p, v)).collect();
    for &(_, _, p, v) in &rm {
        func.dfg.remove_block_param(p);
        func.dfg.change_to_alias(p, v);
    }
    aliases
}

fn mark_live_param(
    func: &Function,
    value: Value,
    live: &mut FxHashMap<Block, Vec<bool>>,
    work: &mut Vec<(Block, usize)>,
) {
    let value = func.dfg.resolve_aliases(value);
    let ValueDef::Param(block, index) = func.dfg.value_def(value) else {
        return;
    };
    let Some(is_live) = live
        .get_mut(&block)
        .and_then(|params| params.get_mut(index))
    else {
        return;
    };
    if !*is_live {
        *is_live = true;
        work.push((block, index));
    }
}

pub fn remove_dead_params(func: &mut Function) -> usize {
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut live: FxHashMap<Block, Vec<bool>> = blocks
        .iter()
        .map(|&block| (block, vec![false; func.dfg.num_block_params(block)]))
        .collect();
    let mut protected: FxHashSet<Block> = func.layout.entry_block().into_iter().collect();
    let mut preds: FxHashMap<Block, Vec<Vec<BlockArg>>> = FxHashMap::default();
    let mut insts = Vec::new();

    for &block in &blocks {
        for inst in func.layout.block_insts(block) {
            insts.push(inst);
            let data = &func.dfg.insts[inst];
            for call in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                let target = call.block(&func.dfg.value_lists);
                let args: Vec<BlockArg> = call.args(&func.dfg.value_lists).collect();
                if args.iter().any(|arg| !matches!(arg, BlockArg::Value(_))) {
                    protected.insert(target);
                }
                preds.entry(target).or_default().push(args);
            }
            if let Some(table) = data.exception_table() {
                for target in func.dfg.exception_tables[table].all_branches() {
                    protected.insert(target.block(&func.dfg.value_lists));
                }
            }
        }
    }

    let mut work = Vec::new();
    for &block in &blocks {
        if protected.contains(&block) {
            for (index, is_live) in live.get_mut(&block).unwrap().iter_mut().enumerate() {
                if !*is_live {
                    *is_live = true;
                    work.push((block, index));
                }
            }
        }
    }
    for inst in &insts {
        for &value in func.dfg.inst_args(*inst) {
            mark_live_param(func, value, &mut live, &mut work);
        }
        if let Some(table) = func.dfg.insts[*inst].exception_table() {
            for item in func.dfg.exception_tables[table].items() {
                if let ExceptionTableItem::Context(value) = item {
                    mark_live_param(func, value, &mut live, &mut work);
                }
            }
        }
    }

    while let Some((target, index)) = work.pop() {
        let Some(incoming) = preds.get(&target) else {
            continue;
        };
        for args in incoming {
            if let Some(BlockArg::Value(value)) = args.get(index).copied() {
                mark_live_param(func, value, &mut live, &mut work);
            }
        }
    }

    let mut dead: FxHashMap<Block, Vec<usize>> = FxHashMap::default();
    for &block in &blocks {
        if protected.contains(&block) {
            continue;
        }
        for (index, &is_live) in live.get(&block).unwrap().iter().enumerate() {
            if !is_live {
                dead.entry(block).or_default().push(index);
            }
        }
    }
    if dead.is_empty() {
        return 0;
    }
    for indices in dead.values_mut() {
        indices.sort_unstable_by(|a, b| b.cmp(a));
    }

    let terms: Vec<Inst> = blocks
        .iter()
        .filter_map(|&block| func.layout.last_inst(block))
        .collect();
    for term in terms {
        let dfg = &mut func.dfg;
        for call in
            dfg.insts[term].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if let Some(indices) = dead.get(&call.block(&dfg.value_lists)) {
                for &index in indices {
                    call.remove(index, &mut dfg.value_lists);
                }
            }
        }
    }

    let removed = dead.values().map(Vec::len).sum();
    for (&block, indices) in &dead {
        for &index in indices {
            let param = func.dfg.block_params(block)[index];
            func.dfg.remove_block_param(param);
        }
    }
    removed
}

fn remove_unused_notrap_loads(func: &mut Function) {
    remove_dead_insts(func, false);
}

/// Drop blocks not reachable from the entry, unless a reachable instruction
/// uses a value they define (then nothing is removed). Returns blocks removed.
pub fn remove_unreachable_blocks(func: &mut Function) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut work = vec![entry];
    while let Some(b) = work.pop() {
        if !seen.insert(b) {
            continue;
        }
        if let Some(term) = func.layout.last_inst(b) {
            let data = &func.dfg.insts[term];
            for call in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                work.push(call.block(&func.dfg.value_lists));
            }
        }
    }
    let dead: Vec<Block> = func.layout.blocks().filter(|b| !seen.contains(b)).collect();
    if dead.is_empty() {
        return 0;
    }
    let defined_in_dead = |func: &Function, v: Value| match func.dfg.resolved_value_def(v) {
        ValueDef::Result(i, _) => func
            .layout
            .inst_block(i)
            .is_some_and(|b| !seen.contains(&b)),
        ValueDef::Param(b, _) => !seen.contains(&b),
        _ => false,
    };
    for &b in &seen {
        for i in func.layout.block_insts(b) {
            if func.dfg.inst_values(i).any(|v| defined_in_dead(func, v)) {
                return 0;
            }
        }
    }
    for &b in &dead {
        while let Some(i) = func.layout.last_inst(b) {
            func.layout.remove_inst(i);
        }
        func.layout.remove_block(b);
    }
    dead.len()
}

/// `brif c, B(args), B(args)` -> `jump B(args)`. Returns branches folded.
pub fn fold_same_target_brifs(func: &mut Function) -> usize {
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { blocks: [x, y], .. } = func.dfg.insts[t] else {
            continue;
        };
        let pool = &func.dfg.value_lists;
        if x.block(pool) == y.block(pool) && x.args(pool).eq(y.args(pool)) {
            func.dfg.insts[t] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination: x,
            };
            n += 1;
        }
    }
    n
}

fn nonzero_iconst(func: &Function, v: Value) -> bool {
    let Some(i) = func.dfg.resolved_value_def(v).inst() else {
        return false;
    };
    matches!(
        func.dfg.insts[i],
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if imm.bits() != 0
    )
}

/// Can `inst` be deleted when none of its results are used? Always true for
/// `notrap` loads; with `pure`, also for side-effect-free, non-trapping ops
/// (plus `udiv`/`urem` by a nonzero constant).
pub fn removable(func: &Function, inst: Inst, pure: bool) -> bool {
    let data = &func.dfg.insts[inst];
    if let InstructionData::Load { flags, .. } = *data {
        return func.dfg.mem_flags[flags].notrap();
    }
    let op = data.opcode();
    pure && !(op.is_call()
        || op.is_branch()
        || op.is_terminator()
        || op.can_load()
        || op.can_store()
        || op.other_side_effects())
        && (!op.can_trap()
            || matches!(
                *data,
                InstructionData::Binary { opcode: Opcode::Udiv | Opcode::Urem, args }
                    if nonzero_iconst(func, args[1])
            ))
}

/// Delete instructions whose results are all unused (see `removable`),
/// propagating kills through operand use-counts until nothing dies.
/// Returns the number deleted.
pub fn remove_dead_insts(func: &mut Function, pure: bool) -> usize {
    crate::uses::worklist_dce(func, |f, i| {
        !f.dfg.inst_results(i).is_empty() && removable(f, i, pure)
    })
}

/// Thread until nothing changes (bounded); returns the edges retargeted.
pub fn run(
    func: &mut Function,
    loads: &FxHashSet<Value>,
    derived: &mut FxHashMap<Value, Value>,
) -> (usize, usize, usize, FxHashMap<Value, Value>) {
    let prof = pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::jumpthread", tracing::Level::INFO);
    let mut tp = std::time::Instant::now();
    merge_chains(func);
    if prof {
        if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  merge_chains {:?}", tp.elapsed()) };
        tp = std::time::Instant::now();
    }
    let domcond = crate::pass_enabled("PLIRON_DOMCOND");
    let trivp = crate::pass_enabled("PLIRON_TRIVPARAM");
    let deadp = crate::pass_enabled("PLIRON_DEADPARAM");
    let ifconv = crate::pass_enabled("PLIRON_IFCONV");
    let mut total = 0;
    let mut dead_params = 0;
    let mut dominated_folds = 0;
    let chk = crate::env_set("PLIRON_JT_CHECK");
    let check = |func: &Function, tag: &str| {
        if chk
            && let Err(e) = cranelift_codegen::verify_function(
                func,
                &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
            )
        {
            panic!("jt step {tag}: {e}\n{}", func.display());
        }
    };
    // `change_to_alias` merges reported so the pipeline can carry the
    // dropped params' facts (nonnull/deref/…) onto their carriers.
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    // One CFG + domtree per "graph epoch": param/alias/value rewrites
    // (trivp, deadp, domcond, inst sweeps) leave block adjacency intact,
    // while ifconv/bypass/round retarget edges and force a rebuild.
    let mut cd: Option<(ControlFlowGraph, DominatorTree)> = None;
    let cfg_dt = |func: &Function| {
        let c = ControlFlowGraph::with_function(func);
        let d = DominatorTree::with_function(func, &c);
        (c, d)
    };
    // `round` builds NonNull lazily on the first real candidate edge — a
    // quiet round never pays for the fixpoint. `derived` mutates across
    // iterations (ifconv), so the Option is per-iteration; when the loop
    // converges (`n == 0`) nothing after its construction rewrote the
    // function, so `fold_null_tests` runs on it inside the loop and the
    // usize result crosses the boundary while `nn`'s borrow may not.
    let mut fnn_cur: Option<usize> = None;
    // Set after a round that changed nothing: if the next iteration's
    // prefix phases (which are the only `func`/`derived` mutators —
    // ifconv writes `derived` only inside real conversions) also change
    // nothing, `round` would rescan an identical function and come back
    // empty. Skip the rescan; `fold_null_tests` still gets a fresh nn.
    let mut round_quiet = false;
    let mut round_skips = 0usize;
    for _ in 0..8 {
        let mut tp = std::time::Instant::now();
        let mut changed = false;
        if trivp {
            let v = {
                let (_, d) = cd.get_or_insert_with(|| cfg_dt(func));
                remove_trivial_params(func, d)
            };
            changed = !v.is_empty();
            pmap.extend(v);
            check(func, "trivp");
        }
        if prof {
            if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  trivp {:?}", tp.elapsed()) };
            tp = std::time::Instant::now();
        }
        if deadp {
            let removed = remove_dead_params(func);
            dead_params += removed;
            if removed != 0 {
                remove_unused_notrap_loads(func);
                changed = true;
            }
            check(func, "deadp");
        }
        if prof {
            if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  deadp {:?}", tp.elapsed()) };
            tp = std::time::Instant::now();
        }
        let dc = if domcond {
            let (c, d) = cd.get_or_insert_with(|| cfg_dt(func));
            fold_dominated_conds_at(func, c, d)
        } else {
            0
        };
        changed |= dc > 0;
        dominated_folds += dc;
        check(func, "domcond");
        if prof {
            if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  domcond {:?}", tp.elapsed()) };
            tp = std::time::Instant::now();
        }
        // Select-diamond if-conversion feeds back: folded forwarders expose
        // new diamonds and bypass_forwarders collapses the residue. The dead
        // side blocks keep phantom CFG edges (skewing dominance checks), so
        // sweep them once unreachable.
        let ic = if ifconv {
            crate::ifconv::run(func, derived)
        } else {
            0
        };
        check(func, "ifconv");
        if ic > 0 {
            changed = true;
            cd = None;
            remove_unreachable_blocks(func);
        }
        check(func, "unreach");
        let bpn = bypass_forwarders(func);
        check(func, "bypass");
        if bpn > 0 {
            changed = true;
            // Edge retargets invalidate the cached graph before `round`
            // reads it.
            cd = None;
        }
        if prof {
            if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  ifconv+bypass {:?}", tp.elapsed()) };
            tp = std::time::Instant::now();
        }
        let mut nn_opt = None;
        let rn = if round_quiet && !changed {
            round_skips += 1;
            0
        } else {
            let (c, d) = cd.get_or_insert_with(|| cfg_dt(func));
            round(func, loads, derived, &mut nn_opt, domcond, c, d)
        };
        round_quiet = rn == 0;
        check(func, "round");
        if prof {
            if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  nn+round {:?}", tp.elapsed()) };
        }
        if rn > 0 {
            cd = None;
        }
        let n = dc + ic + bpn + rn;
        total += n;
        if n == 0 {
            fnn_cur = Some(fold_null_tests(
                func,
                &nn_opt.unwrap_or_else(|| NonNull::new(func, loads, derived)),
            ));
            break;
        }
    }
    if prof {
        if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  loop-total {:?} (skips {round_skips})", tp.elapsed()) };
        tp = std::time::Instant::now();
    }
    let fnn = match fnn_cur {
        Some(x) => x,
        None => fold_null_tests(func, &NonNull::new(func, loads, derived)),
    };
    if prof {
        if pliron_cfg::tracing_on() { tracing::info!(target: "pliron::jumpthread", "  fold_null_tests {:?}", tp.elapsed()) };
    }
    (total + fnn, dead_params, dominated_folds, pmap)
}

#[cfg(test)]
mod tests {
    use super::remove_dead_params;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{BlockArg, Function, InstBuilder, types};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    #[test]
    fn removes_cycle_only_param_and_keeps_exit_condition_param() {
        let mut func = Function::new();
        let mut builder_ctx = FunctionBuilderContext::new();
        let (loop_block, exit_block) = {
            let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let entry = builder.create_block();
            let loop_block = builder.create_block();
            let exit_block = builder.create_block();
            builder.append_block_param(loop_block, types::I64);
            builder.append_block_param(loop_block, types::I64);
            builder.append_block_param(exit_block, types::I64);

            builder.switch_to_block(entry);
            let zero = builder.ins().iconst(types::I64, 0);
            let entry_args = [BlockArg::Value(zero), BlockArg::Value(zero)];
            builder.ins().jump(loop_block, &entry_args);

            builder.switch_to_block(loop_block);
            let cycle = builder.block_params(loop_block)[0];
            let counter = builder.block_params(loop_block)[1];
            let next = builder.ins().iadd_imm_s(counter, 1);
            let condition = builder
                .ins()
                .icmp_imm_u(IntCC::UnsignedLessThan, counter, 4);
            let loop_args = [BlockArg::Value(cycle), BlockArg::Value(next)];
            let exit_args = [BlockArg::Value(counter)];
            builder
                .ins()
                .brif(condition, loop_block, &loop_args, exit_block, &exit_args);

            builder.switch_to_block(exit_block);
            let result = builder.block_params(exit_block)[0];
            builder.ins().return_(&[result]);
            builder.seal_all_blocks();
            (loop_block, exit_block)
        };

        assert_eq!(remove_dead_params(&mut func), 1);
        assert_eq!(func.dfg.num_block_params(loop_block), 1);
        assert_eq!(func.dfg.num_block_params(exit_block), 1);
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                for call in func.dfg.insts[inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if call.block(&func.dfg.value_lists) == loop_block {
                        assert_eq!(call.len(&func.dfg.value_lists), 1);
                    }
                }
            }
        }
    }
}

/// Turns `brif`/`br_table` on a compile-time constant into a `jump`. Later passes (load
/// forwarding) expose constants after threading ran, and Cranelift never folds branches,
/// so the dead successor's block arguments would otherwise stay live.
pub fn fold_const_branches(func: &mut Function) -> usize {
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for _ in 0..2 {
        for &b in &blocks {
            for i in func.layout.block_insts(b) {
                if func.dfg.inst_results(i).len() == 1
                    && let Some(c) = eval(func, &env, &|_| false, &|_| None, i)
                {
                    env.insert(func.dfg.first_result(i), c);
                }
            }
        }
    }
    let mut n = 0;
    for &b in &blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let dest = match func.dfg.insts[t] {
            InstructionData::Brif { arg, blocks, .. } => {
                let arg = func.dfg.resolve_aliases(arg);
                known(func, &env, arg).map(|c| blocks[usize::from(c == 0)])
            }
            InstructionData::BranchTable { arg, table, .. } => {
                let arg = func.dfg.resolve_aliases(arg);
                known(func, &env, arg).map(|c| {
                    let jt = &func.dfg.jump_tables[table];
                    usize::try_from(c)
                        .ok()
                        .and_then(|c| jt.as_slice().get(c).copied())
                        .unwrap_or(jt.default_block())
                })
            }
            _ => None,
        };
        if let Some(d) = dest {
            func.dfg.insts[t] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination: d,
            };
            n += 1;
        }
    }
    n
}

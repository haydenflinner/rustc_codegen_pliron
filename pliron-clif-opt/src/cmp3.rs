//! Three-way-compare / `Ordering` folds across block params
//! (`PLIRON_CMP3=1`, opt-in; `PLIRON_CMP3=0` is the kill switch).
//!
//! rustc's `Ord::cmp` lowers to `select lt, -1, (select ne, 1, 0)` (the
//! `BinOp::Cmp` fallback in `rustc_codegen_ssa`), and a derived
//! lexicographic `cmp`/`partial_cmp` merges one such value per field into
//! a join block param. The caller's test (`== Less`, `is_lt()`,
//! `partial_cmp(..).is_some_and(Ordering::is_lt)` = `select (p == -2), 0,
//! (p < 0)`) then runs on the param, so the egraph's spaceship rules and
//! clifpeep's `selord` mask algebra never see the select tree they would
//! collapse to one compare. Two moves:
//!
//! 1. **Small value sets.** Every integer value in the cone of an
//!    `icmp x, K` gets the set of values it can take (at most four, else
//!    top): constants, compare results `{0,1}`, `select` = union,
//!    extends/narrows mapped element-wise, binary ops by cross product,
//!    and block params as the union of their incoming args, each narrowed
//!    by the edge's own `brif (icmp a, K)` test. Optimistic worklist
//!    fixpoint (monotone joins on a finite lattice, so it terminates).
//!    A compare whose result is the same for every element folds to a
//!    constant: the `Option<Ordering>` `== -2` (None) test on a value
//!    known to be in `{-1, 0, 1}` disappears, and its `select` with it.
//! 2. **Push compares through the join.** When every use of a block param
//!    `p` is `icmp cc p, K` (directly or through one `uextend`/`sextend`),
//!    each compare becomes a new i8 param computed on every incoming edge:
//!    a constant when the arg's (edge-narrowed) set decides it, otherwise
//!    the same compare on the arg, which `selord` folds into a single
//!    `icmp` of the original operands right there (`(a <=> b) < 0` is
//!    `a <u b`). `p` and the per-field spaceships die. Joins are visited
//!    in CFG post-order, so the compares pushed into an inner join (nested
//!    derives, `Span` of `Position`s) get pushed again.
//!
//! Soundness: (1) replaces a compare by the constant every reachable
//! value gives it (an unreachable edge contributes nothing, which only
//! matters for code that never runs); (2) computes the same compare on
//! the same value, only earlier — on the edge instead of in the join.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    Block, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value, ValueDef,
    types,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// At most this many distinct values per set.
const NSET: usize = 4;
/// Cone size budget (values) for the set analysis.
const MAX_CONE: usize = 1 << 14;
/// Per-param limits for the push-through.
const MAX_QUERIES: usize = 4;
const MAX_EDGES: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Vs {
    /// No value yet (optimistic start).
    Bot,
    /// One of these (masked to the value's width).
    S(u8, [u64; NSET]),
    /// Anything.
    Top,
}

impl Vs {
    fn one(k: u64) -> Vs {
        Vs::S(1, [k, 0, 0, 0])
    }
    fn elems(&self) -> &[u64] {
        match self {
            Vs::S(n, a) => &a[..*n as usize],
            _ => &[],
        }
    }
    fn insert(self, k: u64) -> Vs {
        match self {
            Vs::Top => Vs::Top,
            Vs::Bot => Vs::one(k),
            Vs::S(n, mut a) => {
                if a[..n as usize].contains(&k) {
                    return self;
                }
                if n as usize == NSET {
                    return Vs::Top;
                }
                a[n as usize] = k;
                // Keep sorted so equal sets compare equal.
                a[..n as usize + 1].sort_unstable();
                Vs::S(n + 1, a)
            }
        }
    }
    fn join(self, o: Vs) -> Vs {
        match (self, o) {
            (Vs::Top, _) | (_, Vs::Top) => Vs::Top,
            (Vs::Bot, x) | (x, Vs::Bot) => x,
            (a, b) => b.elems().iter().fold(a, |s, &k| s.insert(k)),
        }
    }
    fn map(self, f: impl Fn(u64) -> u64) -> Vs {
        match self {
            Vs::S(..) => self.elems().iter().fold(Vs::Bot, |s, &k| s.insert(f(k))),
            x => x,
        }
    }
    fn cross(self, o: Vs, f: impl Fn(u64, u64) -> u64) -> Vs {
        match (self, o) {
            (Vs::Top, _) | (_, Vs::Top) => Vs::Top,
            (Vs::Bot, _) | (_, Vs::Bot) => Vs::Bot,
            (a, b) => {
                let mut s = Vs::Bot;
                for &x in a.elems() {
                    for &y in b.elems() {
                        s = s.insert(f(x, y));
                    }
                }
                s
            }
        }
    }
}

fn mask(w: u32) -> u64 {
    if w >= 64 { u64::MAX } else { (1u64 << w) - 1 }
}

fn sext(x: u64, w: u32) -> i64 {
    let s = 64 - w.min(64);
    ((x << s) as i64) >> s
}

pub(crate) fn cc_eval(cc: IntCC, a: u64, b: u64, w: u32) -> bool {
    let m = mask(w);
    let (a, b) = (a & m, b & m);
    let (sa, sb) = (sext(a, w), sext(b, w));
    match cc {
        IntCC::Equal => a == b,
        IntCC::NotEqual => a != b,
        IntCC::UnsignedLessThan => a < b,
        IntCC::UnsignedLessThanOrEqual => a <= b,
        IntCC::UnsignedGreaterThan => a > b,
        IntCC::UnsignedGreaterThanOrEqual => a >= b,
        IntCC::SignedLessThan => sa < sb,
        IntCC::SignedLessThanOrEqual => sa <= sb,
        IntCC::SignedGreaterThan => sa > sb,
        IntCC::SignedGreaterThanOrEqual => sa >= sb,
    }
}

fn int_w(func: &Function, v: Value) -> Option<u32> {
    let ty = func.dfg.value_type(v);
    (ty.is_int() && !ty.is_vector() && ty.bits() <= 64).then(|| ty.bits())
}

fn iconst(func: &Function, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    let w = int_w(func, v)?;
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits() as u64 & mask(w)),
        _ => None,
    }
}

/// `icmp cc x, K` with the constant on the right (swapping if needed).
fn icmp_k(func: &Function, i: Inst) -> Option<(IntCC, Value, u64)> {
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let (a, b) = (
        func.dfg.resolve_aliases(args[0]),
        func.dfg.resolve_aliases(args[1]),
    );
    if let Some(k) = iconst(func, b) {
        int_w(func, a)?;
        return Some((cond, a, k));
    }
    if let Some(k) = iconst(func, a) {
        int_w(func, b)?;
        return Some((cond.swap_args(), b, k));
    }
    None
}

/// Incoming edges per block: `(terminator, destination index)`.
pub(crate) type Edges = FxHashMap<Block, Vec<(Inst, usize)>>;

fn edges_of(func: &Function) -> Edges {
    let mut e: Edges = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for (d, bc) in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            e.entry(bc.block(&func.dfg.value_lists))
                .or_default()
                .push((t, d));
        }
    }
    e
}

/// Arg `ix` of edge `(t, d)`, when it's a plain value.
fn edge_arg(func: &Function, t: Inst, d: usize, ix: usize) -> Option<Value> {
    let bc = func.dfg.insts[t]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
    let a = bc.args(&func.dfg.value_lists).nth(ix)?;
    a.as_value().map(|v| func.dfg.resolve_aliases(v))
}

/// Narrow `s` (the set of `a`) by edge `(t, d)`'s own branch test when
/// it's `brif (icmp cc a, K)`.
fn narrow(func: &Function, s: Vs, a: Value, t: Inst, d: usize) -> Vs {
    let Vs::S(..) = s else { return s };
    let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else {
        return s;
    };
    let c = func.dfg.resolve_aliases(arg);
    let Some(ci) = func.dfg.value_def(c).inst() else {
        return s;
    };
    let Some((cc, x, k)) = icmp_k(func, ci) else {
        return s;
    };
    if x != a {
        return s;
    }
    let w = func.dfg.value_type(a).bits();
    let want = d == 0;
    s.elems()
        .iter()
        .filter(|&&e| cc_eval(cc, e, k, w) == want)
        .fold(Vs::Bot, |s, &e| s.insert(e))
}

/// The value-set analysis over the cone of every `icmp x, K`.
pub(crate) struct Sets {
    idx: FxHashMap<Value, u32>,
    vals: Vec<Value>,
    cur: Vec<Vs>,
}

impl Sets {
    /// Set of `v` (resolved): from the cone, else the leaf rules.
    pub(crate) fn get(&self, func: &Function, v: Value) -> Vs {
        let v = func.dfg.resolve_aliases(v);
        if let Some(&i) = self.idx.get(&v) {
            return self.cur[i as usize];
        }
        leaf(func, v).unwrap_or(Vs::Top)
    }

    pub(crate) fn build(func: &Function, edges: &Edges) -> Sets {
        let entry = func.layout.entry_block();
        let mut s = Sets {
            idx: FxHashMap::default(),
            vals: Vec::new(),
            cur: Vec::new(),
        };
        let mut users: Vec<Vec<u32>> = Vec::new();
        let mut stack: Vec<Value> = Vec::new();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                if let Some((_, x, _)) = icmp_k(func, i) {
                    stack.push(x);
                }
            }
        }
        // Discover the cone; `deps` are each member's operands.
        let mut deps: Vec<Vec<Value>> = Vec::new();
        while let Some(v) = stack.pop() {
            if s.idx.contains_key(&v) || leaf(func, v).is_some() || int_w(func, v).is_none() {
                continue;
            }
            if s.vals.len() >= MAX_CONE {
                break;
            }
            let n = s.vals.len() as u32;
            s.idx.insert(v, n);
            s.vals.push(v);
            s.cur.push(Vs::Bot);
            users.push(Vec::new());
            let ds = operands(func, edges, entry, v);
            for &d in ds.iter().flatten() {
                stack.push(d);
            }
            deps.push(ds.unwrap_or_default());
        }
        // Members past the budget, or with a top-forcing def, are top.
        for (i, ds) in deps.iter().enumerate() {
            for &d in ds {
                if let Some(&j) = s.idx.get(&d) {
                    users[j as usize].push(i as u32);
                }
            }
        }
        let mut work: Vec<u32> = (0..s.vals.len() as u32).collect();
        let mut queued: Vec<bool> = vec![true; s.vals.len()];
        while let Some(i) = work.pop() {
            queued[i as usize] = false;
            let v = s.vals[i as usize];
            let nv = s.transfer(func, edges, entry, v);
            let old = s.cur[i as usize];
            let j = old.join(nv);
            if j != old {
                s.cur[i as usize] = j;
                for &u in &users[i as usize] {
                    if !queued[u as usize] {
                        queued[u as usize] = true;
                        work.push(u);
                    }
                }
            }
        }
        s
    }

    fn transfer(&self, func: &Function, edges: &Edges, entry: Option<Block>, v: Value) -> Vs {
        let w = func.dfg.value_type(v).bits();
        let m = mask(w);
        match func.dfg.value_def(v) {
            ValueDef::Param(b, ix) => {
                if Some(b) == entry {
                    return Vs::Top;
                }
                let Some(es) = edges.get(&b) else {
                    return Vs::Top;
                };
                let mut acc = Vs::Bot;
                for &(t, d) in es {
                    let Some(a) = edge_arg(func, t, d, ix) else {
                        return Vs::Top;
                    };
                    acc = acc.join(narrow(func, self.get(func, a), a, t, d));
                    if acc == Vs::Top {
                        break;
                    }
                }
                acc
            }
            ValueDef::Result(i, 0) => match func.dfg.insts[i] {
                InstructionData::Unary { opcode, arg } => {
                    let aw = func.dfg.value_type(arg).bits();
                    let a = self.get(func, arg);
                    match opcode {
                        Opcode::Uextend | Opcode::Ireduce => a.map(|x| x & m),
                        Opcode::Sextend => a.map(|x| sext(x, aw) as u64 & m),
                        Opcode::Bnot => a.map(|x| !x & m),
                        Opcode::Ineg => a.map(|x| x.wrapping_neg() & m),
                        _ => Vs::Top,
                    }
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    let c = self.get(func, args[0]);
                    let (a, b) = (self.get(func, args[1]), self.get(func, args[2]));
                    match c {
                        Vs::S(..) if c.elems().iter().all(|&k| k != 0) => a,
                        Vs::S(..) if c.elems().iter().all(|&k| k == 0) => b,
                        _ => a.join(b),
                    }
                }
                InstructionData::Binary { opcode, args } => {
                    let (a, b) = (self.get(func, args[0]), self.get(func, args[1]));
                    let f: fn(u64, u64) -> u64 = match opcode {
                        Opcode::Iadd => u64::wrapping_add,
                        Opcode::Isub => u64::wrapping_sub,
                        Opcode::Imul => u64::wrapping_mul,
                        Opcode::Band => |x, y| x & y,
                        Opcode::Bor => |x, y| x | y,
                        Opcode::Bxor => |x, y| x ^ y,
                        _ => return Vs::Top,
                    };
                    a.cross(b, |x, y| f(x, y) & m)
                }
                _ => Vs::Top,
            },
            _ => Vs::Top,
        }
    }
}

/// Values whose set needs no operands.
fn leaf(func: &Function, v: Value) -> Option<Vs> {
    if let Some(k) = iconst(func, v) {
        return Some(Vs::one(k));
    }
    let i = func.dfg.value_def(v).inst()?;
    matches!(func.dfg.insts[i].opcode(), Opcode::Icmp | Opcode::Fcmp)
        .then(|| Vs::one(0).insert(1))
}

/// Operands the transfer of `v` reads; `None` when it's always top.
fn operands(func: &Function, edges: &Edges, entry: Option<Block>, v: Value) -> Option<Vec<Value>> {
    match func.dfg.value_def(v) {
        ValueDef::Param(b, ix) => {
            if Some(b) == entry {
                return None;
            }
            let es = edges.get(&b)?;
            let mut out = Vec::with_capacity(es.len());
            for &(t, d) in es {
                out.push(edge_arg(func, t, d, ix)?);
            }
            Some(out)
        }
        ValueDef::Result(i, 0) => match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Uextend | Opcode::Sextend | Opcode::Ireduce | Opcode::Bnot
                | Opcode::Ineg,
                arg,
            } => Some(vec![func.dfg.resolve_aliases(arg)]),
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } => Some(args.iter().map(|&a| func.dfg.resolve_aliases(a)).collect()),
            InstructionData::Binary {
                opcode:
                    Opcode::Iadd | Opcode::Isub | Opcode::Imul | Opcode::Band | Opcode::Bor
                    | Opcode::Bxor,
                args,
            } => Some(args.iter().map(|&a| func.dfg.resolve_aliases(a)).collect()),
            _ => None,
        },
        _ => None,
    }
}

/// Phase 1: compares decided by the value sets become constants;
/// `select`s on a now-constant condition alias their arm.
fn fold_decided(func: &mut Function, sets: &Sets) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(i) = pos.next_inst() {
            if let Some((cc, x, k)) = icmp_k(pos.func, i) {
                let s = sets.get(pos.func, x);
                let w = pos.func.dfg.value_type(x).bits();
                let es = s.elems();
                if !es.is_empty() {
                    let r0 = cc_eval(cc, es[0], k, w);
                    if es.iter().all(|&e| cc_eval(cc, e, k, w) == r0) {
                        let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(i));
                        pos.func.replace(i).iconst(ty, r0 as i64);
                        n += 1;
                    }
                }
                continue;
            }
            if let InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } = pos.func.dfg.insts[i]
                && let Some(c) = iconst(pos.func, args[0])
            {
                let arm = pos.func.dfg.resolve_aliases(args[if c != 0 { 1 } else { 2 }]);
                let r = pos.func.dfg.first_result(i);
                if pos.func.dfg.value_type(arm) == pos.func.dfg.value_type(r) {
                    pos.func.dfg.clear_results(i);
                    pos.remove_inst_and_step_back();
                    pos.func.dfg.change_to_alias(r, arm);
                    n += 1;
                }
            }
        }
    }
    n
}

/// One compare reading param `p`: `icmp cc (ext p), K`.
#[derive(Clone, Copy)]
struct Query {
    inst: Inst,
    cc: IntCC,
    k: u64,
    /// Width the compare runs at.
    w: u32,
    /// `uextend`/`sextend` to `Type` between `p` and the compare.
    ext: Option<(Opcode, Type, Inst)>,
}

/// Every use of `p` as a compare (direct or through one extend), or
/// `None` when `p` has any other use.
fn queries(func: &Function, users: &FxHashMap<Value, Vec<Inst>>, p: Value) -> Option<Vec<Query>> {
    let live = |i: Inst| func.layout.inst_block(i).is_some();
    let mentions = |i: Inst, v: Value| {
        func.dfg
            .inst_values(i)
            .any(|x| func.dfg.resolve_aliases(x) == v)
    };
    let mut out: Vec<Query> = Vec::new();
    let mut seen: FxHashSet<Inst> = FxHashSet::default();
    for &u in users.get(&p).map(|v| v.as_slice()).unwrap_or(&[]) {
        if !live(u) || !seen.insert(u) || !mentions(u, p) {
            continue;
        }
        if let Some((cc, x, k)) = icmp_k(func, u)
            && x == p
            && func.dfg.value_type(func.dfg.first_result(u)) == types::I8
        {
            let w = func.dfg.value_type(p).bits();
            out.push(Query {
                inst: u,
                cc,
                k,
                w,
                ext: None,
            });
            continue;
        }
        let InstructionData::Unary {
            opcode: op @ (Opcode::Uextend | Opcode::Sextend),
            ..
        } = func.dfg.insts[u]
        else {
            return None;
        };
        let e = func.dfg.first_result(u);
        let ty = func.dfg.value_type(e);
        int_w(func, e)?;
        let mut any = false;
        let mut seen2: FxHashSet<Inst> = FxHashSet::default();
        for &u2 in users.get(&e).map(|v| v.as_slice()).unwrap_or(&[]) {
            if !live(u2) || !seen2.insert(u2) || !mentions(u2, e) {
                continue;
            }
            let (cc, x, k) = icmp_k(func, u2)?;
            if x != e || func.dfg.value_type(func.dfg.first_result(u2)) != types::I8 {
                return None;
            }
            out.push(Query {
                inst: u2,
                cc,
                k,
                w: ty.bits(),
                ext: Some((op, ty, u)),
            });
            any = true;
        }
        if !any {
            return None;
        }
    }
    (!out.is_empty() && out.len() <= MAX_QUERIES).then_some(out)
}

/// The query's value on an edge whose arg has set `s`, when decided.
fn decide(q: &Query, s: Vs, aw: u32) -> Option<bool> {
    let es = s.elems();
    if es.is_empty() {
        return None;
    }
    let ext = |x: u64| match q.ext {
        Some((Opcode::Sextend, ..)) => sext(x, aw) as u64 & mask(q.w),
        _ => x,
    };
    let r0 = cc_eval(q.cc, ext(es[0]), q.k, q.w);
    es.iter()
        .all(|&e| cc_eval(q.cc, ext(e), q.k, q.w) == r0)
        .then_some(r0)
}

/// An `Ordering`-constructor-ish def: the arg's own compare tree dies
/// with the param, so recomputing the query on it is free.
/// Returns the instructions saved per edge: a `select`/`isub` tree (~3)
/// or a bool `icmp` the compare folds onto (1).
fn constructor(func: &Function, a: Value) -> Option<i64> {
    let i = func.dfg.value_def(a).inst()?;
    match func.dfg.insts[i].opcode() {
        Opcode::Select | Opcode::Isub => Some(3),
        Opcode::Icmp => Some(1),
        _ => None,
    }
}

/// Phase 2: push all-compare param uses into the incoming edges.
fn push_through(
    func: &mut Function,
    sets: &Sets,
    edges: &Edges,
    order: &[Block],
    reuse: &mut Option<crate::clifpeep::CmpReuse>,
) -> usize {
    let mut users: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                users.entry(func.dfg.resolve_aliases(v)).or_default().push(i);
            }
        }
    }
    let entry = func.layout.entry_block();
    let mut n = 0;
    for &b in order {
        if Some(b) == entry || func.layout.first_inst(b).is_none() {
            continue;
        }
        let Some(es) = edges.get(&b) else { continue };
        if es.is_empty() || es.len() > MAX_EDGES {
            continue;
        }
        let mut ix = 0;
        while ix < func.dfg.block_params(b).len() {
            let p = func.dfg.block_params(b)[ix];
            if !try_push(func, sets, es, &mut users, b, ix, p, reuse) {
                ix += 1;
            } else {
                n += 1;
                // `p`'s slot was removed; `ix` now names the next param.
            }
        }
    }
    n
}

#[allow(clippy::too_many_arguments)]
fn try_push(
    func: &mut Function,
    sets: &Sets,
    es: &[(Inst, usize)],
    users: &mut FxHashMap<Value, Vec<Inst>>,
    b: Block,
    ix: usize,
    p: Value,
    reuse: &mut Option<crate::clifpeep::CmpReuse>,
) -> bool {
    let Some(aw) = int_w(func, p) else {
        return false;
    };
    let Some(qs) = queries(func, users, p) else {
        return false;
    };
    // Every edge must hand a plain value at `ix`, and none may pass `p`
    // itself (a self-loop would keep `p` alive).
    let mut args: Vec<Value> = Vec::with_capacity(es.len());
    let nslots = {
        let mut ks: Vec<(IntCC, u64, Option<(Opcode, Type)>)> = Vec::new();
        for q in &qs {
            let k = (q.cc, q.k, q.ext.map(|(o, t, _)| (o, t)));
            if !ks.contains(&k) {
                ks.push(k);
            }
        }
        ks.len() as i64
    };
    // Rough instruction score: the join's compares (and extends) go
    // away; a decided edge passes `nslots` constants for the old one
    // value; an `Ordering`-constructor arg's select tree dies (~3); an
    // opaque arg pays every compare (and one extend) itself.
    let next = qs.iter().filter(|q| q.ext.is_some()).count().min(1) as i64;
    let mut score: i64 = nslots + next;
    let mut opaque = 0usize;
    for &(t, d) in es {
        let Some(a) = edge_arg(func, t, d, ix) else {
            return false;
        };
        let bc = func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
        if bc
            .args(&func.dfg.value_lists)
            .any(|x| x.as_value().is_some_and(|x| func.dfg.resolve_aliases(x) == p))
        {
            return false;
        }
        let s = narrow(func, sets.get(func, a), a, t, d);
        if qs.iter().all(|q| decide(q, s, aw).is_some()) {
            score -= nslots - 1;
        } else if let Some(g) = constructor(func, a) {
            score += g - (nslots - 1);
        } else {
            score -= nslots + next + (nslots - 1);
            opaque += 1;
        }
        args.push(a);
    }
    if score <= 0 || opaque > 1 && score < 2 {
        return false;
    }
    // Dedupe identical queries onto one new param.
    let mut slots: Vec<(IntCC, u64, Option<(Opcode, Type)>)> = Vec::new();
    let mut slot_of: Vec<usize> = Vec::with_capacity(qs.len());
    for q in &qs {
        let key = (q.cc, q.k, q.ext.map(|(o, t, _)| (o, t)));
        let j = slots.iter().position(|s| *s == key).unwrap_or_else(|| {
            slots.push(key);
            slots.len() - 1
        });
        slot_of.push(j);
    }
    let qrep: Vec<Query> = slots
        .iter()
        .map(|s| *qs.iter().find(|q| (q.cc, q.k, q.ext.map(|(o, t, _)| (o, t))) == *s).unwrap())
        .collect();
    // Per edge: compute each slot's value before the terminator.
    for (ei, &(t, d)) in es.iter().enumerate() {
        let a = args[ei];
        let s = narrow(func, sets.get(func, a), a, t, d);
        let first_new = func.dfg.num_insts();
        let mut vals: Vec<Value> = Vec::with_capacity(qrep.len());
        for q in &qrep {
            let mut pos = FuncCursor::new(func).at_inst(t);
            let v = match decide(q, s, aw) {
                Some(r) => pos.ins().iconst(types::I8, r as i64),
                None => {
                    let x = match q.ext {
                        Some((Opcode::Sextend, ty, _)) => pos.ins().sextend(ty, a),
                        Some((_, ty, _)) => pos.ins().uextend(ty, a),
                        None => a,
                    };
                    let xty = pos.func.dfg.value_type(x);
                    let kk = pos.ins().iconst(xty, q.k as i64);
                    let c = pos.ins().icmp(q.cc, x, kk);
                    let ci = pos.func.dfg.value_def(c).inst().unwrap();
                    // Collapse it onto the arg's own compare pair now
                    // (`(a <=> b) < 0` → `a <u b`).
                    let mut pos = FuncCursor::new(func).at_inst(ci);
                    let InstructionData::IntCompare { cond, args: ca, .. } = pos.func.dfg.insts[ci]
                    else {
                        unreachable!()
                    };
                    crate::clifpeep::selord_icmp(&mut pos, ci, cond, ca, reuse);
                    func.dfg.resolve_aliases(c)
                }
            };
            vals.push(v);
        }
        // Register every inst minted for this edge (selord may build its
        // own compares on the arg's operands) so later `queries` see them.
        for k in first_new..func.dfg.num_insts() {
            let i = Inst::new(k);
            if func.layout.inst_block(i).is_none() {
                continue;
            }
            for v in func.dfg.inst_values(i).collect::<Vec<_>>() {
                users.entry(func.dfg.resolve_aliases(v)).or_default().push(i);
            }
        }
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[t]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
        for v in vals {
            bc.append_argument(v, &mut dfg.value_lists);
        }
    }
    // New params; the old compares alias them.
    let nps: Vec<Value> = (0..qrep.len())
        .map(|_| func.dfg.append_block_param(b, types::I8))
        .collect();
    let mut dead_ext: Vec<Inst> = Vec::new();
    for (q, &j) in qs.iter().zip(&slot_of) {
        let r = func.dfg.first_result(q.inst);
        let np = nps[j];
        func.dfg.clear_results(q.inst);
        func.layout.remove_inst(q.inst);
        func.dfg.change_to_alias(r, np);
        if let Some((_, _, xi)) = q.ext
            && !dead_ext.contains(&xi)
        {
            dead_ext.push(xi);
        }
    }
    for xi in dead_ext {
        func.dfg.clear_results(xi);
        func.layout.remove_inst(xi);
    }
    // Drop `p` and its slot.
    for &(t, d) in es {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[t]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
        bc.remove(ix, &mut dfg.value_lists);
    }
    func.dfg.remove_block_param(p);
    true
}

/// Run both phases; returns the number of rewrites.
pub fn run(func: &mut Function, dom: &DominatorTree) -> usize {
    let order: Vec<Block> = dom.cfg_postorder().to_vec();
    let edges = edges_of(func);
    let sets = Sets::build(func, &edges);
    let mut n = fold_decided(func, &sets);
    let mut reuse = None;
    n += push_through(func, &sets, &edges, &order, &mut reuse);
    if n > 0 {
        n += crate::jumpthread::remove_dead_insts(func, true);
    }
    n
}

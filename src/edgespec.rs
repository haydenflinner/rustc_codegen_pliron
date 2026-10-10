//! Thread a predecessor edge through `brif` dispatch blocks when the supplied
//! block-param args decide the branch.
//!
//! MIR's niche-optimized `Option`/enum plumbing reaches CLIF as a flag plus a
//! sentinel, then a chain of small dispatch blocks re-tests it:
//!
//! ```text
//! blockP:
//!     s = select.i64 flag, 0, 1        // tag materialized for the merge
//!     jump d(s, payload, ...)
//! blockd(f: i64, p: i64, q: i64):
//!     r = ireduce.i32 f
//!     c = icmp eq r, 1
//!     brif c, some_path, other_path
//! ```
//!
//! The `icmp` on the param is already decided by the pred's arg:
//! `select flag, 0, 1` gives `c == (flag == 0)`, so the jump becomes
//! `brif flag, other_path, some_path` — the `select`/`ireduce`/`icmp` chain
//! and the block-param copies on that edge disappear. Constant args resolve
//! outright (`d(2, ..)` threads past the `eq 1` arm to wherever `eq 2` goes),
//! and the walk follows `jump`-only and `brif` blocks until the outcome stops
//! being decided by the supplied args, so a multi-level discriminant cascade
//! collapses in one rewrite.
//!
//! The same evaluator folds same-block condition chains:
//! `brif (band (ireduce (select c, 0, 1)), 1)` is `brif c` with swapped arms;
//! a constant condition becomes a `jump`.
//!
//! Soundness: a walked block's non-terminator insts must be side-effect-free
//! (`jumpthread::pure_op`), and every argument carried to a final destination
//! must be available at the pred's tail — a param/inst dominating it, or an
//! `iconst` rematerialized there. Anything else leaves the edge unchanged.
//!
//! `PLIRON_EDGESPEC=0` disables it.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::I8;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
    ValueDef,
};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

/// Bound on pure-op chains evaluated per value, and on dispatch blocks
/// threaded per edge.
const MAX_EVAL: usize = 16;
const MAX_HOPS: usize = 8;

/// A value's worth under the supplied-arg environment: a constant, or
/// `S(c, t, e)` = `t` when `c != 0` else `e` for known `t`/`e`.
#[derive(Clone, Copy, PartialEq)]
enum E {
    Unk,
    K(u64),
    S(Value, u64, u64),
}

fn mask(v: u64, bits: u32) -> u64 {
    if bits >= 64 { v } else { v & ((1u64 << bits) - 1) }
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

fn eval_icmp(cc: IntCC, a: u64, b: u64, w: u32) -> bool {
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

fn iconst(func: &Function, v: Value) -> Option<u64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(mask(imm.bits() as u64, bits(func, v)?)),
        _ => None,
    }
}

/// `v`'s value set is provably {0,1}: usable as a decision value standing in
/// for itself.
fn is_bool(func: &Function, v: Value, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    let Some(i) = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst() else {
        return false;
    };
    match func.dfg.insts[i] {
        InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            ..
        } => true,
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => iconst(func, args[0]) == Some(1) || iconst(func, args[1]) == Some(1),
        InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } => {
            (is_bool(func, args[1], depth + 1) && is_bool(func, args[2], depth + 1))
                || (iconst(func, args[1]).is_some_and(|k| k <= 1)
                    && iconst(func, args[2]).is_some_and(|k| k <= 1))
        }
        InstructionData::Unary {
            opcode: Opcode::Ireduce | Opcode::Uextend | Opcode::Sextend,
            arg,
        } => is_bool(func, arg, depth + 1),
        _ => false,
    }
}

/// Evaluate `v` under `env` (walked-block params bound to supplied-arg
/// expressions; doubles as the memo table).
fn eval(
    func: &Function,
    env: &mut FxHashMap<Value, E>,
    sub: &FxHashMap<Value, Value>,
    v: Value,
    depth: usize,
) -> E {
    let v = func.dfg.resolve_aliases(v);
    if let Some(&e) = env.get(&v) {
        return e;
    }
    if depth > MAX_EVAL {
        return E::Unk;
    }
    let Some(w) = bits(func, v) else {
        return E::Unk;
    };
    let Some(i) = func.dfg.value_def(v).inst() else {
        return E::Unk;
    };
    let e = match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => E::K(mask(imm.bits() as u64, w)),
        InstructionData::Unary {
            opcode: Opcode::Ireduce,
            arg,
        } => match eval(func, env, sub, arg, depth + 1) {
            E::K(k) => E::K(mask(k, w)),
            E::S(c, t, e) => E::S(c, mask(t, w), mask(e, w)),
            E::Unk => E::Unk,
        },
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => eval(func, env, sub, arg, depth + 1),
        InstructionData::Unary {
            opcode: Opcode::Sextend,
            arg,
        } => {
            let sw = bits(func, arg).unwrap_or(64);
            match eval(func, env, sub, arg, depth + 1) {
                E::K(k) => E::K(mask(sext(k, sw) as u64, w)),
                E::S(c, t, e) => E::S(c, mask(sext(t, sw) as u64, w), mask(sext(e, sw) as u64, w)),
                E::Unk => E::Unk,
            }
        }
        InstructionData::Unary {
            opcode: Opcode::Bnot,
            arg,
        } => match eval(func, env, sub, arg, depth + 1) {
            E::K(k) => E::K(mask(!k, w)),
            E::S(c, t, e) => E::S(c, mask(!t, w), mask(!e, w)),
            E::Unk => E::Unk,
        },
        InstructionData::Binary {
            opcode: op @ (Opcode::Band | Opcode::Bor | Opcode::Bxor),
            args,
        } => {
            let op = |a: u64, b: u64| -> u64 {
                match op {
                    Opcode::Band => a & b,
                    Opcode::Bor => a | b,
                    _ => a ^ b,
                }
            };
            match (
                eval(func, env, sub, args[0], depth + 1),
                eval(func, env, sub, args[1], depth + 1),
            ) {
                (E::K(a), E::K(b)) => E::K(mask(op(a, b), w)),
                (E::S(c, t, e), E::K(b)) | (E::K(b), E::S(c, t, e)) => {
                    E::S(c, mask(op(t, b), w), mask(op(e, b), w))
                }
                (E::S(c, a, b), E::S(c2, x, y)) if c == c2 => {
                    E::S(c, mask(op(a, x), w), mask(op(b, y), w))
                }
                _ => E::Unk,
            }
        }
        InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args,
        } => {
            let Some(ow) = bits(func, args[0]) else {
                return E::Unk;
            };
            let f = |x: u64, y: u64| eval_icmp(cond, mask(x, ow), mask(y, ow), ow) as u64;
            match (
                eval(func, env, sub, args[0], depth + 1),
                eval(func, env, sub, args[1], depth + 1),
            ) {
                (E::K(x), E::K(y)) => E::K(f(x, y)),
                (E::S(c, t, e), E::K(y)) => E::S(c, f(t, y), f(e, y)),
                (E::K(x), E::S(c, t, e)) => E::S(c, f(x, t), f(x, e)),
                _ => E::Unk,
            }
        }
        InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } => {
            match eval(func, env, sub, args[0], depth + 1) {
                E::K(k) => {
                    if k != 0 {
                        eval(func, env, sub, args[1], depth + 1)
                    } else {
                        eval(func, env, sub, args[2], depth + 1)
                    }
                }
                ce => {
                    if let (E::K(t), E::K(e)) = (
                        eval(func, env, sub, args[1], depth + 1),
                        eval(func, env, sub, args[2], depth + 1),
                    ) {
                        let (t, e) = (mask(t, w), mask(e, w));
                        if t == e {
                            E::K(t)
                        } else {
                            match ce {
                                // The cond's truth is itself decided by `c`:
                                // `select (c?a:b), t, e`.
                                E::S(c, a, b) => {
                                    if a != 0 && b == 0 {
                                        E::S(c, t, e)
                                    } else if a == 0 && b != 0 {
                                        E::S(c, e, t)
                                    } else {
                                        E::K(if a != 0 { t } else { e })
                                    }
                                }
                                // Unknown cond: name the pred-side value it
                                // forwards to (a walked param is useless at
                                // the pred without the substitution).
                                E::Unk => E::S(subv(func, sub, args[0]), t, e),
                                E::K(_) => unreachable!(),
                            }
                        }
                    } else {
                        E::Unk
                    }
                }
            }
        }
        _ => E::Unk,
    };
    env.insert(v, e);
    e
}

/// Chase `v` through the walked-param substitution to the value the pred
/// actually supplies.
fn subv(func: &Function, sub: &FxHashMap<Value, Value>, v: Value) -> Value {
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..16 {
        match sub.get(&v) {
            Some(&s) => v = func.dfg.resolve_aliases(s),
            None => break,
        }
    }
    v
}

/// An edge outcome: a final destination plus substituted arguments, or a
/// split decided by a condition materializable at the pred's tail.
enum Edge {
    Fin(Block, Vec<Value>),
    Br(A, Box<Edge>, Box<Edge>),
}

struct W<'a> {
    func: &'a Function,
    cfg: &'a ControlFlowGraph,
    dom: &'a DominatorTree,
    p: Block,
    pterm: Inst,
}

impl<'a> W<'a> {
    /// Every non-terminator inst in `b` is side-effect-free.
    fn pure(&self, b: Block) -> bool {
        let insts: Vec<Inst> = self.func.layout.block_insts(b).collect();
        for (n, &i) in insts.iter().enumerate() {
            if n + 1 == insts.len() {
                break;
            }
            if !crate::jumpthread::pure_op(self.func, i) {
                return false;
            }
        }
        true
    }

    /// Substitute walked-block params in a call's args with pred values.
    fn map_call(&self, bc: BlockCall, sub: &FxHashMap<Value, Value>) -> Option<(Block, Vec<Value>)> {
        let b = bc.block(&self.func.dfg.value_lists);
        let mut args = Vec::new();
        for a in bc.args(&self.func.dfg.value_lists) {
            let BlockArg::Value(v) = a else { return None };
            let v = self.func.dfg.resolve_aliases(v);
            args.push(*sub.get(&v).unwrap_or(&v));
        }
        Some((b, args))
    }

    /// `v` materialized at `p`'s tail: a dominating value, or an `iconst` to
    /// rematerialize. Walked-block params chase the substitution.
    ///
    /// A def inside a skipped block is not usable even when it dominates
    /// `p`: the walked edge would have re-bound it on the way, so the value
    /// observed at `p` is stale.
    fn avail_a(
        &self,
        mut v: Value,
        sub: &FxHashMap<Value, Value>,
        skipped: &FxHashSet<Block>,
    ) -> Option<A> {
        v = self.func.dfg.resolve_aliases(v);
        for _ in 0..16 {
            if let Some(&s) = sub.get(&v) {
                v = self.func.dfg.resolve_aliases(s);
                continue;
            }
            match self.func.dfg.value_def(v) {
                ValueDef::Param(b, _)
                    if !skipped.contains(&b)
                        && (b == self.p
                            || (self.func.layout.is_block_inserted(b)
                                && self.dom.block_dominates(b, self.p))) =>
                {
                    return Some(A::V(v));
                }
                ValueDef::Result(i, _) => {
                    let inst = self.func.dfg.insts[i];
                    if let InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                    } = inst
                    {
                        return Some(A::K(self.func.dfg.value_type(v), imm.bits()));
                    }
                    let ib = self.func.layout.inst_block(i);
                    if let Some(b) = ib
                        && !skipped.contains(&b)
                        && self.dom.dominates(i, self.pterm, &self.func.layout)
                    {
                        return Some(A::V(v));
                    }
                    return None;
                }
                _ => return None,
            }
        }
        None
    }

    /// Landing an edge at `d` from `p`'s tail is legal only if every
    /// upward-exposed use anywhere in `d`'s forward cone still has its def on
    /// every path: defs dominating `p`'s tail stay above the new edge, and
    /// defs inside the cone must cone-dominate their use (the new path
    /// `p → d → … → u` then still passes through them). Anything else — a def
    /// in a skipped or side block that a `p → d → u` path bypasses — breaks
    /// SSA the moment the old path disappears, so the fold is refused.
    ///
    /// Defs in `skipped` blocks are refused outright: the original path
    /// re-bound them on the way (a walked block's params take the skipped
    /// edge's args, its insts re-execute), so even a skipped def that
    /// dominates `p` yields a stale value on the threaded edge.
    fn cone_ok(&self, d: Block, skipped: &FxHashSet<Block>) -> bool {
        if d == self.p && skipped.is_empty() {
            return true;
        }
        const MAX_CONE: usize = 64;
        let mut order = vec![d];
        let mut seen: FxHashSet<Block> = [d].into_iter().collect();
        let mut i = 0;
        while i < order.len() {
            if order.len() > MAX_CONE {
                return false;
            }
            let b = order[i];
            i += 1;
            let Some(t) = self.func.layout.last_inst(b) else {
                continue;
            };
            for bc in self.func.dfg.insts[t].branch_destination(
                &self.func.dfg.jump_tables,
                &self.func.dfg.exception_tables,
            ) {
                let s = bc.block(&self.func.dfg.value_lists);
                if seen.insert(s) {
                    order.push(s);
                }
            }
        }
        let n = order.len();
        let mut midx: FxHashMap<Block, u32> = FxHashMap::default();
        for (k, &b) in order.iter().enumerate() {
            midx.insert(b, k as u32);
        }
        // Local dominators (bit k set <=> order[k] on every d→u cone path).
        let full = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
        let mut cdom = vec![full; n];
        cdom[0] = 1;
        loop {
            let mut changed = false;
            for k in 1..n {
                let u = order[k];
                let mut m = full;
                let mut any = false;
                for pr in self.cfg.pred_iter(u) {
                    if let Some(&j) = midx.get(&pr.block) {
                        m &= cdom[j as usize];
                        any = true;
                    }
                }
                let nv = if any { m } else { 0 } | (1 << k);
                if nv != cdom[k] {
                    cdom[k] = nv;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (k, &u) in order.iter().enumerate() {
            for inst in self.func.layout.block_insts(u) {
                for v in self.func.dfg.inst_values(inst) {
                    let v = self.func.dfg.resolve_aliases(v);
                    let db = match self.func.dfg.value_def(v) {
                        ValueDef::Param(b, _) if b == u => continue,
                        ValueDef::Param(b, _) => b,
                        ValueDef::Result(di, _) => {
                            match self.func.layout.inst_block(di) {
                                Some(b) if b == u => continue,
                                None => return false,
                                Some(b) => {
                                    if !skipped.contains(&b)
                                        && self.dom.dominates(
                                            di,
                                            self.pterm,
                                            &self.func.layout,
                                        )
                                    {
                                        continue;
                                    }
                                    b
                                }
                            }
                        }
                        _ => return false,
                    };
                    // A def-block that cone-dominates `u` stays on every new
                    // path to `u`; a param/block dominating `p` stays above
                    // the new edge — unless it is skipped: the walked edge
                    // would have re-bound it, so the use would see stale.
                    if midx.get(&db).is_some_and(|&j| cdom[k] & (1 << j) != 0)
                        || (!skipped.contains(&db)
                            && self.func.layout.is_block_inserted(db)
                            && self.dom.block_dominates(db, self.p))
                    {
                        continue;
                    }
                    return false;
                }
            }
        }
        true
    }

    /// Settle on `d`: it stays on this edge, so the skipped-prefix defs must
    /// not be consumed anywhere downstream (`cone_ok`) and the carried args
    /// must not themselves be skipped defs that a re-walked binding would
    /// have refreshed.
    fn fin_edge(
        &self,
        d: Block,
        args: Vec<Value>,
        skipped: &FxHashSet<Block>,
    ) -> Option<Edge> {
        if !self.cone_ok(d, skipped) {
            return None;
        }
        for &a in &args {
            let a = self.func.dfg.resolve_aliases(a);
            let db = match self.func.dfg.value_def(a) {
                ValueDef::Param(b, _) => Some(b),
                ValueDef::Result(i, _) => self.func.layout.inst_block(i),
                _ => None,
            };
            if db.is_some_and(|b| skipped.contains(&b)) {
                return None;
            }
        }
        Some(Edge::Fin(d, args))
    }

    /// Follow an edge into `d` with `args`, folding through decided
    /// conditions. `skipped` accumulates the walked blocks the threaded edge
    /// bypasses; their defs may not be observed downstream (`fin_edge` /
    /// `avail_a` enforce). Only the outermost level (`split_ok`) may produce
    /// a split; deeper levels collapse a still-undecided condition back to
    /// the unfolded edge.
    fn resolve(
        &self,
        env: FxHashMap<Value, E>,
        sub: FxHashMap<Value, Value>,
        d: Block,
        args: Vec<Value>,
        depth: usize,
        split_ok: bool,
        skipped: FxHashSet<Block>,
    ) -> Option<Edge> {
        if depth >= MAX_HOPS || !self.pure(d) {
            return self.fin_edge(d, args, &skipped);
        }
        if d == self.p {
            return self.fin_edge(d, args, &skipped);
        }
        let params = self.func.dfg.block_params(d).to_vec();
        if params.len() != args.len() {
            return self.fin_edge(d, args, &skipped);
        }
        let mut env = env.clone();
        let mut sub = sub;
        for (&prm, &a) in params.iter().zip(&args) {
            let e = eval(self.func, &mut env, &sub, a, 0);
            let e = if e == E::Unk && is_bool(self.func, a, 0) {
                E::S(a, 1, 0)
            } else {
                e
            };
            env.insert(prm, e);
            sub.insert(prm, a);
        }
        let term = self.func.layout.last_inst(d)?;
        // `d` is consumed only when we follow its terminator through to a
        // successor; landing on `d` keeps its defs live. `through` is the
        // skipped set for the consumed paths, `skipped` for landing here.
        let mut through = skipped.clone();
        through.insert(d);
        /// Consume `d`'s terminator and continue to `nb`; if the deeper walk
        /// cannot be made legal, land on `d` instead — still threading the
        /// prefix between `p` and `d`.
        macro_rules! go {
            ($env:expr, $sub:expr, $nb:expr, $nargs:expr) => {
                self.resolve($env, $sub, $nb, $nargs, depth + 1, split_ok, through.clone())
                    .or_else(|| self.fin_edge(d, args.clone(), &skipped))
            };
        }
        match self.func.dfg.insts[term] {
            InstructionData::Jump { destination, .. } => {
                let (nb, nargs) = self.map_call(destination, &sub)?;
                go!(env, sub, nb, nargs)
            }
            InstructionData::Brif { arg: c, blocks, .. } => {
                match eval(self.func, &mut env, &sub, c, 0) {
                    E::Unk => self.fin_edge(d, args, &skipped),
                    E::K(k) => {
                        let (nb, nargs) = self.map_call(blocks[usize::from(k == 0)], &sub)?;
                        go!(env, sub, nb, nargs)
                    }
                    E::S(cv, tv, ev) => {
                        let (tbc, ebc) = if tv != 0 && ev == 0 {
                            (blocks[0], blocks[1])
                        } else if tv == 0 && ev != 0 {
                            (blocks[1], blocks[0])
                        } else {
                            // tv == ev, or both nonzero: the condition is
                            // constant from this edge's point of view.
                            let (nb, nargs) =
                                self.map_call(blocks[usize::from(tv == 0)], &sub)?;
                            return go!(env, sub, nb, nargs);
                        };
                        if !split_ok {
                            return self.fin_edge(d, args, &skipped);
                        }
                        let Some(cv) = self.avail_a(cv, &sub, &through) else {
                            return self.fin_edge(d, args, &skipped);
                        };
                        let (tb, ta) = self.map_call(tbc, &sub)?;
                        let (eb, ea) = self.map_call(ebc, &sub)?;
                        let te = self
                            .resolve(
                                env.clone(),
                                sub.clone(),
                                tb,
                                ta.clone(),
                                depth + 1,
                                false,
                                through.clone(),
                            )
                            .or_else(|| self.fin_edge(tb, ta, &through))?;
                        let ee = self
                            .resolve(env, sub, eb, ea.clone(), depth + 1, false, through.clone())
                            .or_else(|| self.fin_edge(eb, ea, &through))?;
                        Some(Edge::Br(cv, Box::new(te), Box::new(ee)))
                    }
                }
            }
            InstructionData::BranchTable { arg, table, .. } => {
                let jt = &self.func.dfg.jump_tables[table];
                let default = jt.default_block();
                let pick = |k: u64| -> BlockCall {
                    if (k as usize) < jt.as_slice().len() {
                        jt.as_slice()[k as usize]
                    } else {
                        default
                    }
                };
                match eval(self.func, &mut env, &sub, arg, 0) {
                    E::Unk => self.fin_edge(d, args, &skipped),
                    E::K(k) => {
                        let (nb, nargs) = self.map_call(pick(k), &sub)?;
                        go!(env, sub, nb, nargs)
                    }
                    E::S(cv, tv, ev) if split_ok && tv != ev => {
                        let Some(cv) = self.avail_a(cv, &sub, &through) else {
                            return self.fin_edge(d, args, &skipped);
                        };
                        let (tb, ta) = self.map_call(pick(tv), &sub)?;
                        let (eb, ea) = self.map_call(pick(ev), &sub)?;
                        let te = self
                            .resolve(
                                env.clone(),
                                sub.clone(),
                                tb,
                                ta.clone(),
                                depth + 1,
                                false,
                                through.clone(),
                            )
                            .or_else(|| self.fin_edge(tb, ta, &through))?;
                        let ee = self
                            .resolve(env, sub, eb, ea.clone(), depth + 1, false, through.clone())
                            .or_else(|| self.fin_edge(eb, ea, &through))?;
                        Some(Edge::Br(cv, Box::new(te), Box::new(ee)))
                    }
                    _ => self.fin_edge(d, args, &skipped),
                }
            }
            _ => self.fin_edge(d, args, &skipped),
        }
    }
}

/// A planned replacement terminator for block `p`.
enum NewTerm {
    Jump(Block, Vec<Value>),
    Brif(A, Block, Vec<Value>, Block, Vec<Value>),
}

fn call_parts(func: &Function, bc: BlockCall) -> Option<(Block, Vec<Value>)> {
    let b = bc.block(&func.dfg.value_lists);
    let mut args = Vec::new();
    for a in bc.args(&func.dfg.value_lists) {
        let BlockArg::Value(v) = a else { return None };
        args.push(v);
    }
    Some((b, args))
}

/// Flatten to a single edge; a nested split keeps the original destination.
fn fin(e: Edge, orig: (Block, Vec<Value>)) -> (Block, Vec<Value>) {
    match e {
        Edge::Fin(b, a) => (b, a),
        Edge::Br(..) => orig,
    }
}

fn plan(
    func: &Function,
    cfg: &ControlFlowGraph,
    dom: &DominatorTree,
    p: Block,
    pterm: Inst,
) -> Option<NewTerm> {
    let w = W { func, cfg, dom, p, pterm };
    match func.dfg.insts[pterm] {
        InstructionData::Jump { destination, .. } => {
            let (d, args) = call_parts(func, destination)?;
            let od = (d, args.clone());
            match w.resolve(
                FxHashMap::default(),
                FxHashMap::default(),
                d,
                args,
                0,
                true,
                FxHashSet::default(),
            )? {
                Edge::Br(cv, t, e) => {
                    let (tb, ta) = fin(*t, od.clone());
                    let (eb, ea) = fin(*e, od);
                    Some(NewTerm::Brif(cv, tb, ta, eb, ea))
                }
                e @ Edge::Fin(..) => {
                    let (d2, a2) = fin(e, od);
                    (d2 != d).then(|| NewTerm::Jump(d2, a2))
                }
            }
        }
        InstructionData::Brif { arg: c, blocks, .. } => {
            match eval(func, &mut FxHashMap::default(), &FxHashMap::default(), c, 0) {
                E::K(k) => {
                    // Constant cond: take the arm, then keep threading.
                    let (d, args) = call_parts(func, blocks[usize::from(k == 0)])?;
                    let od = (d, args.clone());
                    match w.resolve(
                        FxHashMap::default(),
                        FxHashMap::default(),
                        d,
                        args,
                        0,
                        true,
                        FxHashSet::default(),
                    )? {
                        Edge::Br(cv, t, e) => {
                            let (tb, ta) = fin(*t, od.clone());
                            let (eb, ea) = fin(*e, od);
                            Some(NewTerm::Brif(cv, tb, ta, eb, ea))
                        }
                        e => {
                            let (d2, a2) = fin(e, od);
                            Some(NewTerm::Jump(d2, a2))
                        }
                    }
                }
                e => {
                    // Thread one edge past dispatch blocks (split_ok off —
                    // splits only originate at the block's own terminator).
                    let thread = |bc: BlockCall| -> Option<(Block, Vec<Value>)> {
                        let (d, args) = call_parts(func, bc)?;
                        let od = (d, args.clone());
                        if d == p {
                            return Some(od);
                        }
                        let e = w.resolve(
                            FxHashMap::default(),
                            FxHashMap::default(),
                            d,
                            args,
                            0,
                            false,
                            FxHashSet::default(),
                        )?;
                        Some(fin(e, od))
                    };
                    if let E::S(cv, tv, ev) = e
                        && tv != ev
                        && (tv == 0 || ev == 0)
                        && let Some(cva) =
                            w.avail_a(cv, &FxHashMap::default(), &FxHashSet::default())
                    {
                        // `cv ? tv : ev` with one side 0: the branch is
                        // decided by `cv` alone (arms swapped when `tv == 0`).
                        let (tbc, ebc) = if tv != 0 {
                            (blocks[0], blocks[1])
                        } else {
                            (blocks[1], blocks[0])
                        };
                        let (tb, ta) = thread(tbc)?;
                        let (eb, ea) = thread(ebc)?;
                        return Some(NewTerm::Brif(cva, tb, ta, eb, ea));
                    }
                    // Cond opaque: still thread each arm past dispatch blocks.
                    let (tb, ta) = thread(blocks[0])?;
                    let (eb, ea) = thread(blocks[1])?;
                    if tb == blocks[0].block(&func.dfg.value_lists)
                        && eb == blocks[1].block(&func.dfg.value_lists)
                        && ta
                            == call_parts(func, blocks[0])?.1
                        && ea == call_parts(func, blocks[1])?.1
                    {
                        return None;
                    }
                    Some(NewTerm::Brif(A::V(c), tb, ta, eb, ea))
                }
            }
        }
        _ => None,
    }
}

/// What an argument materializes to at the pred's tail.
enum A {
    V(Value),
    /// Rematerialize this `iconst`.
    K(cranelift_codegen::ir::Type, i64),
}

fn prep_args(
    func: &Function,
    dom: &DominatorTree,
    p: Block,
    pterm: Inst,
    args: &[Value],
) -> Option<Vec<A>> {
    let mut out = Vec::with_capacity(args.len());
    for &v in args {
        let v = func.dfg.resolve_aliases(v);
        match func.dfg.value_def(v) {
            ValueDef::Param(b, _)
                if b == p
                    || (func.layout.is_block_inserted(b) && dom.block_dominates(b, p)) =>
            {
                out.push(A::V(v));
            }
            ValueDef::Result(i, _) => {
                let inst = func.dfg.insts[i];
                if func.layout.inst_block(i).is_some()
                    && dom.dominates(i, pterm, &func.layout)
                {
                    out.push(A::V(v));
                } else if let InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm,
                } = inst
                {
                    out.push(A::K(func.dfg.value_type(v), imm.bits()));
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn emit_args(cur: &mut FuncCursor, args: &[A]) -> Vec<BlockArg> {
    args.iter()
        .map(|a| match a {
            A::V(v) => BlockArg::Value(*v),
            A::K(ty, k) => BlockArg::Value(cur.ins().iconst(*ty, *k)),
        })
        .collect()
}

/// Drop unreachable blocks (folds can orphan whole dispatch chains).
pub(crate) fn sweep_blocks(func: &mut Function) {
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    let dead: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&b| !dom.is_reachable(b))
        .collect();
    for b in dead {
        while let Some(i) = func.layout.first_inst(b) {
            func.layout.remove_inst(i);
        }
        func.layout.remove_block(b);
    }
}

/// Remove dead pure insts left by the rewrites (selects/extends/icmps whose
/// results no longer feed anything).
pub(crate) fn sweep_dead(func: &mut Function) {
    let mut uses: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                *uses.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
            }
        }
    }
    loop {
        let insts: Vec<Inst> = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b).collect::<Vec<_>>())
            .collect();
        let mut killed = false;
        for i in insts {
            if func.dfg.insts[i].opcode().is_terminator()
                || !crate::jumpthread::pure_op(func, i)
                || func
                    .dfg
                    .inst_results(i)
                    .iter()
                    .any(|r| uses.get(&func.dfg.resolve_aliases(*r)).copied().unwrap_or(0) > 0)
            {
                continue;
            }
            for v in func.dfg.inst_values(i).collect::<Vec<_>>() {
                if let Some(u) = uses.get_mut(&func.dfg.resolve_aliases(v)) {
                    *u = u.saturating_sub(1);
                }
            }
            func.layout.remove_inst(i);
            killed = true;
        }
        if !killed {
            break;
        }
    }
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    let mut plans = Vec::new();
    for p in func.layout.blocks() {
        if !dom.is_reachable(p) {
            continue;
        }
        let Some(t) = func.layout.last_inst(p) else {
            continue;
        };
        if matches!(
            func.dfg.insts[t],
            InstructionData::Jump { .. } | InstructionData::Brif { .. }
        ) && let Some(nt) = plan(func, &cfg, &dom, p, t)
        {
            plans.push((p, t, nt));
        }
    }
    let mut n = 0;
    let lim = std::env::var("PLIRON_EDGESPEC_LIMIT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok());
    for (p, t, nt) in plans {
        if let Some(l) = lim
            && n >= l
        {
            break;
        }
        if func.layout.last_inst(p) != Some(t) {
            continue;
        }
        match nt {
            NewTerm::Jump(d, a) => {
                let Some(pa) = prep_args(func, &dom, p, t, &a) else {
                    continue;
                };
                func.layout.remove_inst(t);
                let mut cur = FuncCursor::new(func).at_bottom(p);
                let args = emit_args(&mut cur, &pa);
                cur.ins().jump(d, &args);
            }
            NewTerm::Brif(c, tb, ta, eb, ea) => {
                let (Some(pa), Some(pe)) = (
                    prep_args(func, &dom, p, t, &ta),
                    prep_args(func, &dom, p, t, &ea),
                ) else {
                    continue;
                };
                func.layout.remove_inst(t);
                let mut cur = FuncCursor::new(func).at_bottom(p);
                let ta = emit_args(&mut cur, &pa);
                let ea = emit_args(&mut cur, &pe);
                let cv = match c {
                    A::V(v) if cur.func.dfg.value_type(v) == I8 => v,
                    A::V(v) => {
                        let cty = cur.func.dfg.value_type(v);
                        let z = cur.ins().iconst(cty, 0);
                        cur.ins().icmp(IntCC::NotEqual, v, z)
                    }
                    A::K(ty, k) => cur.ins().iconst(ty, k),
                };
                cur.ins().brif(cv, tb, &ta, eb, &ea);
            }
        }
        n += 1;
    }
    if n > 0 {
        sweep_blocks(func);
        sweep_dead(func);
    }
    n
}

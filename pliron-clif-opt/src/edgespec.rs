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
    Block, BlockArg, BlockCall, ExceptionTableItem, Function, Inst, InstBuilder, InstructionData,
    Opcode, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// Bound on pure-op chains evaluated per value, and on dispatch blocks
/// threaded per edge.
const MAX_EVAL: usize = 16;
const MAX_HOPS: usize = 8;

/// A value's worth under the supplied-arg environment: a constant,
/// `S(c, t, e)` = `t` when `c != 0` else `e` for known `t`/`e`, or
/// `B(m, v)` — every bit covered by `m` equals the matching bit of `v`
/// (a partially-known value, e.g. an enum tag byte a `band`/`bor` merge
/// forces while the payload stays unknown).
#[derive(Clone, Copy, PartialEq)]
enum E {
    Unk,
    K(u64),
    S(Value, u64, u64),
    B(u64, u64),
}

/// Normalize: a `B` whose mask covers all `w` bits is a `K`.
fn ebit(w: u32, m: u64, v: u64) -> E {
    let wm = mask(u64::MAX, w);
    let (m, v) = (m & wm, v & wm);
    if m == wm { E::K(v) } else { E::B(m, v) }
}

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
    let i = func.dfg.resolved_value_def(v).inst()?;
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
    let Some(i) = func.dfg.resolved_value_def(v).inst() else {
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
            E::B(m, v) => ebit(w, m, v),
            E::Unk => E::Unk,
        },
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => {
            let sw = bits(func, arg).unwrap_or(w);
            match eval(func, env, sub, arg, depth + 1) {
                E::B(m, v) => {
                    let sm = mask(u64::MAX, sw);
                    ebit(w, m | !sm, v & sm)
                }
                e => e,
            }
        }
        InstructionData::Unary {
            opcode: Opcode::Sextend,
            arg,
        } => {
            let sw = bits(func, arg).unwrap_or(64);
            match eval(func, env, sub, arg, depth + 1) {
                E::K(k) => E::K(mask(sext(k, sw) as u64, w)),
                E::S(c, t, e) => E::S(c, mask(sext(t, sw) as u64, w), mask(sext(e, sw) as u64, w)),
                E::B(m, v) => {
                    let sbit = 1u64 << (sw - 1);
                    let (m, v) = (m & mask(u64::MAX, sw), mask(sext(v, sw) as u64, w));
                    if m & sbit != 0 {
                        ebit(w, m | !mask(u64::MAX, sw), v)
                    } else {
                        ebit(w, m & !sbit, v)
                    }
                }
                E::Unk => E::Unk,
            }
        }
        InstructionData::Unary {
            opcode: Opcode::Bnot,
            arg,
        } => match eval(func, env, sub, arg, depth + 1) {
            E::K(k) => E::K(mask(!k, w)),
            E::S(c, t, e) => E::S(c, mask(!t, w), mask(!e, w)),
            E::B(m, v) => ebit(w, m, !v),
            E::Unk => E::Unk,
        },
        InstructionData::Binary {
            opcode: op @ (Opcode::Band | Opcode::Bor | Opcode::Bxor),
            args,
        } => {
            let opc = op;
            let op = |a: u64, b: u64| -> u64 {
                match opc {
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
                // A const (or partially-known side) fixes bits the other
                // side can't unset — this is how `or`-merged enum tags
                // survive as known bits while the payload stays unknown.
                _ => {
                    let wm = mask(u64::MAX, w);
                    let tob = |e: E| -> Option<(u64, u64)> {
                        match e {
                            E::K(k) => Some((wm, k)),
                            E::B(m, v) => Some((m, v)),
                            _ => None,
                        }
                    };
                    match (
                        opc,
                        tob(eval(func, env, sub, args[0], depth + 1)),
                        tob(eval(func, env, sub, args[1], depth + 1)),
                    ) {
                        (Opcode::Band, Some((m1, v1)), Some((m2, v2))) => {
                            ebit(w, (m1 & m2) | (m1 & !v1) | (m2 & !v2), v1 & v2)
                        }
                        (Opcode::Band, None, Some((m, v))) | (Opcode::Band, Some((m, v)), None) => {
                            ebit(w, m & !v, 0)
                        }
                        (Opcode::Bor, Some((m1, v1)), Some((m2, v2))) => {
                            ebit(w, (m1 & m2) | (m1 & v1) | (m2 & v2), v1 | v2)
                        }
                        (Opcode::Bor, None, Some((m, v))) | (Opcode::Bor, Some((m, v)), None) => {
                            ebit(w, m & v, m & v)
                        }
                        (Opcode::Bxor, Some((m1, v1)), Some((m2, v2))) => ebit(w, m1 & m2, v1 ^ v2),
                        _ => E::Unk,
                    }
                }
            }
        }
        InstructionData::Binary {
            opcode: op @ (Opcode::Ushr | Opcode::Ishl),
            args,
        } => {
            // Shifts by a known amount shift the known mask too, and expose
            // freshly-known bits at the vacated edge.
            let (ea, eb) = (
                eval(func, env, sub, args[0], depth + 1),
                eval(func, env, sub, args[1], depth + 1),
            );
            let E::K(k) = eb else { return E::Unk };
            if k >= w as u64 {
                return E::Unk;
            }
            let (k, wm) = (k as u32, mask(u64::MAX, w));
            let (m, v) = match ea {
                E::K(vv) => (wm, vv),
                E::B(m, v) => (m, v),
                _ => (0, 0),
            };
            match op {
                Opcode::Ushr => {
                    let hi = if k == 0 { 0 } else { wm & (wm << (w - k)) };
                    ebit(w, (m >> k) | hi, v >> k)
                }
                _ => ebit(w, (m << k) | mask(u64::MAX, k), v << k),
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
                // Differing known bits settle eq/ne even when the rest is
                // unknown.
                (E::B(m, v), E::K(k)) | (E::K(k), E::B(m, v)) => {
                    if (v & m) != (mask(k, ow) & m) {
                        match cond {
                            IntCC::Equal => E::K(0),
                            IntCC::NotEqual => E::K(1),
                            _ => E::Unk,
                        }
                    } else {
                        E::Unk
                    }
                }
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
                                E::B(..) => E::Unk,
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

/// The forward cone reachable from `d` (≤64 blocks) plus a local
/// dominator bitset over it: bit `j` in `cdom[k]` means `order[j]` lies
/// on every `d → order[k]` path inside the cone. CFG-only — reusable
/// across every landing attempt on `d` for the whole `plans` phase.
struct Cone {
    order: Vec<Block>,
    midx: FxHashMap<Block, u32>,
    cdom: Vec<u64>,
    /// Upward-exposed uses: (index of the using block in `order`, def).
    /// Alias/def resolution is plan-invariant, so each use is decoded once
    /// per cone rather than once per landing attempt.
    uses: Vec<ConeUse>,
}

struct ConeUse {
    /// `order` index of the block containing the use.
    k: u32,
    def: ConeDef,
}

enum ConeDef {
    Param(Block),
    /// Result def: (def inst, def block).
    Res(Inst, Block),
    /// Use that always fails the legality scan (dead def block).
    Bad,
}

struct W<'a> {
    func: &'a Function,
    cfg: &'a ControlFlowGraph,
    dom: &'a DominatorTree,
    p: Block,
    pterm: Inst,
    /// Blocks whose non-terminator insts are all side-effect-free —
    /// computed once per `run`; `resolve` queried this per hop per edge.
    pure_blocks: &'a FxHashSet<Block>,
    /// `cone` results are CFG-invariant during planning; landing checks
    /// hit the same `d` from many preds.
    cones: &'a std::cell::RefCell<FxHashMap<Block, Cone>>,
}

impl<'a> W<'a> {
    /// Every non-terminator inst in `b` is side-effect-free.
    fn pure(&self, b: Block) -> bool {
        self.pure_blocks.contains(&b)
    }

    /// Substitute walked-block params in a call's args with pred values.
    fn map_call(
        &self,
        bc: BlockCall,
        sub: &FxHashMap<Value, Value>,
    ) -> Option<(Block, Vec<Value>)> {
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
                        && self.dom.try_dominates(i, self.pterm, &self.func.layout)
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
    fn cone(&self, d: Block) -> std::cell::Ref<'_, Cone> {
        {
            let mut m = self.cones.borrow_mut();
            if !m.contains_key(&d) {
                m.insert(d, self.compute_cone(d));
            }
        }
        std::cell::Ref::map(self.cones.borrow(), |m| &m[&d])
    }

    fn compute_cone(&self, d: Block) -> Cone {
        const MAX_CONE: usize = 64;
        let mut order = vec![d];
        let mut seen: FxHashSet<Block> = [d].into_iter().collect();
        let mut i = 0;
        let mut over = false;
        while i < order.len() {
            if order.len() > MAX_CONE {
                over = true;
                break;
            }
            let b = order[i];
            i += 1;
            let Some(t) = self.func.layout.last_inst(b) else {
                continue;
            };
            for bc in self.func.dfg.insts[t]
                .branch_destination(&self.func.dfg.jump_tables, &self.func.dfg.exception_tables)
            {
                let s = bc.block(&self.func.dfg.value_lists);
                if seen.insert(s) {
                    order.push(s);
                }
            }
        }
        if over {
            // Marker cone: the `order` cap is part of `cone_ok`'s
            // conservative refusal, so represent "too big" with an empty
            // cone and let `cone_ok` reject it up front.
            return Cone {
                order: Vec::new(),
                midx: FxHashMap::default(),
                cdom: Vec::new(),
                uses: Vec::new(),
            };
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
        // Decode each upward-exposed use once: alias/def resolution is
        // stable for the whole plans phase.
        let mut uses = Vec::new();
        for (k, &u) in order.iter().enumerate() {
            for inst in self.func.layout.block_insts(u) {
                for v in self.func.dfg.inst_values(inst) {
                    let v = self.func.dfg.resolve_aliases(v);
                    let def = match self.func.dfg.value_def(v) {
                        ValueDef::Param(b, _) if b == u => continue,
                        ValueDef::Param(b, _) => ConeDef::Param(b),
                        ValueDef::Result(di, _) => match self.func.layout.inst_block(di) {
                            Some(b) if b == u => continue,
                            None => ConeDef::Bad,
                            Some(b) => ConeDef::Res(di, b),
                        },
                        _ => ConeDef::Bad,
                    };
                    uses.push(ConeUse { k: k as u32, def });
                }
            }
        }
        Cone {
            order,
            midx,
            cdom,
            uses,
        }
    }

    fn cone_ok(&self, d: Block, skipped: &FxHashSet<Block>) -> bool {
        if d == self.p && skipped.is_empty() {
            return true;
        }
        let cone = self.cone(d);
        if cone.order.is_empty() {
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::edgespec", tracing::Level::DEBUG)
            {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::edgespec", "edgespec cone_ok: {:?} cone>64 from {:?}", self.p, d)
                };
            }
            return false;
        }
        for cu in &cone.uses {
            let db = match cu.def {
                ConeDef::Param(b) => b,
                ConeDef::Res(di, b) => {
                    if !skipped.contains(&b)
                        && self.dom.try_dominates(di, self.pterm, &self.func.layout)
                    {
                        continue;
                    }
                    b
                }
                ConeDef::Bad => return false,
            };
            // A def-block that cone-dominates `u` stays on every new
            // path to `u`; a param/block dominating `p` stays above
            // the new edge — unless it is skipped: the walked edge
            // would have re-bound it, so the use would see stale.
            if cone
                .midx
                .get(&db)
                .is_some_and(|&j| cone.cdom[cu.k as usize] & (1 << j) != 0)
                || (!skipped.contains(&db)
                    && self.func.layout.is_block_inserted(db)
                    && self.dom.block_dominates(db, self.p))
            {
                continue;
            }
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::edgespec", tracing::Level::DEBUG)
            {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::edgespec", "edgespec cone_ok: {:?} use (def blk {:?}) at {:?} fails",
                        self.p, db, cone.order[cu.k as usize]
                    )
                };
            }
            return false;
        }
        true
    }

    /// Settle on `d`: it stays on this edge, so the skipped-prefix defs must
    /// not be consumed anywhere downstream (`cone_ok`) and the carried args
    /// must not themselves be skipped defs that a re-walked binding would
    /// have refreshed.
    fn fin_edge(&self, d: Block, args: Vec<Value>, skipped: &FxHashSet<Block>) -> Option<Edge> {
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
        skipped: &mut FxHashSet<Block>,
    ) -> Option<Edge> {
        if depth >= MAX_HOPS || !self.pure(d) {
            return self.fin_edge(d, args, skipped);
        }
        if d == self.p {
            return self.fin_edge(d, args, skipped);
        }
        let params = self.func.dfg.block_params(d).to_vec();
        if params.len() != args.len() {
            return self.fin_edge(d, args, skipped);
        }
        // Re-entering a walked block, or passing `d` one of its own params
        // (a loop header fed from inside the loop), would rebind a value
        // the args still mean in its old sense: `env` and `sub` are keyed
        // by `Value`, so the old and new bindings can't coexist.
        if skipped.contains(&d)
            || args
                .iter()
                .any(|&a| params.contains(&subv(self.func, &sub, a)) || params.contains(&a))
        {
            return self.fin_edge(d, args, skipped);
        }
        // `env`/`sub` arrive by value; this path owns them outright.
        let mut env = env;
        let mut sub = sub;
        // Block params bind in parallel: evaluate every arg before binding.
        let es: Vec<E> = args
            .iter()
            .map(|&a| {
                let e = eval(self.func, &mut env, &sub, a, 0);
                if e == E::Unk && is_bool(self.func, a, 0) {
                    E::S(a, 1, 0)
                } else {
                    e
                }
            })
            .collect();
        for ((&prm, &a), e) in params.iter().zip(&args).zip(es) {
            env.insert(prm, e);
            sub.insert(prm, a);
        }
        let term = self.func.layout.last_inst(d)?;
        // `d` is consumed only when we follow its terminator through to a
        // successor; landing on `d` keeps its defs live. `skipped` carries
        // the consumed set while the deeper walk runs and loses `d` again
        // before any landing-site check (`fin_edge`) sees it.
        /// Consume `d`'s terminator and continue to `nb`; if the deeper walk
        /// cannot be made legal, land on `d` instead — still threading the
        /// prefix between `p` and `d`.
        macro_rules! go {
            ($env:expr, $sub:expr, $nb:expr, $nargs:expr) => {{
                skipped.insert(d);
                let r = self.resolve($env, $sub, $nb, $nargs, depth + 1, split_ok, skipped);
                skipped.remove(&d);
                r.or_else(|| self.fin_edge(d, args.clone(), skipped))
            }};
        }
        match self.func.dfg.insts[term] {
            InstructionData::Jump { destination, .. } => {
                let (nb, nargs) = self.map_call(destination, &sub)?;
                go!(env, sub, nb, nargs)
            }
            InstructionData::Brif { arg: c, blocks, .. } => {
                match eval(self.func, &mut env, &sub, c, 0) {
                    E::Unk | E::B(..) => self.fin_edge(d, args, skipped),
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
                            let (nb, nargs) = self.map_call(blocks[usize::from(tv == 0)], &sub)?;
                            return go!(env, sub, nb, nargs);
                        };
                        if !split_ok {
                            return self.fin_edge(d, args, skipped);
                        }
                        skipped.insert(d);
                        let Some(cv) = self.avail_a(cv, &sub, skipped) else {
                            skipped.remove(&d);
                            return self.fin_edge(d, args, skipped);
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
                                skipped,
                            )
                            .or_else(|| self.fin_edge(tb, ta, skipped));
                        let te = match te {
                            Some(te) => te,
                            None => {
                                skipped.remove(&d);
                                return None;
                            }
                        };
                        let ee = self
                            .resolve(env, sub, eb, ea.clone(), depth + 1, false, skipped)
                            .or_else(|| self.fin_edge(eb, ea, skipped));
                        skipped.remove(&d);
                        Some(Edge::Br(cv, Box::new(te), Box::new(ee?)))
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
                    E::Unk => self.fin_edge(d, args, skipped),
                    E::K(k) => {
                        let (nb, nargs) = self.map_call(pick(k), &sub)?;
                        go!(env, sub, nb, nargs)
                    }
                    E::S(cv, tv, ev) if split_ok && tv != ev => {
                        skipped.insert(d);
                        let Some(cv) = self.avail_a(cv, &sub, skipped) else {
                            skipped.remove(&d);
                            return self.fin_edge(d, args, skipped);
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
                                skipped,
                            )
                            .or_else(|| self.fin_edge(tb, ta, skipped));
                        let te = match te {
                            Some(te) => te,
                            None => {
                                skipped.remove(&d);
                                return None;
                            }
                        };
                        let ee = self
                            .resolve(env, sub, eb, ea.clone(), depth + 1, false, skipped)
                            .or_else(|| self.fin_edge(eb, ea, skipped));
                        skipped.remove(&d);
                        Some(Edge::Br(cv, Box::new(te), Box::new(ee?)))
                    }
                    _ => self.fin_edge(d, args, skipped),
                }
            }
            _ => self.fin_edge(d, args, skipped),
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
    pure_blocks: &FxHashSet<Block>,
    cones: &std::cell::RefCell<FxHashMap<Block, Cone>>,
    p: Block,
    pterm: Inst,
) -> Option<NewTerm> {
    let w = W {
        func,
        cfg,
        dom,
        p,
        pterm,
        pure_blocks,
        cones,
    };
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
                &mut FxHashSet::default(),
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
                        &mut FxHashSet::default(),
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
                            &mut FxHashSet::default(),
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
                        && ta == call_parts(func, blocks[0])?.1
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
                if b == p || (func.layout.is_block_inserted(b) && dom.block_dominates(b, p)) =>
            {
                out.push(A::V(v));
            }
            ValueDef::Result(i, _) => {
                let inst = func.dfg.insts[i];
                if func.layout.inst_block(i).is_some() && dom.try_dominates(i, pterm, &func.layout) {
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
pub fn sweep_blocks(func: &mut Function) {
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
pub fn sweep_dead(func: &mut Function) {
    crate::uses::worklist_dce(func, |f, i| {
        !f.dfg.insts[i].opcode().is_terminator() && crate::jumpthread::pure_op(f, i)
    });
}

pub fn run(func: &mut Function) -> usize {
    let prof = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::edgespec", tracing::Level::INFO);
    let t0 = std::time::Instant::now();
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    // All-pure blocks — plans are collected before any apply, so this
    // stays valid for every `plan` below.
    let mut pure_blocks: FxHashSet<Block> = FxHashSet::default();
    for b in func.layout.blocks() {
        let mut it = func.layout.block_insts(b);
        it.next_back(); // terminator exempt from the purity check
        if it.all(|i| crate::jumpthread::pure_op(func, i)) {
            pure_blocks.insert(b);
        }
    }
    if prof {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::edgespec", "  edgespec setup {:?}", t0.elapsed())
        };
    }
    let tp = std::time::Instant::now();
    // Cones are CFG-invariant for the whole plans phase (no mutation until
    // apply); many preds land on the same dispatch blocks, so memoize.
    let cones = std::cell::RefCell::new(FxHashMap::default());
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
        ) && let Some(nt) = plan(func, &cfg, &dom, &pure_blocks, &cones, p, t)
        {
            plans.push((p, t, nt));
        }
    }
    if prof {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::edgespec", "  edgespec plans({}) {:?}", plans.len(), tp.elapsed())
        };
    }
    let tp = std::time::Instant::now();
    let mut n = 0;
    let lim = crate::env_get("PLIRON_EDGESPEC_LIMIT").and_then(|s| s.parse::<usize>().ok());
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
    if prof {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::edgespec", "  edgespec apply({n}) {:?}", tp.elapsed())
        };
    }
    let tp = std::time::Instant::now();
    if n > 0 {
        sweep_blocks(func);
        sweep_dead(func);
    }
    n += splices(func);
    if n > 0 {
        sweep_blocks(func);
        sweep_dead(func);
    }
    if prof {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::edgespec", "  edgespec splices(n={n}) {:?}", tp.elapsed())
        };
    }
    let tp = std::time::Instant::now();
    let d = deadbits(func);
    if d > 0 {
        crate::jumpthread::remove_dead_params(
            func,
            &cranelift_codegen::flowgraph::ControlFlowGraph::with_function(func),
        );
        sweep_dead(func);
        sweep_blocks(func);
        n += d;
    }
    if prof {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::edgespec", "  edgespec deadbits+tail(n={n}) {:?}", tp.elapsed())
        };
    }
    n
}

/// Bits of `v` some sink actually reads, propagated backward to a fixpoint.
/// Enum packing threads a tag+payload word through loop phis whose tag byte
/// is the only part ever consumed — once the dispatch join dies, the packed
/// word's bits circulate through `band`/`bor` merges forever without ever
/// being read. When a block param's needed-bit set is empty, its value is
/// unconstrained: substitute `iconst 0` at every use, which folds the whole
/// `band`/`bor` merge chain feeding it to dead code.
fn deadbits(func: &mut Function) -> usize {
    // Operand-bit demand for a value instruction given the need on its
    // result(s). `kth` = the operand's constant value when it is an iconst.
    fn demand(func: &Function, i: Inst, out_need: u64) -> Vec<u64> {
        let wm_of = |func: &Function, v: Value| mask(u64::MAX, bits(func, v).unwrap_or(64));
        match func.dfg.insts[i] {
            InstructionData::Binary {
                opcode:
                    op @ (Opcode::Band | Opcode::Bor | Opcode::Bxor | Opcode::Iadd | Opcode::Isub),
                args,
            } => {
                let mut d = vec![out_need; 2];
                for (k, &a) in args.iter().enumerate() {
                    let other = iconst(func, args[1 - k]);
                    d[k] = match (op, other) {
                        (Opcode::Band, Some(c)) => out_need & c,
                        (Opcode::Band, None) => out_need,
                        (Opcode::Bor, Some(c)) => out_need & !c,
                        (Opcode::Bor, None) => out_need,
                        (Opcode::Bxor, _) => out_need,
                        // Carry: bit j can depend on operand bits 0..=j.
                        _ if out_need != 0 => out_need | (out_need - 1),
                        _ => 0,
                    };
                    d[k] &= wm_of(func, a);
                }
                d
            }
            InstructionData::Binary {
                opcode: op @ (Opcode::Ushr | Opcode::Ishl | Opcode::Sshr),
                args,
            } => {
                let w = bits(func, args[0]).unwrap_or(64);
                let wm = mask(u64::MAX, w);
                let l = match (op, iconst(func, args[1])) {
                    (Opcode::Ushr, Some(k)) if k < w as u64 => (out_need << k as u32) & wm,
                    (Opcode::Ishl, Some(k)) if k < w as u64 => (out_need >> k as u32) & wm,
                    (Opcode::Sshr, Some(k)) if k < w as u64 => {
                        let k = k as u32;
                        let mut m = (out_need << k) & wm;
                        if k > 0 && (out_need >> (w - k)) != 0 {
                            m |= 1u64 << (w - 1);
                        }
                        m
                    }
                    _ => wm,
                };
                vec![l, if out_need != 0 { wm } else { 0 }]
            }
            InstructionData::BinaryImm8 {
                opcode: op,
                arg,
                imm,
            } => {
                let w = bits(func, arg).unwrap_or(64);
                let wm = mask(u64::MAX, w);
                let k = u64::from(imm);
                let l = match op {
                    Opcode::Ushr if k < w as u64 => (out_need << k as u32) & wm,
                    Opcode::Ishl if k < w as u64 => (out_need >> k as u32) & wm,
                    Opcode::Sshr if k < w as u64 => {
                        let k = k as u32;
                        let mut m = (out_need << k) & wm;
                        if k > 0 && (out_need >> (w - k)) != 0 {
                            m |= 1u64 << (w - 1);
                        }
                        m
                    }
                    _ => wm,
                };
                vec![l]
            }
            InstructionData::Unary { opcode, arg } => {
                let w = bits(func, arg).unwrap_or(64);
                let m = match opcode {
                    Opcode::Ireduce => out_need,
                    Opcode::Uextend => out_need & mask(u64::MAX, w),
                    Opcode::Sextend => {
                        (out_need & mask(u64::MAX, w))
                            | if (out_need >> (w - 1)) != 0 {
                                1u64 << (w - 1)
                            } else {
                                0
                            }
                    }
                    Opcode::Bnot => out_need,
                    _ => mask(u64::MAX, w),
                };
                vec![m & mask(u64::MAX, w)]
            }
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } => vec![wm_of(func, args[0]), out_need, out_need],
            _ => func
                .dfg
                .inst_args(i)
                .iter()
                .map(|&a| wm_of(func, a))
                .collect(),
        }
    }

    let mut need: FxHashMap<Value, u64> = FxHashMap::default();
    // param -> resolved edge args feeding it: one terminator walk instead
    // of every block's calls being re-scanned per fixpoint round.
    let mut inargs: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for bc in
            func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            let d = bc.block(&func.dfg.value_lists);
            for (k, a) in bc.args(&func.dfg.value_lists).enumerate() {
                let Some(&p) = func.dfg.block_params(d).get(k) else {
                    break;
                };
                let BlockArg::Value(av) = a else { continue };
                inargs
                    .entry(func.dfg.resolve_aliases(p))
                    .or_default()
                    .push(func.dfg.resolve_aliases(av));
            }
        }
    }
    // Worklist fixpoint: `need` only grows, so propagate a grown value's
    // demand once — a result wakes its def inst (operand demand), a param
    // wakes the edge args that feed it. No per-round function rescans.
    enum Work {
        Inst(Inst),
        Param(Value),
    }
    let mut work: Vec<Work> = Vec::new();
    let grow = |need: &mut FxHashMap<Value, u64>,
                work: &mut Vec<Work>,
                func: &Function,
                v: Value,
                d: u64| {
        if d & !need.get(&v).copied().unwrap_or(0) == 0 {
            return;
        }
        *need.entry(v).or_default() |= d;
        match func.dfg.value_def(v) {
            ValueDef::Result(i, _) => work.push(Work::Inst(i)),
            ValueDef::Param(..) => work.push(Work::Param(v)),
            _ => {}
        }
    };
    for i in func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .collect::<Vec<_>>()
    {
        work.push(Work::Inst(i));
    }
    let mut seen_inst: FxHashSet<Inst> = FxHashSet::default();
    while let Some(w) = work.pop() {
        match w {
            Work::Inst(i) => {
                // One evaluation per enqueue; seeds dedupe via the set.
                if func.layout.inst_block(i).is_none() {
                    continue;
                }
                let out_need = func.dfg.inst_results(i).iter().fold(0u64, |m, r| {
                    m | need
                        .get(&func.dfg.resolve_aliases(*r))
                        .copied()
                        .unwrap_or(0)
                });
                if !seen_inst.insert(i) && out_need == 0 {
                    continue;
                }
                let ds = demand(func, i, out_need);
                let av: Vec<Value> = func.dfg.inst_args(i).to_vec();
                for (k, &a) in av.iter().enumerate() {
                    let d = ds
                        .get(k)
                        .copied()
                        .unwrap_or_else(|| mask(u64::MAX, bits(func, a).unwrap_or(64)));
                    let a = func.dfg.resolve_aliases(a);
                    grow(&mut need, &mut work, func, a, d);
                }
                // Exception contexts are opaque reads: demand all bits.
                if let Some(et) = func.dfg.insts[i].exception_table() {
                    for item in func.dfg.exception_tables[et].items().collect::<Vec<_>>() {
                        let ExceptionTableItem::Context(ctx) = item else {
                            continue;
                        };
                        let v = func.dfg.resolve_aliases(ctx);
                        let d = mask(u64::MAX, bits(func, v).unwrap_or(64));
                        grow(&mut need, &mut work, func, v, d);
                    }
                }
            }
            Work::Param(p) => {
                let pn = need.get(&p).copied().unwrap_or(0);
                for &av in inargs.get(&p).into_iter().flatten() {
                    let d = pn & mask(u64::MAX, bits(func, av).unwrap_or(64));
                    grow(&mut need, &mut work, func, av, d);
                }
            }
        }
    }
    // Params with empty need are unconstrained: replace their uses with a
    // zero constant; the merge chains feeding them die with them.
    let mut deadp: Vec<Value> = Vec::new();
    for b in func.layout.blocks() {
        for &p in func.dfg.block_params(b) {
            let p = func.dfg.resolve_aliases(p);
            if bits(func, p).is_some() && need.get(&p).copied().unwrap_or(0) == 0 {
                deadp.push(p);
            }
        }
    }
    if deadp.is_empty() {
        return 0;
    }
    // Every raw value naming a dead param (aliases resolve to it) gets the
    // same zero substitute.
    let deadset: FxHashSet<Value> = deadp.iter().copied().collect();
    let mut raw: Vec<Value> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                if deadset.contains(&func.dfg.resolve_aliases(v)) {
                    raw.push(v);
                }
            }
        }
    }
    let entry = func.layout.entry_block().unwrap();
    let mut repl: FxHashMap<Value, Value> = FxHashMap::default();
    for &v in &raw {
        if repl.contains_key(&v) {
            continue;
        }
        let ty = func.dfg.value_type(v);
        let mut cur = FuncCursor::new(func).at_first_insertion_point(entry);
        repl.insert(v, cur.ins().iconst(ty, 0));
    }
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            func.dfg
                .map_inst_values(i, |v| repl.get(&v).copied().unwrap_or(v));
        }
    }
    deadp.len()
}

/// Thread `p -> J` edges past a dispatch join `J` whose `brif` the supplied
/// args decide — even when `J`'s params are used upward-exposed in the
/// picked successor. The ordinary walk lands on `J` in that case because
/// skipping it would orphan the param; here the param is spliced onto the
/// successor's signature instead (every pred of the successor is `J`, so its
/// edge supplies the value), turning `p`'s jump into a direct edge that
/// carries the arg it already computed.
///
/// This is the enum/Result shape rustc's packing produces: exit blocks merge
/// tag+payload bits through `band`/`bor` into a join that re-tests the tag
/// bit and forwards the packed word onward. Each pred's tag is already
/// constant (the merge only ever sets known bits), so the join and its test
/// are pure overhead.
fn splices(func: &mut Function) -> usize {
    // The walker's redirects mutated the CFG since `run`'s domtree was
    // built — recompute it so dominance answers match the current graph.
    let cfg = ControlFlowGraph::with_function(func);
    let dom = DominatorTree::with_function(func, &cfg);
    // Function-wide indexes, O(fn) each: value → blocks containing a use,
    // and block → its predecessor blocks. Built lazily past the cheap
    // rejections (they used to be rebuilt per candidate — O(candidates ×
    // fn)) and invalidated only when a splice actually mutates the
    // function. Within one candidate's apply, splices to other
    // successors cannot change the queries these answer here (which
    // J-params `s` uses; whether only J feeds `s`), so a single
    // post-apply invalidation is enough — and a splice doesn't remove
    // preds from cfg/j's path or add dominators, so the stale `dom` is
    // conservative in the safe direction.
    let mut uses: Option<FxHashMap<Value, FxHashSet<Block>>> = None;
    let mut preds: Option<FxHashMap<Block, FxHashSet<Block>>> = None;
    let mut n = 0;
    for j in func.layout.blocks().collect::<Vec<_>>() {
        if !dom.is_reachable(j) {
            continue;
        }
        let params = func.dfg.block_params(j).to_vec();
        if params.is_empty() {
            continue;
        }
        let Some(t) = func.layout.last_inst(j) else {
            continue;
        };
        let InstructionData::Brif { arg: c, blocks, .. } = func.dfg.insts[t] else {
            continue;
        };
        // J must be safe to skip: pure body, and none of its inst results may
        // be used anywhere outside J (they cannot be rematerialized at the
        // pred without re-executing J).
        if !func
            .layout
            .block_insts(j)
            .take_while(|&i| i != t)
            .all(|i| crate::jumpthread::pure_op(func, i))
        {
            continue;
        }
        let rpjs: Vec<Value> = params
            .iter()
            .map(|&p| func.dfg.resolve_aliases(p))
            .collect();
        let jdefs: Vec<Value> = params
            .iter()
            .map(|&p| func.dfg.resolve_aliases(p))
            .chain(func.layout.block_insts(j).flat_map(|i| {
                func.dfg
                    .inst_results(i)
                    .iter()
                    .map(|r| func.dfg.resolve_aliases(*r))
                    .collect::<Vec<_>>()
            }))
            .collect();
        // Edges into J and which successor each one decides (if any). Taken
        // from J's cfg preds — the function-wide edge list and use-map are
        // O(function) and only needed past this point, so they build lazily
        // below for the few candidates that get here.
        let mut decided: Vec<(Block, Inst, usize, Vec<BlockArg>, Block, Vec<BlockArg>)> =
            Vec::new();
        for bpred in cfg.pred_iter(j) {
            let (p, pt) = (bpred.block, bpred.inst);
            if p == j {
                continue;
            }
            for (di, bc) in func.dfg.insts[pt]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .into_iter()
                .enumerate()
            {
                if bc.block(&func.dfg.value_lists) != j {
                    continue;
                }
                let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                if args.len() != params.len() {
                    continue;
                }
                let mut env: FxHashMap<Value, E> = FxHashMap::default();
                let sub: FxHashMap<Value, Value> = FxHashMap::default();
                // Evaluate every arg before binding any param: an edge from
                // inside a loop may pass J's own params, meaning their old
                // values.
                let es: Option<Vec<E>> = args
                    .iter()
                    .map(|a| match a {
                        BlockArg::Value(a) => Some(eval(func, &mut env, &sub, *a, 0)),
                        _ => None,
                    })
                    .collect();
                let Some(es) = es else {
                    continue;
                };
                for (&rpj, e) in rpjs.iter().zip(es) {
                    env.insert(rpj, e);
                }
                let E::K(k) = eval(func, &mut env, &sub, c, 0) else {
                    continue;
                };
                let bc = blocks[usize::from(k == 0)];
                decided.push((
                    p,
                    pt,
                    di,
                    args,
                    bc.block(&func.dfg.value_lists),
                    bc.args(&func.dfg.value_lists).collect(),
                ));
            }
        }
        if decided.is_empty() {
            continue;
        }
        // Uses of values outside their def block: v -> blocks containing a
        // use.
        if uses.is_none() {
            let mut u: FxHashMap<Value, FxHashSet<Block>> = FxHashMap::default();
            for b in func.layout.blocks() {
                for i in func.layout.block_insts(b) {
                    for v in func.dfg.inst_values(i) {
                        u.entry(func.dfg.resolve_aliases(v)).or_default().insert(b);
                    }
                }
            }
            uses = Some(u);
        }
        if preds.is_none() {
            let mut m: FxHashMap<Block, FxHashSet<Block>> = FxHashMap::default();
            for p in func.layout.blocks() {
                let Some(t) = func.layout.last_inst(p) else {
                    continue;
                };
                for bc in func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    m.entry(bc.block(&func.dfg.value_lists))
                        .or_default()
                        .insert(p);
                }
            }
            preds = Some(m);
        }
        let uses_map = uses.as_ref().unwrap();
        let preds_map = preds.as_ref().unwrap();
        // Successor set decided edges need.
        let sset: FxHashSet<Block> = decided.iter().map(|d| d.4).collect();
        let mut ok = !sset.contains(&j);
        // Feasibility per successor: only J may feed it (its signature is
        // about to grow); J's inst results may not escape J at all; J's
        // params may only escape into the spliced successors.
        for &s in &sset {
            if !ok {
                break;
            }
            ok &= preds_map
                .get(&s)
                .is_none_or(|ps| ps.iter().all(|&p| p == j));
        }
        if ok {
            'defs: for (di, &d) in jdefs.iter().enumerate() {
                let is_param = di < params.len();
                for &u in uses_map.get(&d).into_iter().flatten() {
                    if u == j || (is_param && sset.contains(&u)) {
                        continue;
                    }
                    ok = false;
                    break 'defs;
                }
            }
        }
        if !ok {
            continue;
        }
        // Splice J's params onto each needed successor: append one param per
        // used J-param, remap the upward-exposed uses, grow J's calls to it.
        let mut mutated = false;
        // Operand remaps applied to each spliced successor. A decided pred
        // that is itself a spliced successor had its J-edge args rewritten by
        // `map_inst_values`, so its captured `jargs` snapshot is stale — the
        // redirect below must replay the same substitution.
        let mut sremaps: FxHashMap<Block, FxHashMap<Value, Value>> = FxHashMap::default();
        for &s in &sset {
            // Raw operand values in s by the value they resolve to — one
            // scan serves every used-param remap below.
            let mut raw_by_res: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
            for i in func.layout.block_insts(s) {
                for v in func.dfg.inst_values(i) {
                    raw_by_res
                        .entry(func.dfg.resolve_aliases(v))
                        .or_default()
                        .push(v);
                }
            }
            let mut added: Vec<usize> = Vec::new();
            let mut remap: FxHashMap<Value, Value> = FxHashMap::default();
            for (k, (&pj, &rpj)) in params.iter().zip(&rpjs).enumerate() {
                if !uses_map.get(&rpj).is_some_and(|u| u.contains(&s)) {
                    continue;
                }
                let np = {
                    let ty = func.dfg.value_type(pj);
                    func.dfg.append_block_param(s, ty)
                };
                if let Some(raws) = raw_by_res.get(&rpj) {
                    for &rv in raws {
                        remap.insert(rv, np);
                    }
                }
                added.push(k);
            }
            if !remap.is_empty() {
                for i in func.layout.block_insts(s).collect::<Vec<_>>() {
                    func.dfg
                        .map_inst_values(i, |v| remap.get(&v).copied().unwrap_or(v));
                }
                sremaps.insert(s, remap);
                mutated = true;
            }
            if !added.is_empty() {
                // J's own calls to s grow by the same params.
                let calls: Vec<(usize, Vec<BlockArg>)> = func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .into_iter()
                    .enumerate()
                    .filter(|(_, bc)| bc.block(&func.dfg.value_lists) == s)
                    .map(|(di, bc)| (di, bc.args(&func.dfg.value_lists).collect()))
                    .collect();
                for (di, mut args) in calls {
                    args.extend(added.iter().map(|&k| BlockArg::Value(params[k])));
                    let dfg = &mut func.dfg;
                    let nbc = BlockCall::new(s, args, &mut dfg.value_lists);
                    dfg.insts[t]
                        .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
                        [di] = nbc;
                }
            }
            // Redirect decided preds: p -> s(sub'd call args, supplied
            // params). Call args that are J-params take the pred's arg;
            // iconsts rematerialize; anything else must already dominate
            // the pred's tail.
            for (p, pt, di, jargs, _, sargs) in decided.iter().filter(|d| d.4 == s) {
                // If p was spliced earlier in this walk its edge args were
                // remapped onto fresh params — replay that substitution on
                // the snapshot instead of emitting the dead raw value.
                let jarg = |a: &BlockArg| -> BlockArg {
                    let &BlockArg::Value(v) = a else { return *a };
                    sremaps
                        .get(p)
                        .and_then(|m| m.get(&v))
                        .copied()
                        .map(BlockArg::Value)
                        .unwrap_or(*a)
                };
                let mut new_args: Vec<BlockArg> = Vec::new();
                let mut ok2 = true;
                for a in sargs {
                    let BlockArg::Value(av) = a else {
                        ok2 = false;
                        break;
                    };
                    let av = func.dfg.resolve_aliases(*av);
                    if let Some(k) = rpjs.iter().position(|&rpj| rpj == av) {
                        new_args.push(jarg(&jargs[k]));
                        continue;
                    }
                    match func.dfg.value_def(av) {
                        ValueDef::Result(i, _)
                            if func.layout.inst_block(i) != Some(j)
                                && dom.try_dominates(i, *pt, &func.layout) =>
                        {
                            new_args.push(BlockArg::Value(av))
                        }
                        ValueDef::Param(b, _) if b != j && dom.block_dominates(b, *p) => {
                            new_args.push(BlockArg::Value(av))
                        }
                        ValueDef::Result(i, _)
                            if let InstructionData::UnaryImm {
                                opcode: Opcode::Iconst,
                                imm,
                            } = func.dfg.insts[i] =>
                        {
                            let ty = func.dfg.value_type(av);
                            let mut cur = FuncCursor::new(func).at_inst(*pt);
                            new_args.push(BlockArg::Value(cur.ins().iconst(ty, imm.bits())));
                        }
                        _ => {
                            ok2 = false;
                            break;
                        }
                    }
                }
                if !ok2 {
                    continue;
                }
                for &k in &added {
                    new_args.push(jarg(&jargs[k]));
                }
                let dfg = &mut func.dfg;
                let nbc = BlockCall::new(s, new_args, &mut dfg.value_lists);
                dfg.insts[*pt]
                    .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[*di] =
                    nbc;
                n += 1;
                mutated = true;
            }
        }
        // An applied splice rewrote edge args, inst operands, and block
        // signatures — the lazily-built indexes are stale for the next
        // candidate.
        if mutated {
            uses = None;
            preds = None;
        }
        // J may now be unreachable; leave it for sweep_blocks.
    }
    n
}

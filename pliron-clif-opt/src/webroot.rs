//! Batched slot-pointer web analysis.
//!
//! The question every slot-touching pass asks per operand — "which stack
//! slot can this value point at, and at what offset?" — is really one
//! question about a whole *web*: a `stack_addr` result threading through
//! aliases, `iadd` constants, `select` sentinel merges and block-param
//! edges (a `HashMap`'s `Option<&mut table>` arrives as a param fed
//! `&slot` on one edge and `0` on the other). Answering per use by
//! chain-walking re-does the same peel repeatedly and stops cold at the
//! first multi-source param; instead we compute, once, the set of
//! alternatives each value can hold:
//!
//! - `slots` — `(slot, off)` the value may address; `ANY` marks "derived
//!   from the slot at an unbounded offset" (`iadd` by a variable).
//! - `nul` — the value may be exactly `0` (the empty-table sentinel).
//! - `bad` — the value may be a nonzero never-dereferenceable constant
//!   (`0 + k`): like `nul` a `notrap` access self-proves it away, but it
//!   can't satisfy `x == 0`, so it doesn't rule an edge out.
//! - `other` — the value may be something else entirely.
//!
//! A value with `slots` empty is provably not slot-derived; a value with
//! *no* flags and no slots is opaque-absent (unreachable or unknown —
//! callers treat it as non-slot).
//!
//! Two layers on top of the raw sets:
//!
//! 1. Param webs are a monotone least fixpoint over the function's edge
//!    args — one iteration pass, not per-query recursion.
//! 2. `Webs::at` refines a query at block `b` by *edge correlation*: the
//!    value a param holds equals the union over its in-edges, but a use
//!    dominated by `brif (x == 0)`'s non-null arm knows the edge taken
//!    delivered a non-`0` arg for `x`'s position — and fixes every
//!    *sibling* param bound by that same edge. So `store p` where
//!    `p ∈ {heap, &slot}` is provably non-slot in the region where the
//!    sibling was tested non-null. That's hashbrown's
//!    `(Option<ptr>, meta, payload)` plumbing.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    Block, Function, InstructionData, Opcode, StackSlot, Value,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::Cell;

/// `slots` offset for "derived from the slot, offset unknown".
pub const ANY: i64 = i64::MIN;

/// Cap on slot members per web — overflow collapses same-slot offsets to
/// `ANY`; distinct slots beyond the cap poison the analysis (never drop
/// a slot member silently — that could hide an escape).
const MAX_SLOTS: usize = 16;
/// Fixpoint iteration bound.
const MAX_ROUNDS: usize = 16;
/// Structural-eval recursion bound.
const MAX_DEPTH: usize = 32;

#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct Web {
    /// Deduped `(slot, off)` members; `off == ANY` = unknown offset.
    pub slots: Vec<(StackSlot, i64)>,
    /// May be exactly `0`.
    pub nul: bool,
    /// May be a nonzero never-dereferenceable constant.
    pub bad: bool,
    /// May be anything unrepresented — a heap pointer, a call result.
    pub other: bool,
}

impl Web {
    fn slot(ss: StackSlot, off: i64) -> Web {
        Web {
            slots: vec![(ss, off)],
            nul: false,
            bad: false,
            other: false,
        }
    }
    fn other() -> Web {
        Web {
            other: true,
            ..Web::default()
        }
    }
    fn nul() -> Web {
        Web {
            nul: true,
            ..Web::default()
        }
    }
    /// Provably `== 0` — the edge-arg fact a `x != 0` establisher rules
    /// out. A `bad` (0+k) member is *not* null: it takes the non-null arm.
    fn provably_null(&self) -> bool {
        self.nul && self.slots.is_empty() && !self.bad && !self.other
    }
    fn join(&mut self, o: &Web, poisoned: &Cell<bool>) {
        for &m in &o.slots {
            if !self.slots.contains(&m) {
                self.slots.push(m);
            }
        }
        if self.slots.len() > MAX_SLOTS {
            // Collapse same-slot members to `ANY`.
            let mut by_slot: Vec<(StackSlot, i64)> = Vec::new();
            for &(s, _) in &self.slots {
                if let Some(e) = by_slot.iter_mut().find(|e| e.0 == s) {
                    e.1 = ANY;
                } else {
                    by_slot.push((s, ANY));
                }
            }
            self.slots = by_slot;
            if self.slots.len() > MAX_SLOTS {
                poisoned.set(true);
            }
        }
        self.nul |= o.nul;
        self.bad |= o.bad;
        self.other |= o.other;
    }
    /// `+ k`/`− k` by a constant.
    fn shifted(&self, k: i64) -> Web {
        // `x ± 0` is `x` — the `0 + k → bad` rule below asserts `k != 0`.
        if k == 0 {
            return self.clone();
        }
        Web {
            slots: self
                .slots
                .iter()
                .map(|&(s, o)| (s, if o == ANY { ANY } else { o.wrapping_add(k) }))
                .collect(),
            // 0 + k is a nonzero garbage address — still
            // never-dereferenceable, but no longer provably `== 0`.
            // A `bad` (0+j) member + k can land on 0 when j == -k, so
            // null stays possible too.
            nul: self.bad,
            bad: self.nul || self.bad,
            other: self.other,
        }
    }
    /// Arithmetic with an unbounded operand: slot members keep their
    /// identity but lose the offset; constants become opaque.
    fn widened(&self) -> Web {
        Web {
            slots: self.slots.iter().map(|&(s, _)| (s, ANY)).collect(),
            nul: false,
            bad: false,
            other: self.other || self.nul || self.bad,
        }
    }
}

/// One in-edge: per-param-position args (`None` for exception-context
/// block args, which carry no `Value`).
struct Edge {
    args: Vec<Option<Value>>,
}

pub struct Webs {
    /// Reachable blocks' in-edges.
    edges: FxHashMap<Block, Vec<Edge>>,
    /// param → (its block, param index).
    pslot: FxHashMap<Value, (Block, usize)>,
    /// Param fixpoint result — unrestricted web.
    pw: FxHashMap<Value, Web>,
    /// Normalized subject → blocks whose only in-edge proved
    /// `subject != 0` (single-pred non-null targets of `icmp` guards).
    est: FxHashMap<Value, Vec<Block>>,
    dt: DominatorTree,
    /// Structural-eval memo (unrestricted eval).
    memo: std::cell::RefCell<FxHashMap<Value, Web>>,
    /// `at` memo — block-refined results repeat across inst operands.
    atmemo: std::cell::RefCell<FxHashMap<(Value, Block), Web>>,
    /// Per param-block: `(param index, its establisher blocks)` for the
    /// block's params that have any — computed once, almost always
    /// empty, and when empty `at` reduces to the unrestricted fixpoint
    /// for every param of that block.
    estp: std::cell::RefCell<FxHashMap<Block, Vec<(usize, Vec<Block>)>>>,
    poisoned: Cell<bool>,
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
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

/// The value a null test actually talks about: aliases and the
/// `select (x == k ? k : x)` sentinel idiom, which is `x` semantically.
/// Deliberately *not* `param_src`: a uniform-source param's value is its
/// source's binding at edge time and may diverge if the source re-binds,
/// so peeling it would attribute the fact to the wrong binding.
fn nn_subject(func: &Function, mut v: Value) -> Value {
    for _ in 0..8 {
        v = func.dfg.resolve_aliases(v);
        let Some(i) = func.dfg.value_def(v).inst() else {
            return v;
        };
        let InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } = func.dfg.insts[i]
        else {
            return v;
        };
        // select(icmp cc x, k) arms — the identity when one arm is the
        // tested constant and the other is the tested value.
        let Some(ci) = func.dfg.resolved_value_def(args[0]).inst() else {
            return v;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: cmp,
        } = func.dfg.insts[ci]
        else {
            return v;
        };
        let mut peeled = v;
        for (tt, ff, cc) in [
            (args[1], args[2], cond),
            (args[2], args[1], cond.complement()),
        ] {
            if cc != IntCC::Equal {
                continue;
            }
            let (x, kc) = match (iconst(func, cmp[0]), iconst(func, cmp[1])) {
                (Some(k), None) => (cmp[1], k),
                (None, Some(k)) => (cmp[0], k),
                _ => continue,
            };
            if iconst(func, tt) == Some(kc)
                && func.dfg.resolve_aliases(x) == func.dfg.resolve_aliases(ff)
            {
                peeled = x;
            }
        }
        if peeled == v {
            return v;
        }
        v = peeled;
    }
    v
}

impl Webs {
    pub fn new(func: &Function, cfg: &ControlFlowGraph) -> Webs {
        let mut w = Webs {
            edges: FxHashMap::default(),
            pslot: FxHashMap::default(),
            pw: FxHashMap::default(),
            est: FxHashMap::default(),
            dt: DominatorTree::with_function(func, cfg),
            memo: std::cell::RefCell::new(FxHashMap::default()),
            atmemo: std::cell::RefCell::new(FxHashMap::default()),
            estp: std::cell::RefCell::new(FxHashMap::default()),
            poisoned: Cell::new(false),
        };
        // Reachable blocks and their in-edges.
        let mut reach: FxHashSet<Block> = FxHashSet::default();
        if let Some(e) = func.layout.entry_block() {
            let mut st = vec![e];
            while let Some(b) = st.pop() {
                if reach.insert(b) {
                    st.extend(cfg.succ_iter(b));
                }
            }
        }
        for b in func.layout.blocks() {
            for (k, &p) in func.dfg.block_params(b).iter().enumerate() {
                w.pslot.insert(p, (b, k));
            }
        }
        for &b in &reach {
            for i in func.layout.block_insts(b) {
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    let dst = bc.block(&func.dfg.value_lists);
                    let args = bc
                        .args(&func.dfg.value_lists)
                        .map(|a| a.as_value().map(|v| func.dfg.resolve_aliases(v)))
                        .collect();
                    w.edges.entry(dst).or_default().push(Edge { args });
                }
            }
        }
        // Param fixpoint: `pw[p]` = union over in-edge args' webs. The
        // iteration must converge — an early-stopped least fixpoint
        // under-approximates and would hide escapes.
        let mut rounds = 0;
        loop {
            rounds += 1;
            let mut changed = false;
            for (b, es) in w.edges.iter() {
                for (k, &p) in func.dfg.block_params(*b).iter().enumerate() {
                    let mut nw = Web::default();
                    for e in es {
                        if let Some(Some(a)) = e.args.get(k) {
                            nw.join(&w.eval(func, *a, 0), &w.poisoned);
                        }
                    }
                    if w.pw.get(&p) != Some(&nw) {
                        w.pw.insert(p, nw);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
            if rounds >= MAX_ROUNDS {
                w.poisoned.set(true);
                break;
            }
            w.memo.borrow_mut().clear();
        }
        // Null-check establishers.
        for &b in &reach {
            let Some(t) = func.layout.last_inst(b) else {
                continue;
            };
            let InstructionData::Brif {
                opcode: Opcode::Brif,
                arg,
                ..
            } = func.dfg.insts[t]
            else {
                continue;
            };
            let bcs = func
                .dfg
                .insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            if bcs.len() != 2 {
                continue;
            }
            let Some(ci) = func.dfg.resolved_value_def(arg).inst() else {
                continue;
            };
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond,
                args: cmp,
            } = func.dfg.insts[ci]
            else {
                continue;
            };
            let x = match (iconst(func, cmp[0]), iconst(func, cmp[1])) {
                (Some(0), None) => cmp[1],
                (None, Some(0)) => cmp[0],
                _ => continue,
            };
            let idx = match cond {
                IntCC::Equal => 1,     // eq → else arm is non-null
                IntCC::NotEqual => 0,  // ne → then arm is non-null
                _ => continue,
            };
            let dst = bcs[idx].block(&func.dfg.value_lists);
            if cfg.pred_iter(dst).count() == 1 {
                w.est
                    .entry(nn_subject(func, x))
                    .or_default()
                    .push(dst);
            }
        }
        w
    }

    /// True if the analysis hit a representation limit — every web
    /// should then be treated as maximally conservative.
    pub fn poisoned(&self) -> bool {
        self.poisoned.get()
    }

    /// The alternatives `v` may hold when its use sits in `b`, with
    /// param edges contradicted by `b`'s established facts dropped.
    pub fn at(&self, func: &Function, v: Value, b: Block) -> Web {
        let v = func.dfg.resolve_aliases(v);
        // No establishers anywhere: the edge-refinement can never drop
        // an edge, so `at` is exactly the unrestricted eval — already
        // memoized and much cheaper than the per-edge walk.
        if self.est.is_empty() {
            return self.pois(self.eval(func, v, 0), func);
        }
        self.pois(self.at_in(func, v, b, 0), func)
    }

    fn pois(&self, w: Web, func: &Function) -> Web {
        if self.poisoned.get() {
            let mut w = w;
            for ss in func.sized_stack_slots.keys() {
                if !w.slots.iter().any(|&(s, _)| s == ss) {
                    w.slots.push((ss, ANY));
                }
            }
            w.nul = true;
            w.bad = true;
            w.other = true;
            w
        } else {
            w
        }
    }

    fn at_in(&self, func: &Function, v: Value, b: Block, d: usize) -> Web {
        if d > MAX_DEPTH {
            self.poisoned.set(true);
            return Web::other();
        }
        let v = func.dfg.resolve_aliases(v);
        // Block-refined results repeat both across inst operands and
        // inside the def-chain recursion below — without the memo an
        // `at` query on a K-deep iadd chain costs K per query.
        if let Some(w) = self.atmemo.borrow().get(&(v, b)) {
            return w.clone();
        }
        let w = self.at_in_inner(func, v, b, d);
        self.atmemo.borrow_mut().insert((v, b), w.clone());
        w
    }

    fn at_in_inner(&self, func: &Function, v: Value, b: Block, d: usize) -> Web {
        if let Some(&(blk, idx)) = self.pslot.get(&v) {
            // Restricted eval: an establisher `e` for sibling param `q`
            // dominating `b` means every last `blk` in-edge on paths to
            // `b` is followed by a traversal of `e` (`blk` dominates `e`
            // because `q`'s use dominates the guard), where `q`'s binding
            // from that edge was tested non-null — so an edge feeding `q`
            // a provably-null arg can never be the one that arrived and
            // is dropped for *every* param of `blk`. Keyed on `q` itself,
            // not a peeled subject: a peeled param may re-bind between
            // `blk`'s edge and `b`, breaking the value identity.
            //
            // `estp` is the per-block list of established param
            // positions — almost always empty, in which case no edge can
            // be dropped and the unrestricted fixpoint is exact.
            let eps = {
                let mut m = self.estp.borrow_mut();
                m.entry(blk)
                    .or_insert_with(|| {
                        func.dfg
                            .block_params(blk)
                            .iter()
                            .enumerate()
                            .filter_map(|(j, &q)| {
                                self.est.get(&q).cloned().map(|es| (j, es))
                            })
                            .collect()
                    })
                    .clone()
            };
            if eps.is_empty() {
                return self.pw.get(&v).cloned().unwrap_or_default();
            }
            let mut out = Web::default();
            if let Some(es) = self.edges.get(&blk) {
                'e: for e in es {
                    for (j, eb) in eps.iter() {
                        if !eb
                            .iter()
                            .any(|&e| self.dt.dominates(e, b, &func.layout))
                        {
                            continue;
                        }
                        let Some(&Some(a)) = e.args.get(*j) else {
                            continue;
                        };
                        if self.eval(func, a, 0).provably_null() {
                            continue 'e;
                        }
                    }
                    if let Some(&Some(a)) = e.args.get(idx) {
                        out.join(&self.eval(func, a, 0), &self.poisoned);
                    }
                }
            }
            return out;
        }
        let Some(i) = func.dfg.value_def(v).inst() else {
            return Web::other();
        };
        match func.dfg.insts[i] {
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } => {
                let mut w = self.at_in(func, args[1], b, d + 1);
                w.join(&self.at_in(func, args[2], b, d + 1), &self.poisoned);
                w
            }
            InstructionData::Binary {
                opcode: op @ (Opcode::Iadd | Opcode::Isub),
                args,
            } => self.iadd_web(func, v, op, args[0], args[1], b, d),
            _ => self.eval(func, v, 0),
        }
    }

    fn iadd_web(
        &self,
        func: &Function,
        v: Value,
        op: Opcode,
        x: Value,
        y: Value,
        b: Block,
        d: usize,
    ) -> Web {
        let _ = v;
        if let Some(k) = iconst(func, y) {
            let k = if op == Opcode::Isub { k.wrapping_neg() } else { k };
            self.at_in(func, x, b, d + 1).shifted(k)
        } else if op == Opcode::Iadd && let Some(k) = iconst(func, x) {
            self.at_in(func, y, b, d + 1).shifted(k)
        } else {
            let mut w = self.at_in(func, x, b, d + 1).widened();
            w.join(&self.at_in(func, y, b, d + 1).widened(), &self.poisoned);
            w
        }
    }

    /// Unrestricted eval — param values come from the fixpoint table,
    /// structural defs recurse (inst-result graphs are acyclic below
    /// params).
    fn eval(&self, func: &Function, v: Value, d: usize) -> Web {
        if d > MAX_DEPTH {
            self.poisoned.set(true);
            return Web::other();
        }
        let v = func.dfg.resolve_aliases(v);
        if let Some(w) = self.pw.get(&v) {
            return w.clone();
        }
        if let Some(w) = self.memo.borrow().get(&v) {
            return w.clone();
        }
        let Some(i) = func.dfg.value_def(v).inst() else {
            return Web::other();
        };
        let w = match func.dfg.insts[i] {
            InstructionData::StackAddr {
                stack_slot, offset, ..
            } => Web::slot(stack_slot, i64::from(i32::from(offset))),
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => {
                if imm.bits() == 0 {
                    Web::nul()
                } else {
                    Web::other()
                }
            }
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } => {
                let mut w = self.eval(func, args[1], d + 1);
                w.join(&self.eval(func, args[2], d + 1), &self.poisoned);
                w
            }
            InstructionData::Binary {
                opcode: op @ (Opcode::Iadd | Opcode::Isub),
                args,
            } => {
                if let Some(k) = iconst(func, args[1]) {
                    let k = if op == Opcode::Isub { k.wrapping_neg() } else { k };
                    self.eval(func, args[0], d + 1).shifted(k)
                } else if op == Opcode::Iadd && let Some(k) = iconst(func, args[0]) {
                    self.eval(func, args[1], d + 1).shifted(k)
                } else {
                    let mut w = self.eval(func, args[0], d + 1).widened();
                    w.join(&self.eval(func, args[1], d + 1).widened(), &self.poisoned);
                    w
                }
            }
            _ => Web::other(),
        };
        self.memo.borrow_mut().insert(v, w.clone());
        w
    }
}



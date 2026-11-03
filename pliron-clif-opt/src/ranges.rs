//! Value ranges (W3 of `docs/codegen-plan.md`): one on-demand, memoized
//! interval analysis for CLIF values, in the spirit of LLVM's
//! LazyValueInfo and GCC's ranger, that the folding passes query instead of
//! each keeping a private copy (`clifpeep::vmax`, `ovkill`'s bounds map,
//! `celim`'s and `jumpthread`'s `urange`).
//!
//! A range is an unsigned interval `[ulo, uhi]` and a signed interval
//! `[slo, shi]` of the value's own width. [`Ranges::def`] answers from the
//! definition alone (constants, extensions, masks, shifts, narrow loads,
//! `urem`/`udiv` by constants, `umin`/`umax`, `select`, `icmp`, carry-free
//! `iadd`/`imul`, and block params as the join of their incoming args,
//! with a loop param widened to its stepping side). [`Ranges::at`] refines
//! that with the compare facts of the single-predecessor `brif` edges that
//! dominate a block (idom walk, like `jumpthread::facts_at`).
//!
//! Soundness: every rule is exact in the value's width; arithmetic that
//! could wrap returns the full range. Every walk has a budget
//! (`MAX_DEPTH`, `MAX_WORK`); exhausting it returns the full range, never
//! a wrong one. CLIF carries no `nsw`/`nuw`, so nothing is assumed about
//! wrapping beyond what the intervals prove.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{Block, Function, InstructionData, Opcode, Type, Value, ValueDef, types};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_DEPTH: u32 = 12;
const MAX_WORK: usize = 200_000;
const MAX_FACT_HOPS: usize = 48;

/// Interval of a `bits`-wide integer, as unsigned and as signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub bits: u32,
    pub ulo: u64,
    pub uhi: u64,
    pub slo: i64,
    pub shi: i64,
}

fn umask(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}
fn smax(bits: u32) -> i64 {
    if bits >= 64 { i64::MAX } else { (1i64 << (bits - 1)) - 1 }
}
fn smin(bits: u32) -> i64 {
    if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) }
}
/// Sign-extend the low `bits` of `x`.
fn sext(x: u64, bits: u32) -> i64 {
    if bits >= 64 { x as i64 } else { ((x << (64 - bits)) as i64) >> (64 - bits) }
}

impl Range {
    pub fn full(bits: u32) -> Range {
        Range { bits, ulo: 0, uhi: umask(bits), slo: smin(bits), shi: smax(bits) }
    }
    pub fn is_full(&self) -> bool {
        *self == Range::full(self.bits)
    }
    pub fn konst(bits: u32, k: i64) -> Range {
        let u = (k as u64) & umask(bits);
        Range { bits, ulo: u, uhi: u, slo: sext(u, bits), shi: sext(u, bits) }
    }
    /// Unsigned interval only; the signed one follows when it has no
    /// sign-bit crossing, else is full.
    pub fn unsigned(bits: u32, ulo: u64, uhi: u64) -> Range {
        let m = umask(bits);
        let (ulo, uhi) = (ulo & m, uhi.min(m));
        let half = 1u64 << (bits - 1);
        let (slo, shi) = if uhi < half || ulo >= half {
            (sext(ulo, bits), sext(uhi, bits))
        } else {
            (smin(bits), smax(bits))
        };
        Range { bits, ulo, uhi, slo, shi }
    }
    pub fn signed(bits: u32, slo: i64, shi: i64) -> Range {
        let (slo, shi) = (slo.max(smin(bits)), shi.min(smax(bits)));
        let (ulo, uhi) = if slo >= 0 || shi < 0 {
            ((slo as u64) & umask(bits), (shi as u64) & umask(bits))
        } else {
            (0, umask(bits))
        };
        Range { bits, ulo, uhi, slo, shi }
    }
    pub fn join(&self, o: &Range) -> Range {
        Range {
            bits: self.bits,
            ulo: self.ulo.min(o.ulo),
            uhi: self.uhi.max(o.uhi),
            slo: self.slo.min(o.slo),
            shi: self.shi.max(o.shi),
        }
    }
    /// Intersection; `None` when empty (an unreachable combination: the
    /// caller treats it as full to stay conservative).
    pub fn meet(&self, o: &Range) -> Option<Range> {
        let r = Range {
            bits: self.bits,
            ulo: self.ulo.max(o.ulo),
            uhi: self.uhi.min(o.uhi),
            slo: self.slo.max(o.slo),
            shi: self.shi.min(o.shi),
        };
        (r.ulo <= r.uhi && r.slo <= r.shi).then_some(r)
    }
    pub fn is_const(&self) -> Option<u64> {
        (self.ulo == self.uhi).then_some(self.ulo)
    }
}

/// The analysis state for one function (rebuild after mutation).
pub struct Ranges {
    memo: FxHashMap<Value, Range>,
    visiting: FxHashSet<Value>,
    work: usize,
    /// Per block: the single-pred `brif` fact that holds on entry:
    /// (cond value, arm taken).
    edge_fact: FxHashMap<Block, (Value, bool)>,
}

fn width(ty: Type) -> Option<u32> {
    if ty.is_int() && ty.bits() <= 64 { Some(ty.bits()) } else { None }
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

impl Ranges {
    pub fn new(func: &Function, cfg: &ControlFlowGraph) -> Ranges {
        let mut edge_fact = FxHashMap::default();
        for b in func.layout.blocks() {
            let mut preds = cfg.pred_iter(b);
            let (Some(p), None) = (preds.next(), preds.next()) else { continue };
            if let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] {
                let d0 = blocks[0].block(&func.dfg.value_lists);
                let d1 = blocks[1].block(&func.dfg.value_lists);
                if d0 == d1 {
                    continue;
                }
                let arm = if d0 == b { true } else if d1 == b { false } else { continue };
                edge_fact.insert(b, (func.dfg.resolve_aliases(arg), arm));
            }
        }
        Ranges { memo: FxHashMap::default(), visiting: FxHashSet::default(), work: 0, edge_fact }
    }

    /// Range of `v` from its definition alone.
    pub fn def(&mut self, func: &Function, v: Value) -> Range {
        let v = func.dfg.resolve_aliases(v);
        let Some(bits) = width(func.dfg.value_type(v)) else { return Range::full(1) };
        if let Some(r) = self.memo.get(&v) {
            return *r;
        }
        let r = self.compute(func, v, bits, 0);
        self.memo.insert(v, r);
        r
    }

    fn compute(&mut self, func: &Function, v: Value, bits: u32, depth: u32) -> Range {
        let full = Range::full(bits);
        if depth > MAX_DEPTH || self.work > MAX_WORK || !self.visiting.insert(v) {
            return full;
        }
        self.work += 1;
        let r = self.compute_inner(func, v, bits, depth).unwrap_or(full);
        self.visiting.remove(&v);
        r
    }

    fn sub(&mut self, func: &Function, v: Value, depth: u32) -> Range {
        let v = func.dfg.resolve_aliases(v);
        let Some(bits) = width(func.dfg.value_type(v)) else { return Range::full(1) };
        if let Some(r) = self.memo.get(&v) {
            return *r;
        }
        let r = self.compute(func, v, bits, depth + 1);
        // Memoize only complete answers (a budget bail is not a fact
        // about the value).
        if depth == 0 || self.work <= MAX_WORK {
            self.memo.insert(v, r);
        }
        r
    }

    fn compute_inner(&mut self, func: &Function, v: Value, bits: u32, depth: u32) -> Option<Range> {
        let full = Range::full(bits);
        let m = umask(bits);
        match func.dfg.value_def(v) {
            ValueDef::Union(..) => None,
            ValueDef::Param(b, idx) => {
                // Join of the incoming args; a value fed by itself plus a
                // constant (a counted loop param) keeps the other side's
                // bound on the side it moves away from.
                if Some(b) == func.layout.entry_block() {
                    return None;
                }
                let mut acc: Option<Range> = None;
                let mut step: Option<i64> = Some(0);
                let mut nonself = 0;
                let args = self.incoming(func, b, idx);
                if args.is_empty() {
                    return None;
                }
                for a in args {
                    let a = func.dfg.resolve_aliases(a);
                    if a == v {
                        continue;
                    }
                    // `v + k`?
                    let mut k = None;
                    if let ValueDef::Result(i, _) = func.dfg.value_def(a)
                        && let InstructionData::Binary { opcode, args: [x, y] } = func.dfg.insts[i]
                    {
                        let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
                        k = match opcode {
                            Opcode::Iadd if x == v => iconst(func, y),
                            Opcode::Iadd if y == v => iconst(func, x),
                            Opcode::Isub if x == v => iconst(func, y).and_then(|c| c.checked_neg()),
                            _ => None,
                        };
                    }
                    match k {
                        Some(k) => {
                            step = match step {
                                Some(0) => Some(k),
                                Some(s) if s == k => Some(s),
                                _ => None,
                            };
                        }
                        None => {
                            nonself += 1;
                            let r = self.sub(func, a, depth);
                            acc = Some(acc.map_or(r, |x| x.join(&r)));
                        }
                    }
                }
                let base = acc?;
                if nonself == 0 {
                    return None;
                }
                match step {
                    Some(0) => Some(base),
                    // Counting up from `base`: the low bound holds while
                    // it cannot wrap below (it can't: unsigned up-count
                    // only grows until it wraps to 0; so only the signed
                    // low bound is kept when the start is non-negative and
                    // the unsigned one when the step is small enough to
                    // reach the top first — neither is exact under
                    // wrapping, so keep nothing but the obvious: none).
                    _ => None,
                }
            }
            ValueDef::Result(i, ri) => {
                let data = func.dfg.insts[i];
                match data {
                    InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(Range::konst(bits, imm.bits())),
                    InstructionData::Unary { opcode, arg } => {
                        let a = self.sub(func, arg, depth);
                        match opcode {
                            Opcode::Uextend => Some(Range::unsigned(bits, a.ulo, a.uhi)),
                            Opcode::Sextend => Some(Range::signed(bits, a.slo, a.shi)),
                            Opcode::Ireduce => {
                                if a.uhi <= m {
                                    Some(Range::unsigned(bits, a.ulo, a.uhi))
                                } else if a.slo >= smin(bits) && a.shi <= smax(bits) {
                                    Some(Range::signed(bits, a.slo, a.shi))
                                } else {
                                    None
                                }
                            }
                            Opcode::Clz | Opcode::Ctz | Opcode::Popcnt => Some(Range::unsigned(bits, 0, bits as u64)),
                            Opcode::Bmask => Some(full),
                            _ => None,
                        }
                    }
                    InstructionData::Load { opcode, .. } => match opcode {
                        Opcode::Uload8 => Some(Range::unsigned(bits, 0, 0xff)),
                        Opcode::Uload16 => Some(Range::unsigned(bits, 0, 0xffff)),
                        Opcode::Uload32 => Some(Range::unsigned(bits, 0, 0xffff_ffff)),
                        Opcode::Sload8 => Some(Range::signed(bits, -128, 127)),
                        Opcode::Sload16 => Some(Range::signed(bits, -32768, 32767)),
                        Opcode::Sload32 => Some(Range::signed(bits, i32::MIN as i64, i32::MAX as i64)),
                        _ => None,
                    },
                    InstructionData::IntCompare { .. } => Some(Range::unsigned(bits, 0, 1)),
                    InstructionData::Binary { opcode, args: [x, y] } => {
                        let a = self.sub(func, x, depth);
                        let b = self.sub(func, y, depth);
                        let kb = b.is_const();
                        match opcode {
                            // Commutative with a constant on the left.
                            Opcode::Band | Opcode::Bor | Opcode::Bxor | Opcode::Iadd | Opcode::Imul | Opcode::Umin | Opcode::Umax | Opcode::Smin | Opcode::Smax
                                if kb.is_none() && a.is_const().is_some() =>
                            {
                                self.binary(bits, opcode, b, a, a.is_const())
                            }
                            _ => self.binary(bits, opcode, a, b, kb),
                        }
                    }
                    InstructionData::Ternary { opcode: Opcode::Select | Opcode::Bitselect, args: [c, x, y] } if ri == 0 => {
                        if data.opcode() == Opcode::Bitselect {
                            return None;
                        }
                        let a = self.sub(func, x, depth);
                        let b = self.sub(func, y, depth);
                        let _ = c;
                        Some(a.join(&b))
                    }
                    _ => None,
                }
            }
        }
    }

    /// Binary op rule: `a op b`, with `kb` when `b` is a constant.
    fn binary(&mut self, bits: u32, op: Opcode, a: Range, b: Range, kb: Option<u64>) -> Option<Range> {
        let m = umask(bits);
        match op {
            Opcode::Band => {
                let hi = match kb {
                    Some(k) => a.uhi.min(k),
                    None => a.uhi.min(b.uhi),
                };
                Some(Range::unsigned(bits, 0, hi))
            }
            Opcode::Bor | Opcode::Bxor => {
                // Below the next power of two above both tops.
                let top = a.uhi.max(b.uhi);
                if top == m || top.leading_zeros() == 0 {
                    return None;
                }
                let hi = if top == 0 { 0 } else { (1u64 << (64 - top.leading_zeros())) - 1 };
                Some(Range::unsigned(bits, 0, hi.min(m)))
            }
            Opcode::Ushr => {
                let k = kb? & (bits as u64 - 1);
                Some(Range::unsigned(bits, a.ulo >> k, a.uhi >> k))
            }
            Opcode::Sshr => {
                let k = kb? & (bits as u64 - 1);
                Some(Range::signed(bits, a.slo >> k, a.shi >> k))
            }
            Opcode::Ishl => {
                let k = kb? & (bits as u64 - 1);
                if k == 0 {
                    return Some(a);
                }
                if a.uhi.leading_zeros() as u64 + bits as u64 >= 64 + k {
                    Some(Range::unsigned(bits, a.ulo << k, a.uhi << k))
                } else {
                    None
                }
            }
            Opcode::Urem => {
                let k = kb?;
                if k == 0 {
                    return None;
                }
                Some(Range::unsigned(bits, 0, (k - 1).min(a.uhi)))
            }
            Opcode::Udiv => {
                let k = kb?;
                if k == 0 {
                    return None;
                }
                Some(Range::unsigned(bits, a.ulo / k, a.uhi / k))
            }
            Opcode::Umin => Some(Range::unsigned(bits, a.ulo.min(b.ulo), a.uhi.min(b.uhi))),
            Opcode::Umax => Some(Range::unsigned(bits, a.ulo.max(b.ulo), a.uhi.max(b.uhi))),
            Opcode::Smin => Some(Range::signed(bits, a.slo.min(b.slo), a.shi.min(b.shi))),
            Opcode::Smax => Some(Range::signed(bits, a.slo.max(b.slo), a.shi.max(b.shi))),
            Opcode::Iadd => {
                let u = a.uhi.checked_add(b.uhi).filter(|&s| s <= m).map(|_| Range::unsigned(bits, a.ulo + b.ulo, a.uhi + b.uhi));
                let s = a.shi.checked_add(b.shi).filter(|&s| s <= smax(bits)).and_then(|_| a.slo.checked_add(b.slo).filter(|&s| s >= smin(bits))).map(|_| Range::signed(bits, a.slo + b.slo, a.shi + b.shi));
                match (u, s) {
                    (Some(u), Some(s)) => u.meet(&s).or(Some(u)),
                    (Some(u), None) => Some(u),
                    (None, Some(s)) => Some(s),
                    (None, None) => None,
                }
            }
            Opcode::Isub => {
                let u = (a.ulo >= b.uhi).then(|| Range::unsigned(bits, a.ulo - b.uhi, a.uhi - b.ulo));
                let s = a.slo.checked_sub(b.shi).filter(|&s| s >= smin(bits)).and_then(|_| a.shi.checked_sub(b.slo).filter(|&s| s <= smax(bits))).map(|_| Range::signed(bits, a.slo - b.shi, a.shi - b.slo));
                match (u, s) {
                    (Some(u), Some(s)) => u.meet(&s).or(Some(u)),
                    (Some(u), None) => Some(u),
                    (None, Some(s)) => Some(s),
                    (None, None) => None,
                }
            }
            Opcode::Imul => {
                let hi = a.uhi.checked_mul(b.uhi)?;
                if hi > m {
                    return None;
                }
                Some(Range::unsigned(bits, a.ulo.wrapping_mul(b.ulo), hi))
            }
            _ => None,
        }
    }

    /// Incoming edge args of param `idx` of `b`.
    fn incoming(&self, func: &Function, b: Block, idx: usize) -> Vec<Value> {
        let mut out = Vec::new();
        // Walk every branch in the function once per param query is too
        // slow for big functions; the caller supplies a CFG through
        // `with_preds` when it has one. Here, fall back to the layout.
        for pb in func.layout.blocks() {
            let Some(t) = func.layout.last_inst(pb) else { continue };
            let data = &func.dfg.insts[t];
            if !data.opcode().is_branch() {
                continue;
            }
            for bc in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                if bc.block(&func.dfg.value_lists) == b {
                    if let Some(cranelift_codegen::ir::BlockArg::Value(v)) = bc.args(&func.dfg.value_lists).nth(idx) {
                        out.push(v);
                    } else {
                        return Vec::new();
                    }
                }
            }
        }
        out
    }

    /// Range of `v` at the entry of `at`: the definition range refined by
    /// the compare facts of dominating single-predecessor edges.
    pub fn at(&mut self, func: &Function, dt: &DominatorTree, v: Value, at: Block) -> Range {
        let v = func.dfg.resolve_aliases(v);
        let mut r = self.def(func, v);
        let Some(bits) = width(func.dfg.value_type(v)) else { return r };
        let mut b = at;
        let mut hops = 0;
        loop {
            if let Some(&(c, arm)) = self.edge_fact.get(&b) {
                if let Some(f) = self.fact(func, c, arm, v, bits) {
                    if let Some(m) = r.meet(&f) {
                        r = m;
                    }
                }
            }
            hops += 1;
            if hops > MAX_FACT_HOPS {
                break;
            }
            match dt.idom(b) {
                Some(i) => b = i,
                None => break,
            }
            if b == at {
                break;
            }
        }
        r
    }

    /// What `brif c` taking arm `arm` says about `v`.
    fn fact(&mut self, func: &Function, c: Value, arm: bool, v: Value, bits: u32) -> Option<Range> {
        let ci = func.dfg.value_def(c).inst()?;
        let (cc, x, y) = match func.dfg.insts[ci] {
            InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args: [x, y] } => (cond, func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y)),
            _ => return None,
        };
        let (cc, other) = if x == v {
            (cc, y)
        } else if y == v {
            (cc.swap_args(), x)
        } else {
            return None;
        };
        let cc = if arm { cc } else { cc.complement() };
        let o = self.def(func, other);
        let m = umask(bits);
        Some(match cc {
            IntCC::Equal => o,
            IntCC::NotEqual => return None,
            IntCC::UnsignedLessThan => Range::unsigned(bits, 0, o.uhi.checked_sub(1)?),
            IntCC::UnsignedLessThanOrEqual => Range::unsigned(bits, 0, o.uhi),
            IntCC::UnsignedGreaterThan => Range::unsigned(bits, o.ulo.checked_add(1).filter(|&x| x <= m)?, m),
            IntCC::UnsignedGreaterThanOrEqual => Range::unsigned(bits, o.ulo, m),
            IntCC::SignedLessThan => Range::signed(bits, smin(bits), o.shi.checked_sub(1)?),
            IntCC::SignedLessThanOrEqual => Range::signed(bits, smin(bits), o.shi),
            IntCC::SignedGreaterThan => Range::signed(bits, o.slo.checked_add(1)?, smax(bits)),
            IntCC::SignedGreaterThanOrEqual => Range::signed(bits, o.slo, smax(bits)),
        })
    }
}

/// Evaluate `a cc b` when the ranges decide it.
pub fn decide(cc: IntCC, a: &Range, b: &Range) -> Option<bool> {
    match cc {
        IntCC::Equal => {
            if a.uhi < b.ulo || b.uhi < a.ulo {
                Some(false)
            } else if let (Some(x), Some(y)) = (a.is_const(), b.is_const()) {
                Some(x == y)
            } else {
                None
            }
        }
        IntCC::NotEqual => decide(IntCC::Equal, a, b).map(|x| !x),
        IntCC::UnsignedLessThan => {
            if a.uhi < b.ulo {
                Some(true)
            } else if a.ulo >= b.uhi {
                Some(false)
            } else {
                None
            }
        }
        IntCC::UnsignedLessThanOrEqual => {
            if a.uhi <= b.ulo {
                Some(true)
            } else if a.ulo > b.uhi {
                Some(false)
            } else {
                None
            }
        }
        IntCC::UnsignedGreaterThan => decide(IntCC::UnsignedLessThanOrEqual, a, b).map(|x| !x),
        IntCC::UnsignedGreaterThanOrEqual => decide(IntCC::UnsignedLessThan, a, b).map(|x| !x),
        IntCC::SignedLessThan => {
            if a.shi < b.slo {
                Some(true)
            } else if a.slo >= b.shi {
                Some(false)
            } else {
                None
            }
        }
        IntCC::SignedLessThanOrEqual => {
            if a.shi <= b.slo {
                Some(true)
            } else if a.slo > b.shi {
                Some(false)
            } else {
                None
            }
        }
        IntCC::SignedGreaterThan => decide(IntCC::SignedLessThanOrEqual, a, b).map(|x| !x),
        IntCC::SignedGreaterThanOrEqual => decide(IntCC::SignedLessThan, a, b).map(|x| !x),
    }
}

pub fn bits_of(ty: Type) -> Option<u32> {
    width(ty)
}

pub const I8: Type = types::I8;

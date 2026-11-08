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
/// `iadd`/`isub`-const chain peel and `lin` recursion limits for the
/// correlated-difference machinery (`implied`).
const STRIP_DEPTH: u32 = 8;
const LIN_DEPTH: u32 = 3;
const DIFF_HOPS: usize = 16;
/// Most predecessors a multi-pred block may have and still contribute
/// all-agree edge facts (a wider join is a merge point, not a fact
/// source, and the per-edge work would grow unbounded).
const MAX_EDGE_PREDS: usize = 8;

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

/// A value of a type the analysis does not model (vectors, i128): every
/// 64-bit pattern, so any rule consuming it stays conservative.
pub const UNKNOWN: Range = Range { bits: 0, ulo: 0, uhi: u64::MAX, slo: i64::MIN, shi: i64::MAX };

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
    /// (cond value, arm taken, branch-arg → block-param bindings).
    /// Compared values that cross the edge as arguments get renamed to
    /// the param so facts reach the param's uses.
    edge_fact: FxHashMap<Block, (Value, bool, FxHashMap<Value, Value>)>,
    /// Per multi-pred block whose EVERY pred is a `brif` with a
    /// distinct other dest: one (cond, arm-into-block, arg_map) per
    /// edge. A fact holds on entry only when every edge proves it.
    edge_facts_multi: FxHashMap<Block, Vec<(Value, bool, FxHashMap<Value, Value>)>>,
    /// Block param → its single incoming source value, when every
    /// non-self edge arg names the same value (loop-preserving self
    /// edges keep the induction). One pass at construction.
    param_src: FxHashMap<Value, Value>,
    /// `at()` results per (value, block); unrolled runs re-ask the
    /// same question down the same dominator chain.
    at_memo: FxHashMap<(Value, Block), Range>,
}

fn width(ty: Type) -> Option<u32> {
    if ty.is_int() && ty.bits() <= 64 { Some(ty.bits()) } else { None }
}

/// On a self-edge (`pred == dest`) a compare operand that is a param
/// of `dest` and was not passed as an arg refers to the OLD binding —
/// the fact says nothing about the param the edge binds. Poison those
/// params so the translation never matches them to a query.
fn poison_self_params(func: &Function, b: Block, pred: Block, arg_map: &mut FxHashMap<Value, Value>) {
    if pred != b {
        return;
    }
    for &pv in func.dfg.block_params(b) {
        arg_map.entry(pv).or_insert(Value::from_bits(u32::MAX));
    }
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

/// `v` with `iadd`/`isub`-by-constant peeled: `(base, off)` with machine
/// `v = base + off`. Pure syntactic strip; wrap checks are the callers'
/// (`fits`). Depth-bounded for the `x+1+1+…` chains unrolling leaves.
fn strip_add(func: &Function, mut v: Value, mut depth: u32) -> (Value, i128) {
    let mut off: i128 = 0;
    loop {
        v = func.dfg.resolve_aliases(v);
        if depth == 0 {
            return (v, off);
        }
        depth -= 1;
        let bits = func.dfg.value_type(v).bits();
        let next = match func.dfg.value_def(v) {
            ValueDef::Result(i, _) => match func.dfg.insts[i] {
                InstructionData::Binary { opcode: Opcode::Iadd, args: [x, y] } => {
                    let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
                    if let Some(k) = iconst(func, y) {
                        Some((x, k, false))
                    } else {
                        iconst(func, x).map(|k| (y, k, false))
                    }
                }
                InstructionData::Binary { opcode: Opcode::Isub, args: [x, y] } => {
                    iconst(func, func.dfg.resolve_aliases(y))
                        .map(|k| (func.dfg.resolve_aliases(x), k, true))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((nv, k, neg)) = next else { return (v, off) };
        v = nv;
        let k = sext((k as u64) & umask(bits), bits) as i128;
        off += if neg { -k } else { k };
    }
}

/// `base + off` stays in `0..=m` for `base`'s whole range — the machine
/// `iadd`/`isub` then equals the integer sum.
fn fits(m: u64, r: &Range, off: i128) -> bool {
    if off >= 0 {
        r.uhi as i128 + off <= m as i128
    } else {
        r.ulo as i128 + off >= 0
    }
}

impl Ranges {
    pub fn new(func: &Function, cfg: &ControlFlowGraph) -> Ranges {
        let mut edge_fact = FxHashMap::default();
        // Param single-sources: one pass over every branch arg in the
        // function (the `resolve_param` queries would otherwise rescan
        // the layout per block param).
        let mut param_src: FxHashMap<Value, Option<Value>> = FxHashMap::default();
        for pb in func.layout.blocks() {
            let Some(t) = func.layout.last_inst(pb) else { continue };
            let data = &func.dfg.insts[t];
            if !data.opcode().is_branch() {
                continue;
            }
            for bc in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                let db = bc.block(&func.dfg.value_lists);
                let params = func.dfg.block_params(db);
                for (i, &pv) in params.iter().enumerate() {
                    // An edge passing no value for this param poisons it
                    // (exception-table edges bind only the tag operands).
                    let Some(cranelift_codegen::ir::BlockArg::Value(v)) = bc.args(&func.dfg.value_lists).nth(i) else {
                        param_src.insert(pv, None);
                        continue;
                    };
                    let v = func.dfg.resolve_aliases(v);
                    if v == pv {
                        continue;
                    }
                    match param_src.entry(pv) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(Some(v));
                        }
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            if *e.get() != Some(v) {
                                e.insert(None);
                            }
                        }
                    }
                }
            }
        }
        let param_src = param_src
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect();
        let mut edge_facts_multi: FxHashMap<Block, Vec<(Value, bool, FxHashMap<Value, Value>)>> =
            FxHashMap::default();
        for b in func.layout.blocks() {
            let mut preds = cfg.pred_iter(b);
            let Some(p) = preds.next() else { continue };
            if preds.next().is_none() {
                if let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] {
                    let d0 = blocks[0].block(&func.dfg.value_lists);
                    let d1 = blocks[1].block(&func.dfg.value_lists);
                    if d0 == d1 {
                        continue;
                    }
                    let (arm, bc) = if d0 == b {
                        (true, &blocks[0])
                    } else if d1 == b {
                        (false, &blocks[1])
                    } else {
                        continue;
                    };
                    let mut arg_map = FxHashMap::default();
                    let params = func.dfg.block_params(b);
                    for (i, ba) in bc.args(&func.dfg.value_lists).enumerate() {
                        if let (cranelift_codegen::ir::BlockArg::Value(v), Some(&pv)) =
                            (ba, params.get(i))
                        {
                            arg_map.insert(func.dfg.resolve_aliases(v), pv);
                        }
                    }
                    poison_self_params(func, b, p.block, &mut arg_map);
                    edge_fact.insert(b, (func.dfg.resolve_aliases(arg), arm, arg_map));
                }
                continue;
            }
            // Multi-pred: every pred a `brif` with a distinct other
            // dest records one edge; any other shape proves nothing.
            if 2 + preds.count() > MAX_EDGE_PREDS {
                continue;
            }
            let mut edges = Vec::new();
            let mut ok = true;
            for p in cfg.pred_iter(b) {
                let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] else {
                    ok = false;
                    break;
                };
                let d0 = blocks[0].block(&func.dfg.value_lists);
                let d1 = blocks[1].block(&func.dfg.value_lists);
                if d0 == d1 {
                    ok = false;
                    break;
                }
                let (arm, bc) = if d0 == b {
                    (true, &blocks[0])
                } else if d1 == b {
                    (false, &blocks[1])
                } else {
                    ok = false;
                    break;
                };
                let mut arg_map = FxHashMap::default();
                let params = func.dfg.block_params(b);
                for (i, ba) in bc.args(&func.dfg.value_lists).enumerate() {
                    if let (cranelift_codegen::ir::BlockArg::Value(v), Some(&pv)) =
                        (ba, params.get(i))
                    {
                        arg_map.insert(func.dfg.resolve_aliases(v), pv);
                    }
                }
                poison_self_params(func, b, p.block, &mut arg_map);
                edges.push((func.dfg.resolve_aliases(arg), arm, arg_map));
            }
            if ok {
                edge_facts_multi.insert(b, edges);
            }
        }
        Ranges {
            memo: FxHashMap::default(),
            visiting: FxHashSet::default(),
            work: 0,
            edge_fact,
            edge_facts_multi,
            param_src,
            at_memo: FxHashMap::default(),
        }
    }

    /// Range of `v` from its definition alone.
    pub fn def(&mut self, func: &Function, v: Value) -> Range {
        let v = func.dfg.resolve_aliases(v);
        let Some(bits) = width(func.dfg.value_type(v)) else { return UNKNOWN };
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
        let Some(bits) = width(func.dfg.value_type(v)) else { return UNKNOWN };
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

    /// Incoming edge args of param `idx` of `b`. Whole-layout scan —
    /// call only under `def`'s per-value memo, never from a per-query
    /// recursion (`param_src` covers the single-source query).
    fn incoming(&self, func: &Function, b: Block, idx: usize) -> Vec<Value> {
        let mut out = Vec::new();
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
        if let Some(&r) = self.at_memo.get(&(v, at)) {
            return r;
        }
        let mut r = self.def(func, v);
        let Some(bits) = width(func.dfg.value_type(v)) else {
            self.at_memo.insert((v, at), r);
            return r;
        };
        let mut b = at;
        let mut hops = 0;
        loop {
            let ef = self
                .edge_fact
                .get(&b)
                .map(|(c, arm, m)| (*c, *arm, m.clone()));
            if let Some((c, arm, arg_map)) = ef {
                if let Some(f) = self.fact(func, c, arm, v, bits, &arg_map) {
                    if let Some(m) = r.meet(&f) {
                        r = m;
                    }
                }
            }
            if let Some(edges) = self.edge_facts_multi.get(&b) {
                // Each edge's fact is conditional on arriving via that
                // edge: `v` at `b` is the JOIN of the per-edge ranges
                // (unconditional), but only when every edge yields a
                // bound — one that can't leaves `v` unrestricted.
                let edges = edges.clone();
                let mut u = None;
                for (c, arm, arg_map) in edges.iter() {
                    match self.fact(func, *c, *arm, v, bits, arg_map) {
                        Some(f) => {
                            u = Some(u.map_or(f, |u: Range| u.join(&f)));
                        }
                        None => {
                            u = None;
                            break;
                        }
                    }
                }
                if let Some(u) = u
                    && let Some(m) = r.meet(&u)
                {
                    r = m;
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
        self.at_memo.insert((v, at), r);
        r
    }

    /// What `brif c` taking arm `arm` says about `v`. `arg_map` renames
    /// the edge's branch args to the dest block's params so the fact
    /// reaches values bound on the edge.
    fn fact(&mut self, func: &Function, c: Value, arm: bool, v: Value, bits: u32, arg_map: &FxHashMap<Value, Value>) -> Option<Range> {
        // `c` itself (or the param it binds on this edge): 0 on the
        // false arm, nonzero on the true (brif tests != 0 at any width).
        if arg_map.get(&c).copied().unwrap_or(c) == v {
            return Some(if arm {
                Range::unsigned(bits, 1, umask(bits))
            } else {
                Range::konst(bits, 0)
            });
        }
        let ci = func.dfg.value_def(c).inst()?;
        let (cc, x, y) = match func.dfg.insts[ci] {
            InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args: [x, y] } => (cond, func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y)),
            _ => return None,
        };
        let x = arg_map.get(&x).copied().unwrap_or(x);
        let y = arg_map.get(&y).copied().unwrap_or(y);
        // A poisoned operand (self-edge old binding) invalidates the fact.
        if !func.dfg.value_is_valid(x) || !func.dfg.value_is_valid(y) {
            return None;
        }
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
            // `x != k` tightens one side only when k sits at a domain
            // edge: != 0 / != umax bound the unsigned side, != smin /
            // != smax the signed. Anything else is not an interval.
            IntCC::NotEqual => {
                let k = o.is_const()?;
                if k == 0 {
                    Range::unsigned(bits, 1, m)
                } else if k == m {
                    Range::unsigned(bits, 0, m - 1)
                } else {
                    let sk = sext(k, bits);
                    if sk == smin(bits) {
                        Range::signed(bits, smin(bits) + 1, smax(bits))
                    } else if sk == smax(bits) {
                        Range::signed(bits, smin(bits), smax(bits) - 1)
                    } else {
                        return None;
                    }
                }
            }
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

    // ---- correlated-difference facts ----
    //
    // `at()` refines one value at a time; it cannot use a dominating
    // `idx < end - min(end, k)` guard to decide `idx + j < end`.
    // `implied` instead treats each dominating edge compare as an
    // integer inequality and bounds `D = yb - xb`, which decides the
    // sign of `x - y = (kx - ky) - D` for `x = xb + kx`, `y = yb + ky`.
    // Every reconstructed side needs a no-wrap certificate (`fits` on
    // the `at()`-refined range): machine `b + k` equals the integer
    // `b + k` only when `b`'s range plus `k` stays inside `0..=m`.

    /// `v` as the integer `cx·xb + cy·yb + t` evaluated at `cb`, or
    /// None. `cx`/`cy` are small (usually ±1); constructions that can
    /// wrap or aren't affine in the bases fail.
    fn lin(&mut self, func: &Function, dt: &DominatorTree, v: Value, xb: Value, yb: Value, cb: Block, depth: u32) -> Option<(i32, i32, i128)> {
        let v = func.dfg.resolve_aliases(v);
        if v == xb {
            return Some((1, 0, 0));
        }
        if v == yb {
            return Some((0, 1, 0));
        }
        let v = self.resolve_param(func, v);
        if v == xb {
            return Some((1, 0, 0));
        }
        if v == yb {
            return Some((0, 1, 0));
        }
        let bits = width(func.dfg.value_type(v))?;
        let r = self.at(func, dt, v, cb);
        if let Some(k) = r.is_const() {
            return Some((0, 0, k as i128));
        }
        if depth == 0 {
            return None;
        }
        let (b, o) = strip_add(func, v, STRIP_DEPTH);
        if b != v {
            if !fits(umask(bits), &self.at(func, dt, b, cb), o) {
                return None;
            }
            let (cx, cy, t) = self.lin(func, dt, b, xb, yb, cb, depth - 1)?;
            return Some((cx, cy, t + o));
        }
        if let ValueDef::Result(i, _) = func.dfg.value_def(v)
            && let InstructionData::Binary { opcode: Opcode::Isub, args: [p, q] } = func.dfg.insts[i]
        {
            let (p, q) = (func.dfg.resolve_aliases(p), func.dfg.resolve_aliases(q));
            if self.no_wrap_sub(func, p, q) {
                let (cx1, cy1, t1) = self.lin(func, dt, p, xb, yb, cb, depth - 1)?;
                let (cx2, cy2, t2) = self.lin(func, dt, q, xb, yb, cb, depth - 1)?;
                return Some((cx1 - cx2, cy1 - cy2, t1 - t2));
            }
        }
        None
    }

    /// `isub p, q` computes the nonnegative integer `p - q` (no wrap):
    /// `q <= p` for every value, from ranges or `q`'s shape.
    fn no_wrap_sub(&mut self, func: &Function, p: Value, q: Value) -> bool {
        if self.def(func, q).uhi <= self.def(func, p).ulo {
            return true;
        }
        let (p, q) = (func.dfg.resolve_aliases(p), func.dfg.resolve_aliases(q));
        let ValueDef::Result(i, _) = func.dfg.value_def(q) else { return false };
        match func.dfg.insts[i] {
            // `min(p,·) <= p`, `p & · <= p`, `p % · <= p`, `p >> · <= p`.
            InstructionData::Binary { opcode, args: [x, y] } => {
                let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
                match opcode {
                    Opcode::Umin | Opcode::Band | Opcode::Urem => x == p || y == p,
                    Opcode::Ushr => x == p,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// `q` is `umin(p, k)`: the value `q` is forced to whenever
    /// `p - q > 0` is known (satisfiability: `p - min(p,k) >= 1` needs
    /// `p > k`, so `min(p,k) = k`).
    fn umin_bound(&self, func: &Function, p: Value, q: Value) -> Option<i64> {
        let p = func.dfg.resolve_aliases(p);
        let ValueDef::Result(i, _) = func.dfg.value_def(func.dfg.resolve_aliases(q)) else { return None };
        if let InstructionData::Binary { opcode: Opcode::Umin, args: [x, y] } = func.dfg.insts[i] {
            let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
            if x == p {
                return iconst(func, y);
            }
            if y == p {
                return iconst(func, x);
            }
        }
        None
    }

    /// `A - B` expressed in `D = yb - xb`: returns `(cy, t)` with
    /// `A - B = cy·D + t`, `cy` ±1 — the only coefficients that decide
    /// the sign from a `D` bound.
    fn diff_term(&mut self, func: &Function, dt: &DominatorTree, a: Value, b: Value, xb: Value, yb: Value, cb: Block) -> Option<(i32, i128)> {
        let (cxa, cya, ta) = self.lin(func, dt, a, xb, yb, cb, LIN_DEPTH)?;
        let (cxb, cyb, tb) = self.lin(func, dt, b, xb, yb, cb, LIN_DEPTH)?;
        let (cx, cy) = (cxa - cxb, cya - cyb);
        if cx + cy != 0 || cy.abs() != 1 {
            return None;
        }
        Some((cy, ta - tb))
    }

    /// `A cc (P - Q)`: `P - Q` a certified nonnegative integer whose
    /// `lin` failed (the `umin`-clamped remainder idiom). Bounds `D`
    /// through `P - A` vs `Q`'s range.
    fn sub_diff(&mut self, func: &Function, dt: &DominatorTree, a: Value, b: Value, cc: IntCC, xb: Value, yb: Value, cb: Block, lo: &mut i128, hi: &mut i128) {
        let b = self.resolve_param(func, b);
        let ValueDef::Result(i, _) = func.dfg.value_def(b) else { return };
        let InstructionData::Binary { opcode: Opcode::Isub, args: [p, q] } = func.dfg.insts[i] else { return };
        let (p, q) = (func.dfg.resolve_aliases(p), func.dfg.resolve_aliases(q));
        if !self.no_wrap_sub(func, p, q) {
            return;
        }
        let Some((1, 0, at_)) = self.lin(func, dt, a, xb, yb, cb, LIN_DEPTH) else { return };
        let Some((0, 1, pt)) = self.lin(func, dt, p, xb, yb, cb, LIN_DEPTH) else { return };
        // `A cc (P - Q)` ⇔ `A + Q - P cc 0` ⇔ `Q - (D + pt - at_) cc 0`
        // — bounds on `Q` transfer to `D`. `e = at_ - pt`.
        let e = at_ - pt;
        let rq = self.at(func, dt, q, cb);
        let (mut qlo, qhi) = (rq.ulo as i128, rq.uhi as i128);
        match cc {
            // `A < P - Q` ⇒ `P - A > Q` ⇒ `D + pt - at_ >= Q + 1`; with
            // `q = umin(p, k)` satisfiability forces `q = k`.
            IntCC::UnsignedLessThan => {
                if let Some(k) = self.umin_bound(func, p, q) {
                    qlo = qlo.max(k as i128);
                }
                *lo = (*lo).max(e + qlo + 1)
            }
            IntCC::UnsignedLessThanOrEqual => *lo = (*lo).max(e + qlo),
            // `A >= P - Q` ⇔ `P - A <= Q` ⇒ `D <= at_ - pt + qhi`.
            IntCC::UnsignedGreaterThanOrEqual => *hi = (*hi).min(e + qhi),
            IntCC::UnsignedGreaterThan => *hi = (*hi).min(e + qhi - 1),
            // `A = P - Q` ⇔ `D = at_ - pt + Q`.
            IntCC::Equal => {
                *lo = (*lo).max(e + qlo);
                *hi = (*hi).min(e + qhi);
            }
            _ => {}
        }
    }

    /// `cy·D + t cc 0` narrows `[lo, hi]` for `D`.
    fn cmp_bound(cc: IntCC, cy: i32, t: i128, lo: &mut i128, hi: &mut i128) {
        match (cy, cc) {
            (1, IntCC::UnsignedLessThan) => *hi = (*hi).min(-t - 1),
            (1, IntCC::UnsignedLessThanOrEqual) => *hi = (*hi).min(-t),
            (1, IntCC::UnsignedGreaterThan) => *lo = (*lo).max(-t + 1),
            (1, IntCC::UnsignedGreaterThanOrEqual) => *lo = (*lo).max(-t),
            (-1, IntCC::UnsignedLessThan) => *lo = (*lo).max(t + 1),
            (-1, IntCC::UnsignedLessThanOrEqual) => *lo = (*lo).max(t),
            (-1, IntCC::UnsignedGreaterThan) => *hi = (*hi).min(t - 1),
            (-1, IntCC::UnsignedGreaterThanOrEqual) => *hi = (*hi).min(t),
            (_, IntCC::Equal) => {
                let d = if cy == 1 { -t } else { t };
                *lo = (*lo).max(d);
                *hi = (*hi).min(d);
            }
            _ => {}
        }
    }

    /// `v` behind single-source block-param bindings: when a param's
    /// non-self incoming args all name one value (precomputed in
    /// `param_src`), the param is that value for analysis purposes.
    /// Bounded chain.
    fn resolve_param(&self, func: &Function, mut v: Value) -> Value {
        for _ in 0..8 {
            v = func.dfg.resolve_aliases(v);
            match self.param_src.get(&v) {
                Some(&w) => v = w,
                None => return v,
            }
        }
        v
    }

    /// One dominating edge compare's contribution to `[lo, hi]` for
    /// `D = yb - xb` at edge-dest block `cb`.
    fn edge_diff(&mut self, func: &Function, dt: &DominatorTree, c: Value, arm: bool, xb: Value, yb: Value, cb: Block, arg_map: &FxHashMap<Value, Value>, lo: &mut i128, hi: &mut i128) {
        let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst() else { return };
        let InstructionData::IntCompare { opcode: Opcode::Icmp, cond: cc0, args: [a2, b2] } = func.dfg.insts[ci] else { return };
        let cc0 = if arm { cc0 } else { cc0.complement() };
        for (a, b, cc) in [(a2, b2, cc0), (b2, a2, cc0.swap_args())] {
            let a = arg_map.get(&a).copied().unwrap_or(a);
            let b = arg_map.get(&b).copied().unwrap_or(b);
            if !func.dfg.value_is_valid(a) || !func.dfg.value_is_valid(b) {
                continue;
            }
            if let Some((cy, t)) = self.diff_term(func, dt, a, b, xb, yb, cb) {
                Self::cmp_bound(cc, cy, t, lo, hi);
                continue;
            }
            self.sub_diff(func, dt, a, b, cc, xb, yb, cb, lo, hi);
        }
    }

    /// What `brif c` arm `arm` says about `x == y` on that edge:
    /// Some(true) provably equal, Some(false) provably different —
    /// the relations interval facts can't express (`x != y` for
    /// non-constant `y`). `arg_map` is as in `fact`.
    fn edge_rel(&mut self, func: &Function, c: Value, arm: bool, arg_map: &FxHashMap<Value, Value>, x: Value, y: Value) -> Option<bool> {
        // `c` bound to a param: pv = 0 on the false arm, nonzero on the
        // true. `pv == other` settles when `other` is a constant (0 is
        // decided on both arms; nonzero only on the false arm).
        if let Some(&pv) = arg_map.get(&c) {
            for (q, other) in [(x, y), (y, x)] {
                if q != pv {
                    continue;
                }
                if let Some(ko) = iconst(func, other) {
                    if !arm {
                        return Some(ko == 0);
                    }
                    if ko == 0 {
                        return Some(false);
                    }
                }
            }
        }
        let ci = func.dfg.value_def(c).inst()?;
        let InstructionData::IntCompare { opcode: Opcode::Icmp, cond: cc0, args: [a2, b2] } = func.dfg.insts[ci] else { return None };
        let cc = if arm { cc0 } else { cc0.complement() };
        let a = func.dfg.resolve_aliases(a2);
        let b = func.dfg.resolve_aliases(b2);
        let a = arg_map.get(&a).copied().unwrap_or(a);
        let b = arg_map.get(&b).copied().unwrap_or(b);
        if !func.dfg.value_is_valid(a) || !func.dfg.value_is_valid(b) {
            return None;
        }
        let a = self.resolve_param(func, a);
        let b = self.resolve_param(func, b);
        for (a, b, cc) in [(a, b, cc), (b, a, cc.swap_args())] {
            if a != x {
                continue;
            }
            // Fact `x cc y` settles `x == y` for eq/ne/strict order.
            if b == y {
                return match cc {
                    IntCC::Equal => Some(true),
                    IntCC::NotEqual
                    | IntCC::UnsignedLessThan
                    | IntCC::UnsignedGreaterThan
                    | IntCC::SignedLessThan
                    | IntCC::SignedGreaterThan => Some(false),
                    _ => None,
                };
            }
            // `x cc k` settles `x == y` when `y` is a constant.
            if let (Some(kb), Some(ky)) = (iconst(func, b), iconst(func, y)) {
                return match cc {
                    IntCC::Equal => Some(kb == ky),
                    IntCC::NotEqual
                    | IntCC::UnsignedLessThan
                    | IntCC::UnsignedGreaterThan
                    | IntCC::SignedLessThan
                    | IntCC::SignedGreaterThan => {
                        if kb == ky { Some(false) } else { None }
                    }
                    _ => None,
                };
            }
        }
        None
    }

    /// `x == y` decided by dominating edge facts. Single-pred edges
    /// prove relations on entry to their dest; a multi-pred block's
    /// edges must all agree (the unproven edge would admit anything).
    pub fn known_rel(&mut self, func: &Function, dt: &DominatorTree, x: Value, y: Value, at: Block) -> Option<bool> {
        let x = self.resolve_param(func, x);
        let y = self.resolve_param(func, y);
        if x == y {
            return Some(true);
        }
        let mut b = at;
        let mut hops = 0;
        loop {
            if let Some((c, arm, arg_map)) = self.edge_fact.get(&b) {
                let (c, arm, arg_map) = (*c, *arm, arg_map.clone());
                if let Some(r) = self.edge_rel(func, c, arm, &arg_map, x, y) {
                    return Some(r);
                }
            }
            if let Some(edges) = self.edge_facts_multi.get(&b) {
                let edges = edges.clone();
                let mut verdict = None;
                let mut ok = true;
                for (c, arm, arg_map) in edges.iter() {
                    match self.edge_rel(func, *c, *arm, arg_map, x, y) {
                        Some(v) if verdict.is_none_or(|w| w == v) => verdict = Some(v),
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    return verdict;
                }
            }
            hops += 1;
            if hops > MAX_FACT_HOPS {
                return None;
            }
            match dt.idom(b) {
                Some(i) => b = i,
                None => return None,
            }
            if b == at {
                return None;
            }
        }
    }

    /// Integer bounds `[lo, hi]` on `yb - xb` at `at`: the `at()` ranges
    /// plus every dominating edge compare that mentions the bases.
    fn diff_bounds(&mut self, func: &Function, dt: &DominatorTree, xb: Value, yb: Value, rx: &Range, ry: &Range, at: Block) -> (i128, i128) {
        let mut lo = ry.ulo as i128 - rx.uhi as i128;
        let mut hi = ry.uhi as i128 - rx.ulo as i128;
        let mut b = at;
        let mut hops = 0;
        loop {
            if let Some((c, arm, arg_map)) = self.edge_fact.get(&b) {
                let (c, arm) = (*c, *arm);
                let arg_map = arg_map.clone();
                self.edge_diff(func, dt, c, arm, xb, yb, b, &arg_map, &mut lo, &mut hi);
            }
            if let Some(edges) = self.edge_facts_multi.get(&b) {
                // Per-edge bounds are conditional on the arrival path:
                // `D` at `b` is the union (widening) of them.
                let edges = edges.clone();
                let mut elo = i128::MAX;
                let mut ehi = i128::MIN;
                for (c, arm, arg_map) in edges.iter() {
                    let (mut l, mut h) = (lo, hi);
                    self.edge_diff(func, dt, *c, *arm, xb, yb, b, arg_map, &mut l, &mut h);
                    elo = elo.min(l);
                    ehi = ehi.max(h);
                }
                lo = elo;
                hi = ehi;
            }
            hops += 1;
            if hops > DIFF_HOPS {
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
        (lo, hi)
    }

    /// `x cc y` at `at` decided by correlated facts. `x = xb + kx`,
    /// `y = yb + ky` non-wrapping at `at` makes `x - y` the integer
    /// `(kx - ky) - D` with `D = yb - xb`; unsigned compares are plain
    /// sign tests on that difference. The `D` bounds themselves certify
    /// the strips: `D >= d` gives `xb <= yb - d`, so `kx <= d` cannot
    /// wrap even when `xb`'s own range is full. Signed compares need
    /// signed-range reasoning not done here.
    pub fn implied(&mut self, func: &Function, dt: &DominatorTree, cc: IntCC, x: Value, y: Value, at: Block) -> Option<bool> {
        let x = func.dfg.resolve_aliases(x);
        let y = func.dfg.resolve_aliases(y);
        // eq/ne: edge facts (`x != bound` on every way in, `x == y`
        // proven) decide what the intervals cannot.
        if matches!(cc, IntCC::Equal | IntCC::NotEqual)
            && let Some(eq) = self.known_rel(func, dt, x, y, at)
        {
            return Some(if cc == IntCC::Equal { eq } else { !eq });
        }
        let bits = width(func.dfg.value_type(x))?;
        if Some(bits) != width(func.dfg.value_type(y)) {
            return None;
        }
        let m = umask(bits) as i128;
        let (xb, kx) = strip_add(func, x, STRIP_DEPTH);
        let (yb, ky) = strip_add(func, y, STRIP_DEPTH);
        let (rx, ry) = (self.at(func, dt, xb, at), self.at(func, dt, yb, at));
        let (lo, hi) = if xb == yb {
            (0, 0)
        } else {
            self.diff_bounds(func, dt, xb, yb, &rx, &ry, at)
        };
        // Effective base ranges under the `D` bounds: `xb <= yb - lo`
        // etc. `x = xb + kx` is non-wrapping iff the whole effective
        // interval shifted by `kx` stays in `0..=m`.
        let xlo = (rx.ulo as i128).max(ry.ulo as i128 - hi);
        let xhi = (rx.uhi as i128).min(ry.uhi as i128 - lo);
        let ylo = (ry.ulo as i128).max(rx.ulo as i128 + lo);
        let yhi = (ry.uhi as i128).min(rx.uhi as i128 + hi);
        if xlo + kx < 0 || xhi + kx > m || ylo + ky < 0 || yhi + ky > m {
            return None;
        }
        let c = kx - ky;
        // `x - y ∈ [c - hi, c - lo]` (integer).
        let (rlo, rhi) = (c - hi, c - lo);
        match cc {
            IntCC::UnsignedLessThan => {
                if rhi < 0 {
                    Some(true)
                } else if rlo >= 0 {
                    Some(false)
                } else {
                    None
                }
            }
            IntCC::UnsignedLessThanOrEqual => {
                if rhi <= 0 {
                    Some(true)
                } else if rlo > 0 {
                    Some(false)
                } else {
                    None
                }
            }
            IntCC::UnsignedGreaterThan => {
                if rlo > 0 {
                    Some(true)
                } else if rhi <= 0 {
                    Some(false)
                } else {
                    None
                }
            }
            IntCC::UnsignedGreaterThanOrEqual => {
                if rlo >= 0 {
                    Some(true)
                } else if rhi < 0 {
                    Some(false)
                } else {
                    None
                }
            }
            IntCC::Equal => {
                if rlo == 0 && rhi == 0 {
                    Some(true)
                } else if rlo > 0 || rhi < 0 {
                    Some(false)
                } else {
                    None
                }
            }
            IntCC::NotEqual => {
                if rlo > 0 || rhi < 0 {
                    Some(true)
                } else if rlo == 0 && rhi == 0 {
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        }
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

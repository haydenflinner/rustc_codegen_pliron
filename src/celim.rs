//! Constraint elimination on the final Cranelift IR (`PLIRON_CELIM`).
//!
//! Folds `icmp`s whose result is decided by a condition that held on a
//! dominating edge — the classic `if i < len { a[i]; a[i + 1]; }` case where
//! the second bounds test is implied by the first. This is a superset of
//! jumpthread's `fold_dominated_conds` (`PLIRON_DOMCOND`):
//!
//! - Edge facts propagate into *multi-predecessor* successors when every
//!   other predecessor is dominated by the successor (loop headers
//!   included), not just single-predecessor ones.
//! - Fact operands are translated through the edge's block-arg ->
//!   block-param mapping, so a dominating `x < n` still matches a query on
//!   `n`'s block-param name.
//! - `brif v` on a non-icmp value seeds `v != 0` / `v == 0` facts; `br_table`
//!   seeds `idx == k` facts.
//! - Operand offsets: `x + 1 <= y` is implied by `x < y`, `x - 1 >= y` by
//!   `x > y`, `x <= y` by a dominating `x - 1 < y` (the wrap case would make
//!   the fact false, so it can't occur on the edge), likewise signed.
//! - Facts `x - 1 < k` / `x + 1 > k` tighten the unsigned range of `x`
//!   (likewise the signed range), feeding range-decided queries.
//! - One transitivity hop: `x <= y` and `y <= z` decide `x <= z`.
//! - Signed facts narrow a signed interval; a range confined to one sign
//!   half transfers to the other domain (`x s< 0` gives `x u>= 2^63`, etc).
//!
//! Soundness: a fold only replaces the icmp with the constant the query
//! provably evaluates to, so observable behavior — including which panic
//! edge stays reachable — is unchanged. Dead check blocks are left to the
//! cold-block sinking / dead-code paths already in place.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    Block, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef,
};
use rustc_data_structures::fx::FxHashMap;

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
    (ty.is_int() && !ty.is_vector() && ty.bits() <= 64).then(|| ty.bits())
}

/// Compile-time constant behind `v`, from an `iconst` anywhere.
fn known(func: &Function, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
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

/// An icmp operand: `base + off` with |off| <= 1 (from `iadd`/`isub` with a
/// constant), or a plain constant.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    V(Value, i64),
    K(u64),
}

type Cmp = (IntCC, Op, Op);

/// Decompose `v` into `base + off` for |off| <= 1, else `base + 0`; an
/// `iconst` becomes `K`.
fn decomp(func: &Function, v: Value) -> Op {
    let v = func.dfg.resolve_aliases(v);
    if let Some(k) = known(func, v) {
        return Op::K(k);
    }
    if let Some(i) = func.dfg.value_def(v).inst()
        && let InstructionData::Binary {
            opcode: opcode @ (Opcode::Iadd | Opcode::Isub),
            args,
        } = func.dfg.insts[i]
    {
        let (a, b) = (
            func.dfg.resolve_aliases(args[0]),
            func.dfg.resolve_aliases(args[1]),
        );
        if let Some(k) = known(func, b) {
            let off = if opcode == Opcode::Isub {
                (k as i64).wrapping_neg()
            } else {
                k as i64
            };
            if (-1..=1).contains(&off) {
                return Op::V(a, off);
            }
        } else if opcode == Opcode::Iadd
            && let Some(k) = known(func, a)
            && (-1..=1).contains(&(k as i64))
        {
            return Op::V(b, k as i64);
        }
    }
    Op::V(v, 0)
}

/// `v` without offset peeling: `Op::V(v, 0)` or `K`.
fn plain(func: &Function, v: Value) -> Op {
    let v = func.dfg.resolve_aliases(v);
    match known(func, v) {
        Some(k) => Op::K(k),
        None => Op::V(v, 0),
    }
}

/// A scalar `icmp` as `(cc, x, y)` plus the raw operands; `(x - y) ==/!= 0`
/// becomes `x ==/!= y`. `peel` turns `x +/- 1` operands into `Op::V(x, d)`.
fn norm_icmp(func: &Function, i: Inst, peel: bool) -> Option<(Cmp, (Value, Value))> {
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let raw = (args[0], args[1]);
    let a = func.dfg.resolve_aliases(args[0]);
    if func.dfg.value_type(a).is_vector() {
        return None;
    }
    let b = func.dfg.resolve_aliases(args[1]);
    let op = if peel { decomp } else { plain };
    let (mut x, mut y) = (op(func, a), op(func, b));
    if matches!(cond, IntCC::Equal | IntCC::NotEqual) {
        if x == Op::K(0) {
            std::mem::swap(&mut x, &mut y);
        }
        if y == Op::K(0)
            && let Op::V(v, 0) = x
            && let Some(d) = func.dfg.value_def(v).inst()
            && let InstructionData::Binary {
                opcode: Opcode::Isub,
                args: s,
            } = func.dfg.insts[d]
        {
            // `(x - y) ==/!= 0` is `x ==/!= y`; report the sub's operands
            // as raw so range queries use `x`, not the `x - y` value.
            return Some((
                (cond, op(func, s[0]), op(func, s[1])),
                (func.dfg.resolve_aliases(s[0]), func.dfg.resolve_aliases(s[1])),
            ));
        }
    }
    Some(((cond, x, y), raw))
}

/// The icmp a `brif`/`br_table` condition tests, through `band 1` and
/// extends of its 0/1 result.
fn cond_icmp(func: &Function, v: Value) -> Option<Inst> {
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
                if known(func, b) == Some(1) {
                    v = a;
                } else if known(func, a) == Some(1) {
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

/// Does every path into `s`'s dominance region enter through the `p -> s`
/// edge? True when `s`'s other predecessors are all dominated by `s` (back
/// edges inside the region), so the first entry to `s` is always from `p`.
/// Single-predecessor `s` is the degenerate case.
fn edge_dominates(
    func: &Function,
    domtree: &DominatorTree,
    cfg: &ControlFlowGraph,
    p: Block,
    s: Block,
) -> bool {
    cfg.pred_iter(s)
        .all(|q| q.block == p || domtree.dominates(s, q.block, &func.layout))
}

/// Map `p`'s edge args to `s`'s block params, so a fact about an argument
/// still matches the param name the successor's queries use. Sound only
/// when the param always receives the same value: trivially for a
/// single-predecessor `s`; for a multi-pred `s` (reachable through `p`'s
/// edge and through edges inside `s`'s region) every edge into `s` must
/// pass the same resolved value for that slot, or the param's value on a
/// later visit could differ from the one the fact was proven about.
fn edge_args(
    func: &Function,
    cfg: &ControlFlowGraph,
    inst: Inst,
    s: Block,
) -> FxHashMap<Value, Value> {
    let mut m = FxHashMap::default();
    for bc in
        func.dfg.insts[inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
    {
        if bc.block(&func.dfg.value_lists) == s {
            let params = func.dfg.block_params(s);
            for (i, a) in bc.args(&func.dfg.value_lists).enumerate() {
                if let cranelift_codegen::ir::BlockArg::Value(v) = a {
                    let v = func.dfg.resolve_aliases(v);
                    if i < params.len() && v != params[i] && uniform_slot(func, cfg, s, inst, i, v)
                    {
                        m.insert(v, params[i]);
                    }
                }
            }
        }
    }
    m
}

/// Every edge into `s` passes `v` for param `slot` (the `skip` edge is the
/// one the fact came from). Lets a dominating-edge fact be restated on the
/// destination's param name without breaking on loop re-entry.
fn uniform_slot(
    func: &Function,
    cfg: &ControlFlowGraph,
    s: Block,
    skip: Inst,
    slot: usize,
    v: Value,
) -> bool {
    cfg.pred_iter(s).all(|q| {
        if q.inst == skip {
            return true;
        }
        func.dfg.insts[q.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .filter(|bc| bc.block(&func.dfg.value_lists) == s)
            .all(|bc| {
                bc.args(&func.dfg.value_lists)
                    .nth(slot)
                    .is_some_and(|a| {
                        matches!(a, cranelift_codegen::ir::BlockArg::Value(u)
                            if func.dfg.resolve_aliases(u) == v)
                    })
            })
    })
}

fn sub_op(m: &FxHashMap<Value, Value>, o: Op) -> Op {
    match o {
        Op::V(v, d) => Op::V(*m.get(&v).unwrap_or(&v), d),
        k => k,
    }
}

/// Conditions known on entry to each block whose entry is gated by a
/// dominating edge's branch (`brif`/`br_table`).
fn edge_facts(
    func: &Function,
    cfg: &ControlFlowGraph,
    domtree: &DominatorTree,
) -> FxHashMap<Block, Vec<Cmp>> {
    let mut fact: FxHashMap<Block, Vec<Cmp>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        match func.dfg.insts[t] {
            InstructionData::Brif { arg, blocks, .. } => {
                let (tb, eb) = (
                    blocks[0].block(&func.dfg.value_lists),
                    blocks[1].block(&func.dfg.value_lists),
                );
                if tb == eb {
                    continue;
                }
                let arg = func.dfg.resolve_aliases(arg);
                let icmp = cond_icmp(func, arg).and_then(|i| norm_icmp(func, i, true).map(|q| q.0));
                for &(s, take) in &[(tb, true), (eb, false)] {
                    if !edge_dominates(func, domtree, cfg, b, s) {
                        continue;
                    }
                    let m = edge_args(func, cfg, t, s);
                    let fs = fact.entry(s).or_default();
                    // The branch value itself: `v != 0` on the taken edge.
                    let vc = if take {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    };
                    fs.push((vc, sub_op(&m, plain(func, arg)), Op::K(0)));
                    if let Some((cc, x, y)) = icmp {
                        let cc = if take { cc } else { cc.complement() };
                        fs.push((cc, sub_op(&m, x), sub_op(&m, y)));
                    }
                }
            }
            InstructionData::BranchTable { arg, table, .. } => {
                let jt = &func.dfg.jump_tables[table];
                let arg = func.dfg.resolve_aliases(arg);
                let mut bases = vec![arg];
                // `idx == k` also constrains a `uextend`/`ireduce` source.
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
                        bases.push(x);
                    }
                }
                let def = jt.default_block().block(&func.dfg.value_lists);
                for (k, c) in jt.as_slice().iter().enumerate() {
                    let s = c.block(&func.dfg.value_lists);
                    if s == def {
                        continue;
                    }
                    if jt
                        .as_slice()
                        .iter()
                        .filter(|c| c.block(&func.dfg.value_lists) == s)
                        .count()
                        != 1
                    {
                        continue;
                    }
                    if !edge_dominates(func, domtree, cfg, b, s) {
                        continue;
                    }
                    let m = edge_args(func, cfg, t, s);
                    let fs = fact.entry(s).or_default();
                    for &x in &bases {
                        fs.push((IntCC::Equal, sub_op(&m, plain(func, x)), Op::K(k as u64)));
                    }
                }
            }
            _ => {}
        }
    }
    fact
}

/// Facts holding throughout `b`: those on `b` and its dominators.
fn facts_at<'a>(
    domtree: &DominatorTree,
    fact: &'a FxHashMap<Block, Vec<Cmp>>,
    b: Block,
) -> Vec<&'a Cmp> {
    let mut fs = Vec::new();
    let mut cur = Some(b);
    for _ in 0..64 {
        let Some(c) = cur else { break };
        if let Some(v) = fact.get(&c) {
            fs.extend(v.iter());
        }
        cur = domtree.idom(c);
    }
    fs
}

/// What a true fact `f` says about a query `q` on the same two operands.
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
        Equal => match qc {
            UnsignedLessThanOrEqual
            | UnsignedGreaterThanOrEqual
            | SignedLessThanOrEqual
            | SignedGreaterThanOrEqual => Some(true),
            UnsignedLessThan | UnsignedGreaterThan | SignedLessThan | SignedGreaterThan => {
                Some(false)
            }
            _ => None,
        },
        // Strict facts decide equality and the matching weak compare.
        UnsignedLessThan => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            UnsignedLessThanOrEqual => Some(true),
            _ => None,
        },
        SignedLessThan => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            SignedLessThanOrEqual => Some(true),
            _ => None,
        },
        UnsignedGreaterThan => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            UnsignedGreaterThanOrEqual => Some(true),
            _ => None,
        },
        SignedGreaterThan => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            SignedGreaterThanOrEqual => Some(true),
            _ => None,
        },
        _ => None,
    }
}

/// A const fact `x + d cc k` restated as `x cc k'` when the offset's
/// direction keeps the bound sound: `x - 1 u< k` means `x < k + 1` (the
/// `x = 0` wrap case would make the fact false), and `x + 1 u> k` means
/// `x > k - 1` (the `x = max` wrap case can't hold). Other directions can't
/// merge the wrapped and unwrapped cases into one bound. Signed likewise.
fn shift_k(cc: IntCC, k: u64, d: i64) -> Option<u64> {
    match (cc, d) {
        (IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual, -1) => {
            k.checked_add(1)
        }
        (IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual, 1) => {
            k.checked_sub(1)
        }
        _ if d == 0 => Some(k),
        _ => None,
    }
}

fn shift_ks(cc: IntCC, k: i64, d: i64) -> Option<i64> {
    match (cc, d) {
        (IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual, -1) => k.checked_add(1),
        (IntCC::SignedGreaterThan | IntCC::SignedGreaterThanOrEqual, 1) => k.checked_sub(1),
        _ if d == 0 => Some(k),
        _ => None,
    }
}

/// Unsigned range `[lo, hi]` known for `v` from the dominating facts and
/// one level of structural peeling.
fn urange(func: &Function, fs: &[&Cmp], v: Value, depth: u32) -> Option<(u64, u64)> {
    let v = func.dfg.resolve_aliases(v);
    let w = bits(func, v)?;
    let full = mask(u64::MAX, w);
    let half = 1u64 << (w - 1);
    let (mut lo, mut hi) = (0u64, full);
    for &&(cc, a, b) in fs {
        let (cc, k, d) = match (a, b) {
            (Op::V(x, d), Op::K(k)) if x == v => (cc, shift_k(cc, k, d), d),
            (Op::K(k), Op::V(y, d)) if y == v => {
                let cc = cc.swap_args();
                (cc, shift_k(cc, k, d), d)
            }
            _ => continue,
        };
        // `x - 1 < k`/`x + 1 > k` also rule out the extremes of `x`, even
        // when the shifted bound itself isn't expressible.
        match (cc, d) {
            (IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual, -1) => {
                lo = lo.max(1);
            }
            (IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual, 1) => {
                hi = hi.min(full - 1);
            }
            _ => {}
        }
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
            IntCC::NotEqual => {
                if k == 0 {
                    lo = lo.max(1);
                } else if k == full {
                    hi = hi.min(full - 1);
                }
            }
            // A signed bound on a value known to be one-sided transfers:
            // `x s>= 0` keeps x in [0, half); `x s< 0` puts it in
            // [half, full] (tighter: [half, full + k]).
            IntCC::SignedGreaterThanOrEqual | IntCC::SignedGreaterThan => {
                let sk = sext(k, w);
                let l = if cc == IntCC::SignedGreaterThanOrEqual {
                    sk
                } else {
                    sk.saturating_add(1)
                };
                if l >= 0 {
                    lo = lo.max(l as u64);
                    hi = hi.min(half - 1);
                }
            }
            IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual => {
                let sk = sext(k, w);
                let h = if cc == IntCC::SignedLessThan {
                    sk.saturating_sub(1)
                } else {
                    sk
                };
                if h < 0 {
                    // Signed x <= h < 0: unsigned reps are [half, 2^w + h].
                    lo = lo.max(half);
                    hi = hi.min(full.wrapping_add((h + 1) as u64));
                }
            }
            _ => {}
        }
    }
    if depth < 4
        && let Some(i) = func.dfg.value_def(v).inst()
    {
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } => {
                if let Some(aw) = bits(func, arg) {
                    hi = hi.min(mask(u64::MAX, aw));
                    if let Some((_, h)) = urange(func, fs, arg, depth + 1) {
                        hi = hi.min(h);
                    }
                }
            }
            InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg,
            } => {
                if let Some((l, h)) = urange(func, fs, arg, depth + 1)
                    && h <= full
                {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Binary {
                opcode: opcode @ (Opcode::Iadd | Opcode::Isub),
                args,
            } => {
                let (x, y) = (
                    func.dfg.resolve_aliases(args[0]),
                    func.dfg.resolve_aliases(args[1]),
                );
                if let Some(k) = known(func, y)
                    && let Some((l, h)) = urange(func, fs, x, depth + 1)
                {
                    let (l2, h2) = if opcode == Opcode::Isub {
                        (l.wrapping_sub(k), h.wrapping_sub(k))
                    } else {
                        (l.wrapping_add(k), h.wrapping_add(k))
                    };
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
                if opcode == Opcode::Isub
                    && let Some(k) = known(func, x)
                    && let Some((l, h)) = urange(func, fs, y, depth + 1)
                    && h <= k
                {
                    lo = lo.max(k - h);
                    hi = hi.min(k - l);
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => {
                if let Some(m) = known(func, args[1]).or_else(|| known(func, args[0])) {
                    hi = hi.min(m);
                }
            }
            InstructionData::Binary {
                opcode: Opcode::Ushr,
                args,
            } => {
                if let Some(s) = known(func, args[1])
                    && s < u64::from(w)
                {
                    hi = hi.min(full >> s);
                    if let Some((l, h)) = urange(func, fs, args[0], depth + 1) {
                        lo = lo.max(l >> s);
                        hi = hi.min(h >> s);
                    }
                }
            }
            _ => {}
        }
    }
    (lo <= hi && (lo, hi) != (0, full)).then_some((lo, hi))
}

/// Signed range `[lo, hi]` (as i64 math on `w` bits) from signed facts,
/// plus transfer from the unsigned range when it fits one sign half.
fn srange(func: &Function, fs: &[&Cmp], v: Value, depth: u32) -> Option<(i64, i64)> {
    let v = func.dfg.resolve_aliases(v);
    let w = bits(func, v)?;
    let (smin, smax) = if w >= 64 {
        (i64::MIN, i64::MAX)
    } else {
        (-(1i64 << (w - 1)), (1i64 << (w - 1)) - 1)
    };
    let (mut lo, mut hi) = (smin, smax);
    for &&(cc, a, b) in fs {
        let (cc, k, d) = match (a, b) {
            (Op::V(x, d), Op::K(k)) if x == v => (cc, shift_ks(cc, sext(k, w), d), d),
            (Op::K(k), Op::V(y, d)) if y == v => {
                let cc = cc.swap_args();
                (cc, shift_ks(cc, sext(k, w), d), d)
            }
            _ => continue,
        };
        // `x - 1 s< k`/`x + 1 s> k` rule out the signed extremes of `x`.
        match (cc, d) {
            (IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual, -1) => {
                lo = lo.max(smin + 1);
            }
            (IntCC::SignedGreaterThan | IntCC::SignedGreaterThanOrEqual, 1) => {
                hi = hi.min(smax - 1);
            }
            _ => {}
        }
        let Some(k) = k else { continue };
        match cc {
            IntCC::SignedLessThan if k > smin => hi = hi.min(k - 1),
            IntCC::SignedLessThanOrEqual => hi = hi.min(k),
            IntCC::SignedGreaterThan if k < smax => lo = lo.max(k + 1),
            IntCC::SignedGreaterThanOrEqual => lo = lo.max(k),
            IntCC::Equal => {
                lo = lo.max(k);
                hi = hi.min(k);
            }
            // `x u<= k` with `k` in the low half keeps the signed value in
            // [0, k].
            IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual
                if k >= 0 && (k as u64) < (1u64 << (w - 1)) =>
            {
                let k = k as u64;
                let h = if cc == IntCC::UnsignedLessThan && k > 0 {
                    k - 1
                } else {
                    k
                };
                lo = lo.max(0);
                hi = hi.min(h as i64);
            }
            _ => {}
        }
    }
    if depth < 4
        && let Some(i) = func.dfg.value_def(v).inst()
    {
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Sextend,
                arg,
            } => {
                if let Some((l, h)) = srange(func, fs, arg, depth + 1) {
                    lo = lo.max(l);
                    hi = hi.min(h);
                }
            }
            InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } => {
                if let Some(aw) = bits(func, arg) {
                    let afull = mask(u64::MAX, aw);
                    if afull <= smax as u64 {
                        lo = lo.max(0);
                        hi = hi.min(afull as i64);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some((ul, uh)) = urange(func, fs, v, depth + 1) {
        let half = 1u64 << (w - 1);
        if uh < half {
            lo = lo.max(ul as i64);
            hi = hi.min(uh as i64);
        } else if ul >= half {
            // Signed view of [ul, uh] when the whole range is negative.
            let conv = |x: u64| (x as i64).wrapping_sub(if w < 64 { 1i64 << w } else { 0 });
            lo = lo.max(conv(ul));
            hi = hi.min(conv(uh));
        }
    }
    (lo <= hi && (lo, hi) != (smin, smax)).then_some((lo, hi))
}

/// Decide `x cc k` (`k cc x` when `swapped`) from an unsigned `[lo, hi]`.
fn ucmp(cc: IntCC, lo: u64, hi: u64, k: u64, swapped: bool) -> Option<bool> {
    let cc = if swapped { cc.swap_args() } else { cc };
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
        IntCC::UnsignedLessThan => dec(hi < k, lo >= k),
        IntCC::UnsignedLessThanOrEqual => dec(hi <= k, lo > k),
        IntCC::UnsignedGreaterThan => dec(lo > k, hi <= k),
        IntCC::UnsignedGreaterThanOrEqual => dec(lo >= k, hi < k),
        IntCC::Equal => dec(lo == k && hi == k, k < lo || k > hi),
        IntCC::NotEqual => dec(k < lo || k > hi, lo == k && hi == k),
        _ => None,
    }
}

/// Decide `x cc k` (`k cc x` when `swapped`) from a signed `[lo, hi]`.
fn scmp(cc: IntCC, lo: i64, hi: i64, k: i64, swapped: bool) -> Option<bool> {
    let cc = if swapped { cc.swap_args() } else { cc };
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
        IntCC::SignedLessThan => dec(hi < k, lo >= k),
        IntCC::SignedLessThanOrEqual => dec(hi <= k, lo > k),
        IntCC::SignedGreaterThan => dec(lo > k, hi <= k),
        IntCC::SignedGreaterThanOrEqual => dec(lo >= k, hi < k),
        IntCC::Equal => dec(lo == k && hi == k, k < lo || k > hi),
        IntCC::NotEqual => dec(k < lo || k > hi, lo == k && hi == k),
        _ => None,
    }
}

/// `(x + qa) qc (y + qb)` decided by the zero-offset fact `x fc y`.
/// Only directions where the query's add can't flip the result appear: the
/// wrap case would contradict the fact (e.g. `x < y` forces `x <= max - 1`,
/// so `x + 1` can't wrap).
fn off_implied(fc: IntCC, qa: i64, qb: i64, qc: IntCC) -> Option<bool> {
    use IntCC::*;
    if qa == 0 && qb == 0 {
        return None;
    }
    match (fc, qa, qb) {
        // `x < y` gives `x + 1 <= y` and `x <= y - 1` (y >= 1 on the edge).
        (UnsignedLessThan, 1, 0) | (UnsignedLessThan, 0, -1) => match qc {
            UnsignedLessThanOrEqual => Some(true),
            UnsignedGreaterThanOrEqual | UnsignedGreaterThan => Some(false),
            _ => None,
        },
        // `x > y` gives `x - 1 >= y` and `x >= y + 1`.
        (UnsignedGreaterThan, -1, 0) | (UnsignedGreaterThan, 0, 1) => match qc {
            UnsignedGreaterThanOrEqual => Some(true),
            UnsignedLessThan | UnsignedLessThanOrEqual => Some(false),
            _ => None,
        },
        // `x == y` decides `x +/- 1` vs `y` and `x` vs `y +/- 1`: if the
        // query add wrapped, x/y was at the limit and the results differ.
        (Equal, 1 | -1, 0) | (Equal, 0, 1 | -1) => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            _ => None,
        },
        // Signed mirrors.
        (SignedLessThan, 1, 0) | (SignedLessThan, 0, -1) => match qc {
            SignedLessThanOrEqual => Some(true),
            SignedGreaterThanOrEqual | SignedGreaterThan => Some(false),
            _ => None,
        },
        (SignedGreaterThan, -1, 0) | (SignedGreaterThan, 0, 1) => match qc {
            SignedGreaterThanOrEqual => Some(true),
            SignedLessThan | SignedLessThanOrEqual => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `(x + fa) fc (y + fb)` deciding a zero-offset query `x qc y`. Only
/// decompositions where the wrap case contradicts the fact are sound:
/// `x - 1 u< y` forces `x != 0`, giving `x <= y`; `x + 1 u> y` forces
/// `x != max`, giving `x >= y`; `x u< y + 1` forces `y != max`, giving
/// `x <= y`; `x u> y - 1` forces `y != 0`, giving `x >= y`. Equality
/// offsets decide `x ==/!= y` directly.
fn fact_off_implied(fc: IntCC, fa: i64, fb: i64, qc: IntCC) -> Option<bool> {
    use IntCC::*;
    match (fc, fa, fb) {
        (UnsignedLessThan, -1, 0) | (UnsignedLessThan, 0, 1) => match qc {
            UnsignedLessThanOrEqual => Some(true),
            UnsignedGreaterThan => Some(false),
            _ => None,
        },
        (UnsignedGreaterThan, 1, 0) | (UnsignedGreaterThan, 0, -1) => match qc {
            UnsignedGreaterThanOrEqual => Some(true),
            UnsignedLessThan => Some(false),
            _ => None,
        },
        (SignedLessThan, -1, 0) | (SignedLessThan, 0, 1) => match qc {
            SignedLessThanOrEqual => Some(true),
            SignedGreaterThan => Some(false),
            _ => None,
        },
        (SignedGreaterThan, 1, 0) | (SignedGreaterThan, 0, -1) => match qc {
            SignedGreaterThanOrEqual => Some(true),
            SignedLessThan => Some(false),
            _ => None,
        },
        (Equal, 1 | -1, 0) | (Equal, 0, 1 | -1) => match qc {
            NotEqual => Some(true),
            Equal => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// One transitivity hop: `x R1 m` and `m R2 z` give `x R3 z`.
fn transit(r1: IntCC, r2: IntCC) -> Option<IntCC> {
    use IntCC::*;
    if r1 == Equal {
        return Some(r2);
    }
    if r2 == Equal {
        return Some(r1);
    }
    // (direction family, strict): lt is 0, gt is 1; signed adds 2.
    let fam = |r: IntCC| match r {
        UnsignedLessThanOrEqual => Some((0, false)),
        UnsignedLessThan => Some((0, true)),
        UnsignedGreaterThanOrEqual => Some((1, false)),
        UnsignedGreaterThan => Some((1, true)),
        SignedLessThanOrEqual => Some((2, false)),
        SignedLessThan => Some((2, true)),
        SignedGreaterThanOrEqual => Some((3, false)),
        SignedGreaterThan => Some((3, true)),
        _ => None,
    };
    let cc = |d: u8, s: bool| match (d, s) {
        (0, false) => UnsignedLessThanOrEqual,
        (0, true) => UnsignedLessThan,
        (1, false) => UnsignedGreaterThanOrEqual,
        (1, true) => UnsignedGreaterThan,
        (2, false) => SignedLessThanOrEqual,
        (2, true) => SignedLessThan,
        (3, false) => SignedGreaterThanOrEqual,
        (3, true) => SignedGreaterThan,
        _ => unreachable!(),
    };
    let (d1, s1) = fam(r1)?;
    let (d2, s2) = fam(r2)?;
    (d1 == d2).then(|| cc(d1, s1 || s2))
}

/// Decide the query `q` from the dominating facts `fs`.
fn decide(func: &Function, fs: &[&Cmp], q: Cmp, raw: (Value, Value)) -> Option<bool> {
    // `x == y` facts let the query match on either name.
    let eqs: Vec<(Value, Value)> = fs
        .iter()
        .filter_map(|&&(c, a, b)| match (c, a, b) {
            (IntCC::Equal, Op::V(x, 0), Op::V(y, 0)) => Some((x, y)),
            _ => None,
        })
        .collect();
    let alts = |o: Op| -> Vec<Op> {
        let mut a = vec![o];
        if let Op::V(v, 0) = o {
            for &(x, y) in &eqs {
                if x == v {
                    a.push(Op::V(y, 0));
                } else if y == v {
                    a.push(Op::V(x, 0));
                }
            }
        }
        a
    };
    let decide1 = |q: Cmp| -> Option<bool> {
        for &&f in fs {
            if let Some(r) = implied(f, q) {
                return Some(r);
            }
            if let (Op::V(fx, fd1), Op::V(fy, fd2)) = (f.1, f.2)
                && let (Op::V(qx, qd1), Op::V(qy, qd2)) = (q.1, q.2)
            {
                if (fx, fy) == (qx, qy) {
                    // Query offsets relative to the fact's frame.
                    if let Some(r) = off_implied(f.0, qd1 - fd1, qd2 - fd2, q.0) {
                        return Some(r);
                    }
                    if let Some(r) = fact_off_implied(f.0, fd1 - qd1, fd2 - qd2, q.0) {
                        return Some(r);
                    }
                } else if (fx, fy) == (qy, qx) {
                    let cc = f.0.swap_args();
                    if let Some(r) = off_implied(cc, qd2 - fd2, qd1 - fd1, q.0.swap_args()) {
                        return Some(r);
                    }
                    if let Some(r) =
                        fact_off_implied(cc, fd2 - qd2, fd1 - qd1, q.0.swap_args())
                    {
                        return Some(r);
                    }
                }
            }
        }
        // Transitivity: `x R1 m` and `m R2 z` decide `x R3 z` (z may be a
        // constant on the query's right).
        if let Op::V(x, 0) = q.1 {
            for &&f1 in fs {
                let Some((r1, m)) = (match (f1.1, f1.2) {
                    (Op::V(a, 0), Op::V(m, 0)) if a == x => Some((f1.0, m)),
                    (Op::V(m, 0), Op::V(a, 0)) if a == x => Some((f1.0.swap_args(), m)),
                    _ => None,
                }) else {
                    continue;
                };
                for &&f2 in fs {
                    let r2 = match (f2.1, f2.2) {
                        (Op::V(a, 0), b) if a == m && b == q.2 => Some(f2.0),
                        (b, Op::V(a, 0)) if a == m && b == q.2 => Some(f2.0.swap_args()),
                        _ => None,
                    };
                    if let Some(r3) = r2.and_then(|r2| transit(r1, r2))
                        && let Some(r) = implied((r3, q.1, q.2), q)
                    {
                        return Some(r);
                    }
                }
            }
        }
        None
    };
    for a in alts(q.1) {
        for b in alts(q.2) {
            if let Some(r) = decide1((q.0, a, b)) {
                return Some(r);
            }
        }
    }
    // Constant or range decision on `x cc k`.
    let w = bits(func, raw.0)?;
    match (q.1, q.2) {
        (Op::K(a), Op::K(b)) => {
            let (sa, sb) = (sext(a, w), sext(b, w));
            Some(match q.0 {
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
            })
        }
        (Op::V(_, _), Op::K(k)) | (Op::K(k), Op::V(_, _)) => {
            let (rv, swapped) = if matches!(q.1, Op::V(_, _)) {
                (func.dfg.resolve_aliases(raw.0), false)
            } else {
                (func.dfg.resolve_aliases(raw.1), true)
            };
            if let Some((l, h)) = urange(func, fs, rv, 0) {
                // A signed cc on a low-half range behaves as unsigned.
                let half = 1u64 << (w - 1);
                let cc = if h < half && k < half {
                    match q.0 {
                        IntCC::SignedLessThan => IntCC::UnsignedLessThan,
                        IntCC::SignedLessThanOrEqual => IntCC::UnsignedLessThanOrEqual,
                        IntCC::SignedGreaterThan => IntCC::UnsignedGreaterThan,
                        IntCC::SignedGreaterThanOrEqual => IntCC::UnsignedGreaterThanOrEqual,
                        c => c,
                    }
                } else {
                    q.0
                };
                if let Some(r) = ucmp(cc, l, h, k, swapped) {
                    return Some(r);
                }
            }
            if let Some((l, h)) = srange(func, fs, rv, 0)
                && let Some(r) = scmp(q.0, l, h, sext(k, w), swapped)
            {
                return Some(r);
            }
            None
        }
        _ => None,
    }
}

/// Fold `icmp`s decided by dominating branch conditions; returns the folds.
pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    let fact = edge_facts(func, &cfg, &domtree);
    if fact.is_empty() {
        return 0;
    }
    let mut folds: Vec<(Inst, bool)> = Vec::new();
    for b in func.layout.blocks() {
        let fs = facts_at(&domtree, &fact, b);
        if fs.is_empty() {
            continue;
        }
        for i in func.layout.block_insts(b) {
            let Some((q, raw)) = norm_icmp(func, i, true) else {
                continue;
            };
            if let Some(k) = decide(func, &fs, q, raw) {
                folds.push((i, k));
            }
        }
    }
    for &(i, k) in &folds {
        let ty = func.dfg.value_type(func.dfg.first_result(i));
        func.replace(i).iconst(ty, i64::from(k));
    }
    if !folds.is_empty() {
        // A folded check leaves a constant `brif`; retarget it so the dead
        // successor's edge args drop away for the remaining passes.
        crate::jumpthread::fold_const_branches(func);
    }
    folds.len()
}

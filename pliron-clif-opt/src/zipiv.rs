//! Lockstep induction variables (`PLIRON_ZIPIV=1`).
//!
//! `a.iter().zip(b)`, `Iterator::cmp` and slice equality over two slices
//! of one length walk two pointers that step by the same amount: `q - p`
//! is loop-invariant, so `q == Q` holds exactly when `p == Q - (q - p)`.
//! The loop still tests both ends every iteration — `Big::cmp`'s reverse
//! zip checks `b == b_start` *and* `a == a_start` per limb, plus a
//! `select` keeping `b`'s pointer when it is exhausted (LLVM's
//! IndVarSimplify sees through it: one compare + one exit test per limb).
//!
//! Per loop header, two params `p`, `q` with the same constant step on
//! every back edge and an entry difference `D = q0 - p0` (equal across
//! entry edges, compared as linear forms over `iadd`/`isub`/`imul`/`ishl`
//! by constants, through single-source block params):
//!
//! 1. `icmp eq|ne q, Q` (`Q` invariant) becomes `icmp eq|ne p, P` when
//!    `Q - D` is, as a linear form, a value `P` that dominates the header.
//! 2. `icmp eq|ne x, y` in the loop folds when `x != y` is known at its
//!    block: a dominating `brif` arm implies it (the arm's block has that
//!    branch as its only predecessor), or `x` is a header param every
//!    incoming edge's own branch proves `!= y`. `brif`s and `select`s on
//!    folded values fold with them.
//!
//! `q`'s step may sit behind `select(c, q, q + s)` (an exhausted side
//! keeping its pointer): by induction it is `q + s` whenever `c` is a
//! `q`-test whose step-1 image is known false there. That is checked
//! before anything is rewritten, since step 1's soundness rests on it.
//!
//! Only `eq`/`ne` move: an ordered compare is not offset-invariant.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::vmap::VMap;

const MAX_LOOPS: usize = 64;
const MAX_DEPTH: usize = 8;

/// `k + sum(coef * leaf)`, leaves sorted by index.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Lin {
    k: i64,
    t: Vec<(Value, i64)>,
}

impl Lin {
    fn leaf(v: Value) -> Lin {
        Lin {
            k: 0,
            t: vec![(v, 1)],
        }
    }
    fn konst(k: i64) -> Lin {
        Lin { k, t: Vec::new() }
    }
    fn add(&self, o: &Lin, sign: i64) -> Lin {
        let mut m: FxHashMap<Value, i64> = self.t.iter().copied().collect();
        for &(v, c) in &o.t {
            let e = m.entry(v).or_default();
            *e = e.wrapping_add(c.wrapping_mul(sign));
        }
        let mut t: Vec<(Value, i64)> = m.into_iter().filter(|&(_, c)| c != 0).collect();
        t.sort_by_key(|&(v, _)| v.as_u32());
        Lin {
            k: self.k.wrapping_add(o.k.wrapping_mul(sign)),
            t,
        }
    }
    fn scale(&self, s: i64) -> Lin {
        let mut t: Vec<(Value, i64)> = self
            .t
            .iter()
            .map(|&(v, c)| (v, c.wrapping_mul(s)))
            .filter(|&(_, c)| c != 0)
            .collect();
        t.sort_by_key(|&(v, _)| v.as_u32());
        Lin {
            k: self.k.wrapping_mul(s),
            t,
        }
    }
    /// Equal modulo `bits`-bit wrapping.
    fn eq_mod(&self, o: &Lin, bits: u32) -> bool {
        let m = |x: i64| if bits >= 64 { x } else { x & ((1i64 << bits) - 1) };
        m(self.k) == m(o.k)
            && self.t.len() == o.t.len()
            && self.t.iter().zip(&o.t).all(|(a, b)| a.0 == b.0 && m(a.1) == m(b.1))
    }
    /// The constant this is, modulo `bits`-bit wrapping.
    fn as_const(&self, bits: u32) -> Option<i64> {
        let m = |x: i64| if bits >= 64 { x } else { x & ((1i64 << bits) - 1) };
        self.t.iter().all(|&(_, c)| m(c) == 0).then_some(self.k)
    }
}

struct Ctx<'a> {
    ps: &'a VMap<Value>,
    none: FxHashSet<Value>,
}

impl Ctx<'_> {
    /// The value `v` provably always equals (aliases + single-source params).
    fn canon(&self, func: &Function, v: Value) -> Value {
        crate::loadfwd::psrc(self.ps, &self.none, func, v)
    }

    fn iconst(&self, func: &Function, v: Value) -> Option<i64> {
        let v = self.canon(func, v);
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits()),
            _ => None,
        }
    }

    fn lin(&self, func: &Function, v: Value, d: usize) -> Lin {
        let v = self.canon(func, v);
        if let Some(k) = self.iconst(func, v) {
            return Lin::konst(k);
        }
        if d == 0 {
            return Lin::leaf(v);
        }
        let Some(i) = func.dfg.value_def(v).inst() else {
            return Lin::leaf(v);
        };
        let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
            return Lin::leaf(v);
        };
        let l = |x: Value| self.lin(func, x, d - 1);
        match opcode {
            Opcode::Iadd => l(args[0]).add(&l(args[1]), 1),
            Opcode::Isub => l(args[0]).add(&l(args[1]), -1),
            Opcode::Imul => match (self.iconst(func, args[0]), self.iconst(func, args[1])) {
                (_, Some(k)) => l(args[0]).scale(k),
                (Some(k), None) => l(args[1]).scale(k),
                _ => Lin::leaf(v),
            },
            Opcode::Ishl => match self.iconst(func, args[1]) {
                Some(k) if (0..32).contains(&k) => l(args[0]).scale(1i64 << k),
                _ => Lin::leaf(v),
            },
            _ => Lin::leaf(v),
        }
    }

    /// `icmp eq|ne` operands (canonical) and whether it is `eq`.
    fn eqne(&self, func: &Function, c: Value) -> Option<(Value, Value, bool)> {
        let i = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst()?;
        match func.dfg.insts[i] {
            InstructionData::IntCompare { cond, args, .. }
                if matches!(cond, IntCC::Equal | IntCC::NotEqual) =>
            {
                Some((
                    self.canon(func, args[0]),
                    self.canon(func, args[1]),
                    cond == IntCC::Equal,
                ))
            }
            _ => None,
        }
    }
}

fn key(x: Value, y: Value) -> (Value, Value) {
    if x.as_u32() <= y.as_u32() { (x, y) } else { (y, x) }
}

/// `x != y` facts keyed by the unordered pair: each holds in every block
/// the listed block dominates (its single predecessor is a `brif` arm
/// that implies it).
fn ne_facts(
    func: &Function,
    cfg: &ControlFlowGraph,
    cx: &Ctx,
) -> FxHashMap<(Value, Value), Vec<Block>> {
    let mut out: FxHashMap<(Value, Value), Vec<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[t] else {
            continue;
        };
        let Some((x, y, is_eq)) = cx.eqne(func, arg) else {
            continue;
        };
        // `x != y` on the false arm of `eq`, the true arm of `ne`.
        let s = blocks[usize::from(is_eq)].block(&func.dfg.value_lists);
        let other = blocks[usize::from(!is_eq)].block(&func.dfg.value_lists);
        if s == other || cfg.pred_iter(s).count() != 1 {
            continue;
        }
        out.entry(key(x, y)).or_default().push(s);
    }
    out
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// An edge into the header: (branch inst, canonical args, `Some(then?)`
/// for a `brif`, from inside the loop?).
type Edge = (Inst, Vec<Value>, Option<bool>, bool);

fn edge_args(
    func: &Function,
    cx: &Ctx,
    inst: Inst,
    dst: Block,
) -> Option<(Vec<Value>, Option<bool>)> {
    let mut out = None;
    let dests =
        func.dfg.insts[inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
    let is_brif = matches!(func.dfg.insts[inst], InstructionData::Brif { .. });
    for (k, bc) in dests.iter().enumerate() {
        if bc.block(&func.dfg.value_lists) != dst {
            continue;
        }
        if out.is_some() {
            return None;
        }
        let mut vs = Vec::new();
        for a in bc.args(&func.dfg.value_lists) {
            match a {
                BlockArg::Value(v) => vs.push(cx.canon(func, v)),
                _ => return None,
            }
        }
        out = Some((vs, is_brif.then_some(k == 0)));
    }
    out
}

/// One sweep; `.1` is whether a CFG edge changed (the analyses are then
/// stale and the caller must recompute them before sweeping again).
pub fn run(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
) -> (usize, bool) {
    let ps = crate::proots::alias_map_cfg(func, cfg);
    let cx = Ctx {
        ps: &ps,
        none: FxHashSet::default(),
    };
    let facts = ne_facts(func, cfg, &cx);
    // Blocks per loop (nested loops' blocks included), one layout walk.
    let loops: Vec<Loop> = la.loops().take(MAX_LOOPS).collect();
    let mut members: FxHashMap<Loop, Vec<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let mut l = la.innermost_loop(b);
        while let Some(x) = l {
            members.entry(x).or_default().push(b);
            l = la.loop_parent(x);
        }
    }
    let mut n = 0;
    for lp in loops {
        let blocks = members.remove(&lp).unwrap_or_default();
        let (k, cfg_changed) = run_loop(func, cfg, dt, la, lp, &cx, &facts, blocks);
        n += k;
        // A folded `brif` changed the CFG: the analyses are stale.
        if cfg_changed {
            return (n, true);
        }
    }
    (n, false)
}

struct Lp<'a> {
    h: Block,
    params: Vec<Value>,
    edges: Vec<Edge>,
    blocks: FxHashSet<Block>,
    order: Vec<Block>,
    facts: &'a FxHashMap<(Value, Value), Vec<Block>>,
}

impl Lp<'_> {
    /// `x != y` is known at block `at`.
    fn known_ne(&self, func: &Function, cx: &Ctx, dt: &DominatorTree, x: Value, y: Value, at: Block) -> bool {
        if self
            .facts
            .get(&key(x, y))
            .is_some_and(|ss| ss.iter().any(|&s| dt.dominates(s, at, &func.layout)))
        {
            return true;
        }
        self.params.iter().enumerate().any(|(pi, &pp)| {
            let other = if pp == x {
                y
            } else if pp == y {
                x
            } else {
                return false;
            };
            self.edges.iter().all(|(inst, args, arm, _)| {
                let Some(then) = *arm else { return false };
                let InstructionData::Brif { arg, .. } = func.dfg.insts[*inst] else {
                    return false;
                };
                let Some((a, b, is_eq)) = cx.eqne(func, arg) else {
                    return false;
                };
                key(a, b) == key(args[pi], other) && then != is_eq
            })
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    cx: &Ctx,
    facts: &FxHashMap<(Value, Value), Vec<Block>>,
    order: Vec<Block>,
) -> (usize, bool) {
    let h = la.loop_header(lp);
    let params: Vec<Value> = func.dfg.block_params(h).to_vec();
    if params.len() < 2 {
        return (0, false);
    }
    let mut edges = Vec::new();
    for p in cfg.pred_iter(h) {
        let Some((args, arm)) = edge_args(func, cx, p.inst, h) else {
            return (0, false);
        };
        if args.len() != params.len() {
            return (0, false);
        }
        edges.push((p.inst, args, arm, la.is_in_loop(p.block, lp)));
    }
    if !edges.iter().any(|e| e.3) || !edges.iter().any(|e| !e.3) {
        return (0, false);
    }
    let lpi = Lp {
        h,
        params: params.clone(),
        edges,
        blocks: order.iter().copied().collect(),
        order,
        facts,
    };
    // Back-edge step per param: a constant `s`, possibly behind
    // `select(c, x, x + s)`-style picks that hold only while `c` takes the
    // recorded value: (s, [(c, needed value, select's block)]).
    type Step = (i64, Vec<(Value, bool, Block)>);
    let step = |i: usize| -> Option<Step> {
        let mut s = None;
        let mut conds = Vec::new();
        let pv = Lin::leaf(params[i]);
        let bits = func.dfg.value_type(params[i]).bits();
        for e in lpi.edges.iter().filter(|e| e.3) {
            let a = e.1[i];
            let d = cx.lin(func, a, MAX_DEPTH).add(&pv, -1);
            let k = if let Some(k) = d.as_const(bits) {
                k
            } else {
                let di = func.dfg.value_def(a).inst()?;
                let InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } = func.dfg.insts[di]
                else {
                    return None;
                };
                let dx = cx.lin(func, args[1], MAX_DEPTH).add(&pv, -1).as_const(bits)?;
                let dy = cx.lin(func, args[2], MAX_DEPTH).add(&pv, -1).as_const(bits)?;
                let c = func.dfg.resolve_aliases(args[0]);
                let at = func.layout.inst_block(di)?;
                match (dx, dy) {
                    (0, k) if k != 0 => conds.push((c, false, at)),
                    (k, 0) if k != 0 => conds.push((c, true, at)),
                    _ => return None,
                }
                if dx != 0 { dx } else { dy }
            };
            match s {
                None => s = Some(k),
                Some(x) if x == k => {}
                _ => return None,
            }
        }
        s.filter(|&k| k != 0).map(|k| (k, conds))
    };
    let steps: Vec<Option<Step>> = (0..params.len()).map(step).collect();
    let mut n = 0;
    for qi in 0..params.len() {
        let Some((sq, qconds)) = steps[qi].clone() else {
            continue;
        };
        let q = params[qi];
        let ty = func.dfg.value_type(q);
        if !ty.is_int() || ty.bits() > 64 {
            continue;
        }
        for pi in 0..params.len() {
            // `p` steps unconditionally; its proofs never involve `q`.
            if pi == qi
                || steps[pi]
                    .as_ref()
                    .is_none_or(|(sp, c)| *sp != sq || !c.is_empty())
                || func.dfg.value_type(params[pi]) != ty
            {
                continue;
            }
            let p = params[pi];
            // D = q0 - p0, identical on every entry edge.
            let mut dd: Option<Lin> = None;
            let mut ok = true;
            for e in lpi.edges.iter().filter(|e| !e.3) {
                let d = cx
                    .lin(func, e.1[qi], MAX_DEPTH)
                    .add(&cx.lin(func, e.1[pi], MAX_DEPTH), -1);
                match &dd {
                    None => dd = Some(d),
                    Some(x) if x.eq_mod(&d, ty.bits()) => {}
                    _ => ok = false,
                }
            }
            let (true, Some(dd)) = (ok, dd) else {
                continue;
            };
            // A conditional step holds by induction iff each pick's `c` is
            // a `q` test whose image `p ==/!= P` is known `p != P` there.
            let picks_ok = qconds.iter().all(|&(c, need, at)| {
                let Some((x, y, is_eq)) = cx.eqne(func, c) else {
                    return false;
                };
                let qq = if x == q {
                    y
                } else if y == q {
                    x
                } else {
                    return false;
                };
                let Some(pv) = image(func, cx, dt, &lpi, qq, &dd, ty.bits(), p) else {
                    return false;
                };
                lpi.known_ne(func, cx, dt, p, pv, at) && need == !is_eq
            });
            if !picks_ok {
                continue;
            }
            n += rewrite(func, cx, dt, &lpi, q, p, &dd, ty.bits());
        }
    }
    let mut folded = 0;
    if n > 0 {
        folded = fold_known(func, cx, dt, &lpi);
    }
    (n + folded, folded > 0)
}

/// `P ≡ Q - D` as an existing value dominating the header (no new
/// arithmetic), for an invariant `Q`.
#[allow(clippy::too_many_arguments)]
fn image(
    func: &Function,
    cx: &Ctx,
    dt: &DominatorTree,
    lpi: &Lp,
    qq: Value,
    dd: &Lin,
    bits: u32,
    p: Value,
) -> Option<Value> {
    if def_block(func, qq).is_none_or(|db| lpi.blocks.contains(&db)) {
        return None;
    }
    let t = cx.lin(func, qq, MAX_DEPTH).add(dd, -1);
    let [(pv, 1)] = t.t[..] else {
        return None;
    };
    (Lin::leaf(pv).eq_mod(&t, bits)
        && func.dfg.value_type(pv) == func.dfg.value_type(p)
        && def_block(func, pv).is_some_and(|db| dt.dominates(db, lpi.h, &func.layout)))
    .then_some(pv)
}

/// Step 1: `icmp eq|ne q, Q` → `icmp eq|ne p, P` for `P ≡ Q - D`.
#[allow(clippy::too_many_arguments)]
fn rewrite(
    func: &mut Function,
    cx: &Ctx,
    dt: &DominatorTree,
    lpi: &Lp,
    q: Value,
    p: Value,
    dd: &Lin,
    bits: u32,
) -> usize {
    let mut n = 0;
    for &b in &lpi.order {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] else {
                continue;
            };
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
                continue;
            }
            let (a0, a1) = (cx.canon(func, args[0]), cx.canon(func, args[1]));
            let qq = if a0 == q {
                a1
            } else if a1 == q {
                a0
            } else {
                continue;
            };
            let Some(pv) = image(func, cx, dt, lpi, qq, dd, bits, p) else {
                continue;
            };
            if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[i] {
                *args = [p, pv];
            }
            n += 1;
        }
    }
    n
}

/// Step 2: fold `icmp eq|ne` in the loop whose operands are known unequal
/// at its block, then the `brif`s and `select`s consuming them.
fn fold_known(func: &mut Function, cx: &Ctx, dt: &DominatorTree, lpi: &Lp) -> usize {
    let mut known: Vec<(Inst, bool)> = Vec::new();
    for &b in &lpi.order {
        for i in func.layout.block_insts(b) {
            let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] else {
                continue;
            };
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
                continue;
            }
            let (a0, a1) = (cx.canon(func, args[0]), cx.canon(func, args[1]));
            if lpi.known_ne(func, cx, dt, a0, a1, b) {
                known.push((i, cond == IntCC::NotEqual));
            }
        }
    }
    let mut n = 0;
    for (i, val) in known {
        let v = func.dfg.first_result(i);
        let mut cur = FuncCursor::new(func).at_inst(i);
        let c = cur.ins().iconst(types::I8, i64::from(val));
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(v, c);
        func.layout.remove_inst(i);
        n += 1;
    }
    if n == 0 {
        return 0;
    }
    for &b in &lpi.order {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            match func.dfg.insts[i] {
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    let Some(k) = cx.iconst(func, args[0]) else {
                        continue;
                    };
                    let r = func.dfg.first_result(i);
                    let pick = if k != 0 { args[1] } else { args[2] };
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(r, pick);
                    func.layout.remove_inst(i);
                }
                InstructionData::Brif {
                    arg, blocks: bs, ..
                } => {
                    let Some(k) = cx.iconst(func, arg) else {
                        continue;
                    };
                    let dest = if k != 0 { bs[0] } else { bs[1] };
                    func.dfg.insts[i] = InstructionData::Jump {
                        opcode: Opcode::Jump,
                        destination: dest,
                    };
                }
                _ => {}
            }
        }
    }
    n
}

//! Lockstep loop parameters share one register (`PLIRON_IVMERGE=1`).
//!
//! `a.iter().rev().cmp(b.iter().rev())` over two slices of one length
//! (dragon's `Big32x40::cmp`, `iter::zip` of two equal slices) walks two
//! pointers that step by the same amount. LLVM's IndVarSimplify/LSR keep
//! one of them: `q - p` is loop-invariant, so `q` is `p + D` and its end
//! test `q == Q` is `p == Q - D`, the same test as `p`'s own. After the
//! late steps our loop still carries both (native `big_cmp`: a third
//! param holding `q`'s previous value, `q`'s step behind `select(q ==
//! Q, q, q - 4)`, and both end tests); on wasm `REVCMP` has dropped one
//! test, but `q` and a duplicate of it stay loop-carried.
//!
//! Per loop header, params of one integer type form a class `{r, m..}`
//! when, for each member, `m = r + D_m` with `D_m` a linear form over
//! values from outside the loop:
//!
//! - on every entry edge, `lin(arg_m) - lin(arg_r)` is `D_m` (linear forms
//!   over `iadd`/`isub`/`imul`/`ishl` by constants, through single-source
//!   params; a `select` whose condition the edge decides is its pick:
//!   the edge's own `brif` arm, or the arms above a chain of single-
//!   predecessor blocks);
//! - on every back edge, with every member leaf `m'` replaced by
//!   `r + D_m'`, `lin(arg_m) - lin(arg_r)` is again `D_m`.
//!
//! That is a joint induction: the relations hold on entry and every back
//! edge preserves them, whatever the recurrences are (a duplicate param
//! is the `D = 0` case). Arithmetic is modulo the type's width, so no
//! nowrap premise is needed.
//!
//! The rewrite keeps one member (the root) and drops the others:
//! `icmp eq|ne m, Q` with an invariant `Q` becomes `icmp eq|ne r, Q - D_m`
//! (an existing value when `Q - D_m` is one, else computed before the
//! loop); a load or store addressed by `m` takes `r + (D_m - k)` with the
//! constant `k` moved into its offset (one `iadd`, which aarch64 and x64
//! fold into a register-register address); any other use reads
//! `r + D_m` computed at the top of the header. A rewritten test that a
//! dominating `brif` arm already decides (the root's own end test)
//! folds, with the `brif`s and `select`s on it. The root is the member
//! whose choice leaves the fewest instructions in the loop; a class is
//! taken only when the instructions it adds in the loop (the `iadd`s) are
//! at most the ones that die with the dropped params' steps.
//!
//! Only `eq`/`ne` compares move: an ordered compare is not
//! offset-invariant.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value, ValueDef,
    types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::pipeline::{Analysis, CfgDtLa};
use crate::uses::Uses;
use crate::vmap::VMap;
use crate::zipiv::Lin;

const MAX_LOOPS: usize = 64;
const MAX_PARAMS: usize = 32;
const MAX_DEPTH: usize = 8;
const MAX_CLASS: usize = 8;
const MAX_UP: usize = 8;
const MAX_TERMS: usize = 4;
const MAX_CONE: usize = 256;
const MAX_APPLY: usize = 16;

fn mask(x: i64, bits: u32) -> i64 {
    if bits >= 64 { x } else { x & ((1i64 << bits) - 1) }
}

fn sext(x: i64, bits: u32) -> i64 {
    if bits >= 64 {
        x
    } else {
        let s = 64 - bits;
        (x << s) >> s
    }
}

/// The address-mode facts the rewrite and its cost rule need.
#[derive(Clone, Copy)]
pub struct Amode {
    /// `[base + index]` exists (aarch64, x64; not wasm).
    pub reg_reg: bool,
    /// `[base + index + disp]` exists (x64).
    pub reg_reg_disp: bool,
    /// A load/store offset may be negative (not wasm).
    pub neg_off: bool,
}

impl Amode {
    pub fn of(t: &crate::target::Target) -> Amode {
        Amode {
            reg_reg: t.amode_scale(),
            reg_reg_disp: t.is_x64(),
            neg_off: t.neg_mem_offset(),
        }
    }
}

struct Cx<'a> {
    ps: &'a VMap<Value>,
    none: FxHashSet<Value>,
}

impl Cx<'_> {
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

    /// Linear form of `v`; a `select` whose condition is in `known` is
    /// its pick.
    fn lin(&self, func: &Function, v: Value, d: usize, known: &[(Value, bool)]) -> Lin {
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
        let l = |x: Value| self.lin(func, x, d - 1, known);
        match func.dfg.insts[i] {
            InstructionData::Binary { opcode, args } => match opcode {
                Opcode::Iadd => l(args[0]).add(&l(args[1]), 1),
                Opcode::Isub => l(args[0]).add(&l(args[1]), -1),
                Opcode::Imul => match (self.iconst(func, args[0]), self.iconst(func, args[1])) {
                    (_, Some(k)) => l(args[0]).scale(k),
                    (Some(k), None) => l(args[1]).scale(k),
                    _ => Lin::leaf(v),
                },
                // CLIF masks the shift amount by the width: only an amount
                // below it is a plain multiply.
                Opcode::Ishl => match self.iconst(func, args[1]) {
                    Some(k)
                        if (0..63).contains(&k)
                            && (k as u32) < func.dfg.value_type(v).bits() =>
                    {
                        l(args[0]).scale(1i64 << k)
                    }
                    _ => Lin::leaf(v),
                },
                _ => Lin::leaf(v),
            },
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } => {
                let c = self.canon(func, args[0]);
                match known.iter().find(|k| k.0 == c) {
                    Some(&(_, t)) => l(if t { args[1] } else { args[2] }),
                    None => Lin::leaf(v),
                }
            }
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

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// `brif` conditions decided on the way into `dst` through `term` (in
/// `src`): that branch's arm, then the arms above a chain of
/// single-predecessor blocks.
fn known_on(
    func: &Function,
    cfg: &ControlFlowGraph,
    cx: &Cx,
    src: Block,
    term: Inst,
    dst: Block,
) -> Vec<(Value, bool)> {
    let mut out = Vec::new();
    let arm = |out: &mut Vec<(Value, bool)>, t: Inst, to: Block| {
        if let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[t] {
            let a = blocks[0].block(&func.dfg.value_lists);
            let b = blocks[1].block(&func.dfg.value_lists);
            if a != b {
                out.push((cx.canon(func, arg), to == a));
            }
        }
    };
    arm(&mut out, term, dst);
    let mut x = src;
    let mut seen = vec![src];
    for _ in 0..MAX_UP {
        let mut it = cfg.pred_iter(x);
        let (Some(p), None) = (it.next(), it.next()) else {
            break;
        };
        if seen.contains(&p.block) {
            break;
        }
        seen.push(p.block);
        arm(&mut out, p.inst, x);
        x = p.block;
    }
    out
}

struct Edge {
    inst: Inst,
    /// Resolved args to the header.
    args: Vec<Value>,
    back: bool,
    known: Vec<(Value, bool)>,
}

fn edge_args(func: &Function, inst: Inst, dst: Block) -> Option<Vec<Value>> {
    let mut out = None;
    for bc in
        func.dfg.insts[inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
    {
        if bc.block(&func.dfg.value_lists) != dst {
            continue;
        }
        if out.is_some() {
            return None;
        }
        let mut vs = Vec::new();
        for a in bc.args(&func.dfg.value_lists) {
            match a {
                BlockArg::Value(v) => vs.push(func.dfg.resolve_aliases(v)),
                _ => return None,
            }
        }
        out = Some(vs);
    }
    out
}

/// How one use of a dropped member is rewritten.
#[derive(Clone)]
enum Use {
    /// `icmp eq|ne` against an invariant: operand index of the member,
    /// the image `Q - D` (an existing value, a constant, or computed
    /// from `Q` before the loop).
    Cmp(Inst, usize, Image),
    /// Load/store address: new offset (the root plus `D`'s leaves is the
    /// address).
    Mem(Inst, i32),
}

#[derive(Clone)]
enum Image {
    Val(Value),
    Const(i64),
    Sub(Value),
}

struct Member {
    idx: usize,
    /// `param = root + d`.
    d: Lin,
    uses: Vec<Use>,
}

struct ClassPlan {
    root: usize,
    members: Vec<Member>,
}

struct LoopPlan {
    h: Block,
    ins: Block,
    ty_bits: Vec<u32>,
    classes: Vec<ClassPlan>,
}

struct Ctx<'a, 'b> {
    func: &'a Function,
    cfg: &'a ControlFlowGraph,
    dt: &'a DominatorTree,
    la: &'a LoopAnalysis,
    cx: &'a Cx<'b>,
    uses: &'a Uses,
    am: Amode,
}

impl Ctx<'_, '_> {
    /// `l`'s leaves all dominate `ins` (so they are loop-invariant), have
    /// type `ty`, and it is materializable cheaply (unit coefficients).
    fn invariant(&self, l: &Lin, ins: Block, ty: Type) -> bool {
        let bits = ty.bits();
        l.t.len() <= MAX_TERMS
            && l.t.iter().all(|&(v, c)| {
                (mask(c, bits) == mask(1, bits) || mask(c, bits) == mask(-1, bits))
                    && self.func.dfg.value_type(v) == ty
                    && def_block(self.func, v).is_some_and(|db| self.dt.block_dominates(db, ins))
            })
    }

    /// In-loop pure instructions that die once the `drop`ped params' edge
    /// args go.
    fn cone(&self, lp: Loop, edges: &[Edge], drop: &[bool]) -> FxHashSet<Inst> {
        let func = self.func;
        let mut left: FxHashMap<Value, u32> = FxHashMap::default();
        let mut dead: FxHashSet<Inst> = FxHashSet::default();
        let mut work: Vec<Value> = Vec::new();
        let dec = |left: &mut FxHashMap<Value, u32>, work: &mut Vec<Value>, v: Value| {
            let e = left
                .entry(v)
                .or_insert_with(|| self.uses.sites(v).len() as u32);
            *e = e.saturating_sub(1);
            if *e == 0 {
                work.push(v);
            }
        };
        for e in edges {
            for (i, &a) in e.args.iter().enumerate() {
                if drop[i] {
                    dec(&mut left, &mut work, a);
                }
            }
        }
        let mut budget = MAX_CONE;
        while let Some(v) = work.pop() {
            let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
                continue;
            };
            if dead.contains(&i)
                || !crate::jumpthread::pure_op(func, i)
                || func.layout.inst_block(i).is_none_or(|b| !self.la.is_in_loop(b, lp))
            {
                continue;
            }
            dead.insert(i);
            for &a in func.dfg.inst_args(i) {
                dec(&mut left, &mut work, func.dfg.resolve_aliases(a));
            }
            budget -= 1;
            if budget == 0 {
                break;
            }
        }
        dead
    }

    /// Classify the uses of member `m` (`= root + d`); `None` when one
    /// cannot be rewritten.
    #[allow(clippy::too_many_arguments)]
    fn classify(
        &self,
        lp: Loop,
        h: Block,
        ins: Block,
        edges: &[Edge],
        drop: &[bool],
        dead: &FxHashSet<Inst>,
        m: Value,
        d: &Lin,
    ) -> Option<(Vec<Use>, usize)> {
        let func = self.func;
        let ty = func.dfg.value_type(m);
        let bits = ty.bits();
        let k = sext(d.k, bits);
        let leaves = !d.t.is_empty();
        // `root - leaf` folds into no address mode.
        let sub_only = matches!(d.t[..], [(_, c)] if mask(c, bits) != mask(1, bits));
        let mut seen: FxHashSet<Inst> = FxHashSet::default();
        let mut out = Vec::new();
        let mut general = false;
        let mut cost = 0usize;
        for &(_, ui) in self.uses.sites(m) {
            if !seen.insert(ui) || dead.contains(&ui) {
                continue;
            }
            let occ = func
                .dfg
                .inst_values(ui)
                .filter(|&v| func.dfg.resolve_aliases(v) == m)
                .count();
            if let Some(e) = edges.iter().find(|e| e.inst == ui) {
                let gone = e
                    .args
                    .iter()
                    .enumerate()
                    .filter(|&(i, &a)| drop[i] && a == m)
                    .count();
                if occ > gone {
                    general = true;
                }
                continue;
            }
            match func.dfg.insts[ui] {
                InstructionData::IntCompare { cond, args, .. }
                    if matches!(cond, IntCC::Equal | IntCC::NotEqual) && occ == 1 =>
                {
                    let mi = usize::from(func.dfg.resolve_aliases(args[1]) == m);
                    let q = func.dfg.resolve_aliases(args[1 - mi]);
                    let qb = def_block(func, q)?;
                    if self.la.is_in_loop(qb, lp) || !self.dt.block_dominates(qb, ins) {
                        general = true;
                        continue;
                    }
                    let img = self.cx.lin(func, q, MAX_DEPTH, &[]).add(d, -1);
                    let image = if let Some(c) = img.as_const(bits) {
                        Image::Const(c)
                    } else if let [(pv, c)] = img.t[..]
                        && mask(c, bits) == mask(1, bits)
                        && mask(img.k, bits) == 0
                        && func.dfg.value_type(pv) == ty
                        && def_block(func, pv).is_some_and(|db| self.dt.block_dominates(db, h))
                    {
                        Image::Val(pv)
                    } else {
                        Image::Sub(q)
                    };
                    out.push(Use::Cmp(ui, mi, image));
                }
                InstructionData::Load { arg, offset, .. }
                    if func.dfg.resolve_aliases(arg) == m && occ == 1 =>
                {
                    let off = i64::from(i32::from(offset)).wrapping_add(k);
                    match i32::try_from(off) {
                        Ok(o) if o >= 0 || self.am.neg_off => {
                            cost += self.mem_cost(leaves, sub_only, o);
                            out.push(Use::Mem(ui, o));
                        }
                        _ => general = true,
                    }
                }
                InstructionData::Store { args, offset, .. }
                    if func.dfg.resolve_aliases(args[1]) == m && occ == 1 =>
                {
                    let off = i64::from(i32::from(offset)).wrapping_add(k);
                    match i32::try_from(off) {
                        Ok(o) if o >= 0 || self.am.neg_off => {
                            cost += self.mem_cost(leaves, sub_only, o);
                            out.push(Use::Mem(ui, o));
                        }
                        _ => general = true,
                    }
                }
                _ => general = true,
            }
        }
        if general {
            cost += usize::from(leaves) + usize::from(k != 0);
        }
        Some((out, cost))
    }

    /// In-loop instructions a rewritten address costs.
    fn mem_cost(&self, leaves: bool, sub_only: bool, off: i32) -> usize {
        if !leaves || (!sub_only && (self.am.reg_reg_disp || (self.am.reg_reg && off == 0))) {
            0
        } else {
            1
        }
    }

    fn plan_loop(&self, lp: Loop) -> Option<LoopPlan> {
        let func = self.func;
        let h = self.la.loop_header(lp);
        let params: Vec<Value> = func.dfg.block_params(h).to_vec();
        if params.len() < 2 || params.len() > MAX_PARAMS {
            return None;
        }
        let mut edges = Vec::new();
        for p in self.cfg.pred_iter(h) {
            let args = edge_args(func, p.inst, h)?;
            if args.len() != params.len() {
                return None;
            }
            edges.push(Edge {
                inst: p.inst,
                args,
                back: self.la.is_in_loop(p.block, lp),
                known: known_on(func, self.cfg, self.cx, p.block, p.inst, h),
            });
        }
        let entries: Vec<Block> = self
            .cfg
            .pred_iter(h)
            .filter(|p| !self.la.is_in_loop(p.block, lp))
            .map(|p| p.block)
            .collect();
        if entries.is_empty() || !edges.iter().any(|e| e.back) {
            return None;
        }
        // Where the differences are computed: the single entry block, or
        // the header's immediate dominator (which dominates every entry).
        let ins = if entries.len() == 1 {
            entries[0]
        } else {
            self.dt.idom(h)?
        };
        if self.la.is_in_loop(ins, lp) {
            return None;
        }
        let lins: Vec<Vec<Option<Lin>>> = edges
            .iter()
            .map(|e| {
                params
                    .iter()
                    .enumerate()
                    .map(|(i, &pv)| {
                        let ty = func.dfg.value_type(pv);
                        (ty.is_int() && ty.bits() <= 64)
                            .then(|| self.cx.lin(func, e.args[i], MAX_DEPTH, &e.known))
                    })
                    .collect()
            })
            .collect();
        let mut assigned = vec![false; params.len()];
        let mut classes = Vec::new();
        for r in 0..params.len() {
            if assigned[r] || lins[0][r].is_none() {
                continue;
            }
            let ty = func.dfg.value_type(params[r]);
            let bits = ty.bits();
            // Hypotheses from the entry edges.
            let mut cls: Vec<(usize, Lin)> = vec![(r, Lin::konst(0))];
            for m in r + 1..params.len() {
                if cls.len() >= MAX_CLASS {
                    break;
                }
                if assigned[m] || func.dfg.value_type(params[m]) != ty {
                    continue;
                }
                let mut dd: Option<Lin> = None;
                let mut ok = true;
                for (ei, e) in edges.iter().enumerate() {
                    if e.back {
                        continue;
                    }
                    let (Some(a), Some(b)) = (&lins[ei][m], &lins[ei][r]) else {
                        ok = false;
                        break;
                    };
                    let d = a.add(b, -1);
                    match &dd {
                        None => dd = Some(d),
                        Some(x) if x.eq_mod(&d, bits) => {}
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if let (true, Some(d)) = (ok, dd)
                    && self.invariant(&d, ins, ty)
                {
                    cls.push((m, d));
                }
            }
            // The back edges preserve the relations (joint induction);
            // drop failures until stable.
            for _ in 0..MAX_CLASS {
                let sub = |l: &Lin| -> Lin {
                    let mut out = Lin::konst(l.k);
                    for &(v, c) in &l.t {
                        match cls.iter().find(|(mi, _)| params[*mi] == v) {
                            Some((_, d)) => {
                                out = out.add(&Lin::leaf(params[r]).add(d, 1).scale(c), 1);
                            }
                            None => out = out.add(&Lin::leaf(v).scale(c), 1),
                        }
                    }
                    out
                };
                let fail: Vec<usize> = cls
                    .iter()
                    .skip(1)
                    .filter(|(m, d)| {
                        edges.iter().enumerate().filter(|(_, e)| e.back).any(|(ei, _)| {
                            let (Some(a), Some(b)) = (&lins[ei][*m], &lins[ei][r]) else {
                                return true;
                            };
                            !sub(a).add(&sub(b), -1).eq_mod(d, bits)
                        })
                    })
                    .map(|(m, _)| *m)
                    .collect();
                if fail.is_empty() {
                    break;
                }
                cls.retain(|(m, _)| !fail.contains(m));
            }
            if cls.len() < 2 {
                continue;
            }
            // A failure on the last round leaves the class unproven.
            let proven = cls.iter().skip(1).all(|(m, d)| {
                edges.iter().enumerate().filter(|(_, e)| e.back).all(|(ei, _)| {
                    let (Some(a), Some(b)) = (&lins[ei][*m], &lins[ei][r]) else {
                        return false;
                    };
                    let sub = |l: &Lin| -> Lin {
                        let mut out = Lin::konst(l.k);
                        for &(v, c) in &l.t {
                            match cls.iter().find(|(mi, _)| params[*mi] == v) {
                                Some((_, dv)) => {
                                    out = out.add(&Lin::leaf(params[r]).add(dv, 1).scale(c), 1)
                                }
                                None => out = out.add(&Lin::leaf(v).scale(c), 1),
                            }
                        }
                        out
                    };
                    sub(a).add(&sub(b), -1).eq_mod(d, bits)
                })
            });
            if !proven {
                continue;
            }
            for (m, _) in &cls {
                assigned[*m] = true;
            }
            // Choose the root: fewest instructions left in the loop. When
            // no root pays for the whole class, keep only the members
            // equal to the root (`d = 0`), which cost nothing.
            let eval = |rho: usize, drho: &Lin, dups_only: bool| -> Option<(isize, ClassPlan)> {
                let pick: Vec<(usize, Lin)> = cls
                    .iter()
                    .filter(|(m, _)| *m != rho)
                    .map(|(m, dm)| (*m, dm.add(drho, -1)))
                    .filter(|(_, d)| !dups_only || d.as_const(bits) == Some(0))
                    .collect();
                if pick.is_empty() {
                    return None;
                }
                let mut drop = vec![false; params.len()];
                for (m, _) in &pick {
                    drop[*m] = true;
                }
                let dead = self.cone(lp, &edges, &drop);
                let removed = dead
                    .iter()
                    .filter(|&&i| func.dfg.insts[i].opcode() != Opcode::Iconst)
                    .count();
                let mut members = Vec::new();
                let mut cost = 0usize;
                for (m, d) in pick {
                    if !self.invariant(&d, ins, ty) {
                        return None;
                    }
                    let (uses, c) =
                        self.classify(lp, h, ins, &edges, &drop, &dead, params[m], &d)?;
                    cost += c;
                    members.push(Member { idx: m, d, uses });
                }
                (cost <= removed)
                    .then(|| (removed as isize - cost as isize, ClassPlan { root: rho, members }))
            };
            let mut best: Option<(isize, ClassPlan)> = None;
            for dups_only in [false, true] {
                for (rho, drho) in &cls {
                    if let Some((net, cp)) = eval(*rho, drho, dups_only) {
                        let better = best.as_ref().is_none_or(|(b, bp)| {
                            (cp.members.len(), net) > (bp.members.len(), *b)
                        });
                        if better {
                            best = Some((net, cp));
                        }
                    }
                }
                if best.is_some() {
                    break;
                }
            }
            if let Some((_, cp)) = best {
                classes.push(cp);
            }
        }
        if classes.is_empty() {
            return None;
        }
        Some(LoopPlan {
            h,
            ins,
            ty_bits: params.iter().map(|&p| func.dfg.value_type(p).bits()).collect(),
            classes,
        })
    }
}

/// `d`'s non-constant part as the rewrite applies it: one leaf added or
/// subtracted where it is used, or a sum computed before the loop.
#[derive(Clone, Copy)]
enum Leaves {
    None,
    Add(Value),
    Sub(Value),
}

impl Leaves {
    /// `x + leaves`.
    fn plus(self, cur: &mut FuncCursor, x: Value) -> Value {
        match self {
            Leaves::None => x,
            Leaves::Add(v) => cur.ins().iadd(x, v),
            Leaves::Sub(v) => cur.ins().isub(x, v),
        }
    }

    /// `x - leaves`.
    fn minus(self, cur: &mut FuncCursor, x: Value) -> Value {
        match self {
            Leaves::None => x,
            Leaves::Add(v) => cur.ins().isub(x, v),
            Leaves::Sub(v) => cur.ins().iadd(x, v),
        }
    }
}

/// `d`'s leaves: a lone leaf as is, a sum (unit coefficients) computed
/// before `at`.
fn emit_leaves(func: &mut Function, at: Inst, l: &Lin, ty: Type) -> Leaves {
    let bits = ty.bits();
    let pos: Vec<Value> = l
        .t
        .iter()
        .filter(|&&(_, c)| mask(c, bits) == mask(1, bits))
        .map(|&(v, _)| v)
        .collect();
    let neg: Vec<Value> = l
        .t
        .iter()
        .filter(|&&(_, c)| mask(c, bits) != mask(1, bits))
        .map(|&(v, _)| v)
        .collect();
    match (&pos[..], &neg[..]) {
        ([], []) => return Leaves::None,
        ([p], []) => return Leaves::Add(*p),
        ([], [n]) => return Leaves::Sub(*n),
        _ => {}
    }
    let mut cur = FuncCursor::new(func).at_inst(at);
    let mut acc = match (pos.first(), neg.first()) {
        (Some(&p), _) => p,
        (None, Some(&n)) => cur.ins().ineg(n),
        (None, None) => unreachable!("two or more leaves"),
    };
    for &p in pos.iter().skip(1) {
        acc = cur.ins().iadd(acc, p);
    }
    for &n in neg.iter().skip(usize::from(pos.is_empty())) {
        acc = cur.ins().isub(acc, n);
    }
    Leaves::Add(acc)
}

/// Apply one header's plan; the rewritten compares go to `cmps`.
fn apply(func: &mut Function, edges_of: &[Inst], plan: &LoopPlan, cmps: &mut Vec<Inst>) -> usize {
    let h = plan.h;
    let at = func.layout.last_inst(plan.ins).expect("entry block has a terminator");
    let params: Vec<Value> = func.dfg.block_params(h).to_vec();
    let mut drop: Vec<usize> = Vec::new();
    // (member, root, leaves of `d` computed before the loop, constant)
    let mut alias: Vec<(Value, Value, Leaves, i64)> = Vec::new();
    for cp in &plan.classes {
        let root = params[cp.root];
        for m in &cp.members {
            let ty = func.dfg.value_type(params[m.idx]);
            let bits = plan.ty_bits[m.idx];
            let k = sext(m.d.k, bits);
            // Unused copies go with the final sweep.
            let leaves = emit_leaves(func, at, &m.d, ty);
            for u in &m.uses {
                match *u {
                    Use::Cmp(ui, mi, ref img) => {
                        let pv = match *img {
                            Image::Val(v) => v,
                            Image::Const(c) => {
                                FuncCursor::new(func).at_inst(at).ins().iconst(ty, c)
                            }
                            Image::Sub(q) => {
                                let mut cur = FuncCursor::new(func).at_inst(at);
                                let mut x = leaves.minus(&mut cur, q);
                                if k != 0 {
                                    let kc = cur.ins().iconst(ty, k.wrapping_neg());
                                    x = cur.ins().iadd(x, kc);
                                }
                                x
                            }
                        };
                        if let InstructionData::IntCompare { args, .. } = &mut func.dfg.insts[ui] {
                            args[mi] = root;
                            args[1 - mi] = pv;
                        }
                        cmps.push(ui);
                    }
                    Use::Mem(ui, off) => {
                        let addr = leaves.plus(&mut FuncCursor::new(func).at_inst(ui), root);
                        match &mut func.dfg.insts[ui] {
                            InstructionData::Load { arg, offset, .. } => {
                                *arg = addr;
                                *offset = Offset32::new(off);
                            }
                            InstructionData::Store { args, offset, .. } => {
                                args[1] = addr;
                                *offset = Offset32::new(off);
                            }
                            _ => unreachable!("Use::Mem on a load or store"),
                        }
                    }
                }
            }
            drop.push(m.idx);
            alias.push((params[m.idx], root, leaves, k));
        }
    }
    drop.sort_unstable_by(|a, b| b.cmp(a));
    for &inst in edges_of {
        let dfg = &mut func.dfg;
        for call in
            dfg.insts[inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
        {
            if call.block(&dfg.value_lists) == h {
                for &i in &drop {
                    call.remove(i, &mut dfg.value_lists);
                }
            }
        }
    }
    for &i in &drop {
        func.dfg.remove_block_param(params[i]);
    }
    // Every other use reads `root + d` from the top of the header (unused
    // copies go with the final sweep).
    let top = func.layout.first_inst(h).expect("header has a terminator");
    for (m, root, leaves, k) in alias {
        let ty = func.dfg.value_type(m);
        let mut cur = FuncCursor::new(func).at_inst(top);
        let mut v = leaves.plus(&mut cur, root);
        if k != 0 {
            let kc = cur.ins().iconst(ty, k);
            v = cur.ins().iadd(v, kc);
        }
        func.dfg.change_to_alias(m, v);
    }
    drop.len()
}

/// Fold the rewritten compares a dominating `brif` arm decides, then the
/// `brif`s and `select`s on them. Returns compares folded and whether a
/// `brif` became a `jump`.
fn fold_known(func: &mut Function, cfg: &ControlFlowGraph, cx: &Cx, cmps: &[Inst]) -> (usize, bool) {
    let mut known: Vec<(Inst, bool)> = Vec::new();
    for &i in cmps {
        let Some(b) = func.layout.inst_block(i) else {
            continue;
        };
        let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] else {
            continue;
        };
        let k = key(cx.canon(func, args[0]), cx.canon(func, args[1]));
        let mut x = b;
        let mut seen = vec![b];
        for _ in 0..MAX_UP {
            let mut it = cfg.pred_iter(x);
            let (Some(p), None) = (it.next(), it.next()) else {
                break;
            };
            if let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst] {
                let t = blocks[0].block(&func.dfg.value_lists);
                let f = blocks[1].block(&func.dfg.value_lists);
                if t != f
                    && let Some((a0, a1, is_eq)) = cx.eqne(func, arg)
                    && key(a0, a1) == k
                {
                    let eq = (x == t) == is_eq;
                    known.push((i, if cond == IntCC::Equal { eq } else { !eq }));
                    break;
                }
            }
            if seen.contains(&p.block) {
                break;
            }
            seen.push(p.block);
            x = p.block;
        }
    }
    let mut consts: FxHashSet<Value> = FxHashSet::default();
    for &(i, val) in &known {
        let v = func.dfg.first_result(i);
        let mut cur = FuncCursor::new(func).at_inst(i);
        let c = cur.ins().iconst(types::I8, i64::from(val));
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(v, c);
        func.layout.remove_inst(i);
        consts.insert(c);
    }
    if consts.is_empty() {
        return (0, false);
    }
    let pick = |func: &Function, c: Value| -> Option<bool> {
        let c = func.dfg.resolve_aliases(c);
        if !consts.contains(&c) {
            return None;
        }
        let i = func.dfg.value_def(c).inst()?;
        match func.dfg.insts[i] {
            InstructionData::UnaryImm { imm, .. } => Some(imm.bits() != 0),
            _ => None,
        }
    };
    let mut jumped = false;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for b in blocks {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            match func.dfg.insts[i] {
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    let Some(t) = pick(func, args[0]) else { continue };
                    let r = func.dfg.first_result(i);
                    let v = if t { args[1] } else { args[2] };
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(r, v);
                    func.layout.remove_inst(i);
                }
                InstructionData::Brif { arg, blocks: bs, .. } => {
                    let Some(t) = pick(func, arg) else { continue };
                    let dest = if t { bs[0] } else { bs[1] };
                    func.dfg.insts[i] = InstructionData::Jump {
                        opcode: Opcode::Jump,
                        destination: dest,
                    };
                    jumped = true;
                }
                _ => {}
            }
        }
    }
    (known.len(), jumped)
}

/// Run over every loop: params dropped plus compares folded.
pub fn run(func: &mut Function, an: &mut Analysis, am: Amode) -> usize {
    // The step's gate, read here too so direct callers (tests) honor it.
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_IVMERGE", "1"));
    if !*ON {
        return 0;
    }
    let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
    let loops: Vec<Loop> = la.loops().take(MAX_LOOPS).collect();
    // Candidates need two params of one integer type on a header.
    let two = |h: Block| {
        let ps = func.dfg.block_params(h);
        ps.len() <= MAX_PARAMS
            && ps.iter().enumerate().any(|(i, &a)| {
                let t = func.dfg.value_type(a);
                t.is_int() && ps[i + 1..].iter().any(|&b| func.dfg.value_type(b) == t)
            })
    };
    if !loops.iter().any(|&lp| two(la.loop_header(lp))) {
        return 0;
    }
    let mut n = 0;
    let mut applied = 0;
    let mut cmps: Vec<Inst> = Vec::new();
    let mut ps = crate::proots::alias_map_cfg(func, cfg);
    let mut uses = Uses::new(func);
    for lp in loops {
        if applied >= MAX_APPLY {
            break;
        }
        let plan = {
            let cx = Cx {
                ps: &ps,
                none: FxHashSet::default(),
            };
            let ctx = Ctx {
                func,
                cfg,
                dt,
                la,
                cx: &cx,
                uses: &uses,
                am,
            };
            ctx.plan_loop(lp)
        };
        let Some(plan) = plan else { continue };
        let edges_of: Vec<Inst> = cfg.pred_iter(plan.h).map(|p| p.inst).collect();
        n += apply(func, &edges_of, &plan, &mut cmps);
        applied += 1;
        // The rewrite moved uses and removed params: re-index.
        ps = crate::proots::alias_map_cfg(func, cfg);
        uses = Uses::new(func);
    }
    if n == 0 {
        return 0;
    }
    let cx = Cx {
        ps: &ps,
        none: FxHashSet::default(),
    };
    let (folded, jumped) = fold_known(func, cfg, &cx, &cmps);
    if jumped {
        crate::jumpthread::remove_unreachable_blocks(func);
    }
    crate::edgespec::sweep_dead(func);
    n + folded
}
